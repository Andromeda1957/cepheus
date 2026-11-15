//! `cepheus peer`: this host's deck as a network service for the hub. A process of its own beside the service,
//! as the phone gateway is, started by the service when `peer_listen` is set. Addresses outside `peer_allow`
//! are dropped before TLS, a hub proves itself with the key whose SHA-256 `hosts install` left here, and an
//! address that keeps failing is locked out.

use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rustls::ServerConfig;

use crate::config::Config;
use crate::hosts::logins::{self, LoginPaths};
use crate::hosts::wire::{Channel, Hello, HostReq, HostResp, ProfileLogin, ToolLogin, Welcome, PEER_VERSION};
use crate::hosts::{self, mirror, tls, Cidr};
use crate::protocol::{read_msg, write_msg, HostStats, PROTOCOL_VERSION};
use crate::remote::auth::{self, Limiter};
use crate::remote::{self, chat, search, store, uploads};
use crate::resources::Sampler;
use crate::{client, gitwt, handover, paths, util};
#[path = "phone_forward.rs"]
mod phone_forward;

/// The hub's key hash, beside the certificate in the peer's directory.
const HUB_KEY_HASH: &str = "hub.sha256";
const LOCK: &str = "peer.lock";
/// Connections at once: a main channel, a host channel and one per deck or
/// phone using the host, with room to spare.
const MAX_CONNS: usize = 32;
/// How long a stats reading of the repository stays fresh.
const GIT_FRESH: Duration = Duration::from_secs(5);

pub fn peer_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("peer")
}

/// When the host's branch status, `HEAD` and branches were last read, and
/// what they were.
type GitReading = (Instant, Option<gitwt::BranchStatus>, Option<String>, Vec<crate::protocol::BranchInfo>);

pub struct Peer {
    dir: PathBuf,
    allow: Vec<Cidr>,
    limiter: Mutex<Limiter>,
    conns: AtomicUsize,
    name: String,
    start_dir: PathBuf,
    /// The service socket a deck channel pipes to; none: this user's own,
    /// starting the service when it is not running.
    socket: Option<PathBuf>,
    config_file: PathBuf,
    sampler: Mutex<Sampler>,
    git: Mutex<Option<GitReading>>,
    /// The program file a hub's newer build replaces.
    program: PathBuf,
    /// Where this host's Claude and Codex keep their sign-ins.
    logins: LoginPaths,
    scans_dir: PathBuf,
    #[cfg(test)]
    test_config: Option<Config>,
}

impl Peer {
    pub fn new(dir: PathBuf, allow: Vec<Cidr>, name: String, start_dir: PathBuf, socket: Option<PathBuf>) -> Peer {
        Peer {
            dir,
            allow,
            limiter: Mutex::new(Limiter::default()),
            conns: AtomicUsize::new(0),
            name,
            start_dir,
            socket,
            config_file: paths::config_path(),
            sampler: Mutex::new(Sampler::default()),
            git: Mutex::new(None),
            program: crate::service::current_exe_clean(),
            logins: LoginPaths::from_env(),
            scans_dir: Config::load().0.scans_dir(),
            #[cfg(test)]
            test_config: None,
        }
    }

    /// Keep sign-ins in `logins` rather than this user's own.
    #[cfg(test)]
    pub fn with_logins(mut self, logins: LoginPaths) -> Peer {
        self.logins = logins;
        self
    }

    #[cfg(test)]
    pub fn with_scans_dir(mut self, scans_dir: PathBuf) -> Peer {
        self.scans_dir = scans_dir;
        self
    }

    #[cfg(test)]
    pub fn with_config(mut self, config: Config) -> Peer {
        self.test_config = Some(config);
        self
    }

    #[cfg(test)]
    pub fn with_config_file(mut self, config_file: PathBuf) -> Peer {
        self.config_file = config_file;
        self
    }

    /// Files the phone attached to this host's sessions through the hub.
    fn uploads_dir(&self) -> PathBuf {
        self.dir.join("uploads")
    }

    /// Where this host's programs keep their logs, for the phone's chat.
    fn chat_dirs(&self) -> chat::Dirs {
        chat::Dirs {
            claude: self.logins.claude_dir.clone(),
            codex: self.logins.codex_dir.clone(),
            opencode_db: paths::opencode_db(),
            antigravity: paths::antigravity_dir(),
            start_dir: self.start_dir.clone(),
            sqlite3: vec!["sqlite3".into()],
        }
    }
}

/// Enough independently keyed interfaces for an enrolled fleet, with a hard
/// bound on both authorization work and the private digest file.
const MAX_HUB_KEYS: usize = 64;
const MAX_HUB_KEY_BYTES: u64 = (MAX_HUB_KEYS * 65) as u64;

/// Record `key` as a hub this host serves, by its SHA-256 only, one per
/// line; the oldest goes once there are more than `MAX_HUB_KEYS`.
pub fn record_hub_key(dir: &Path, key: &str) -> Result<()> {
    record_hub_key_hash(dir, &auth::sha256_hex(key.as_bytes()))
}

/// A trusted enrollment channel may supply a public digest instead of a key.
pub fn record_hub_key_hash(dir: &Path, hash: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("viewer identity must be a SHA-256 digest");
    }
    util::ensure_private_dir(dir)?;
    // Serialize the bounded read/modify/write across peer threads and installer
    // processes so concurrent interfaces cannot lose another enrollment.
    let lock =
        fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join("hub-key.lock"))?;
    // SAFETY: flock uses a live descriptor owned by this call.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error()).context("locking viewer enrollment");
    }
    let hash = hash.to_ascii_lowercase();
    let mut hashes: Vec<String> = hub_key_hashes(dir)?.into_iter().filter(|h| *h != hash).collect();
    hashes.push(hash);
    let keep = hashes.len().saturating_sub(MAX_HUB_KEYS);
    util::write_atomic(&dir.join(HUB_KEY_HASH), format!("{}\n", hashes[keep..].join("\n")).as_bytes())?;
    Ok(())
}

fn hub_key_hashes(dir: &Path) -> Result<Vec<String>> {
    let file = match fs::File::open(dir.join(HUB_KEY_HASH)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading authorized viewer identities"),
    };
    let mut bytes = Vec::new();
    file.take(MAX_HUB_KEY_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_HUB_KEY_BYTES {
        bail!("authorized viewer identity file exceeds its limit");
    }
    let text = std::str::from_utf8(&bytes).context("invalid authorized viewer identity file")?;
    let hashes: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .collect();
    if hashes.len() > MAX_HUB_KEYS {
        bail!("too many authorized viewer identities");
    }
    Ok(hashes)
}

pub fn run(args: &[String]) -> i32 {
    let result = match args.first().map(String::as_str) {
        None => serve(),
        Some("authorize") => authorize(),
        Some("fingerprint") => fingerprint(),
        Some("setup") => hosts::setup::run(&args[1..]),
        Some(other) => Err(anyhow::anyhow!("unknown peer command {other:?}")),
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("cepheus peer: {err:#}");
            1
        }
    }
}

/// Record the hub key read from standard input, by its SHA-256 only.
fn authorize() -> Result<()> {
    let mut key = String::new();
    io::stdin().read_to_string(&mut key)?;
    let key = key.trim();
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("expected the hub key (64 hex digits) on standard input");
    }
    record_hub_key(&peer_dir(&paths::state_dir()), key)?;
    println!("hub key recorded");
    Ok(())
}

/// Print the certificate's SHA-256, making the certificate on first use.
fn fingerprint() -> Result<()> {
    let dir = peer_dir(&paths::state_dir());
    let identity = store::load_or_create_identity(&dir, &util::hostname())?;
    println!("{}", identity.fingerprint);
    Ok(())
}

fn serve() -> Result<()> {
    let (config, err) = Config::load();
    if let Some(err) = err {
        bail!("config: {err}");
    }
    let listen =
        client::coordinator::listener(&config).context("no valid native peer listener is configured")?.to_string();
    let allowed_sources = client::coordinator::allowed_sources(&config);
    let dir = peer_dir(&paths::state_dir());
    let Some(_lock) = take_lock(&dir)? else {
        eprintln!("cepheus peer: another peer listener is running");
        return Ok(());
    };
    let identity = store::load_or_create_identity(&dir, &util::hostname())?;
    eprintln!("cepheus peer: certificate SHA-256 {}", identity.fingerprint);
    let tls = remote::server::tls_config(&identity.cert_der, &identity.key_der)?;
    let start_dir = config.resolve_start_dir().unwrap_or_else(|_| paths::home());
    let allow = hosts::parse_cidrs(&allowed_sources);
    let peer = Arc::new(Peer::new(dir, allow, config.local_host_label(), start_dir, None));
    if peer.allow.is_empty() {
        bail!("\"peer_allow\" names no address a hub may connect from");
    }
    watch_exit(listen.clone(), allowed_sources);
    let mut reported = false;
    loop {
        match TcpListener::bind(&listen) {
            Ok(listener) => {
                eprintln!("cepheus peer: listening on {listen}");
                accept_all(&peer, &tls, listener);
            }
            Err(err) => {
                if !reported {
                    eprintln!("cepheus peer: cannot listen on {listen} yet: {err}");
                    reported = true;
                }
                thread::sleep(Duration::from_secs(10));
            }
        }
    }
}

fn take_lock(dir: &Path) -> io::Result<Option<fs::File>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    util::ensure_private_dir(dir)?;
    let file = fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join(LOCK))?;
    // SAFETY: flock on a descriptor this process owns.
    let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    Ok(held.then_some(file))
}

/// Whether a peer listener runs, from its lock.
pub fn running(state_dir: &Path) -> bool {
    matches!(take_lock(&peer_dir(state_dir)), Ok(None))
}

/// Exit once this program is replaced or the listener's settings change, so
/// the service starts the current build with the current settings in its
/// place.
fn watch_exit(listen: String, allow: Vec<String>) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(5));
        if handover::replaced_binary().is_some() {
            eprintln!("cepheus peer: the program was replaced; exiting");
            std::process::exit(0);
        }
        let (config, err) = Config::load();
        if let Some(why) = err.is_none().then(|| settings_changed(&config, &listen, &allow)).flatten() {
            eprintln!("cepheus peer: {why}; exiting");
            std::process::exit(0);
        }
    });
}

/// Why a listener started with `listen` and `allow` must give way: the
/// config turned it off or changed what it serves. An address dropped from
/// `peer_allow` would otherwise keep reaching it until a restart.
fn settings_changed(config: &Config, listen: &str, allow: &[String]) -> Option<&'static str> {
    let effective = client::coordinator::listener(config).map(|address| address.to_string());
    if effective.is_none() {
        Some("\"peer_listen\" was turned off")
    } else if effective.as_deref() != Some(listen) {
        Some("\"peer_listen\" changed")
    } else if client::coordinator::allowed_sources(config) != allow {
        Some("\"peer_allow\" changed")
    } else {
        None
    }
}

pub fn accept_all(peer: &Arc<Peer>, tls: &Arc<ServerConfig>, listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Ok(addr) = stream.peer_addr() else { continue };
        let ip = auth::plain_ip(addr.ip());
        let open = peer.limiter.lock().is_ok_and(|mut l| l.open(ip, Instant::now()));
        if !hosts::cidr_allows(&peer.allow, ip) || !open || peer.conns.load(Ordering::SeqCst) >= MAX_CONNS {
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let (peer, tls) = (Arc::clone(peer), Arc::clone(tls));
        peer.conns.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            if let Err(err) = connection(&peer, &tls, stream, ip) {
                eprintln!("cepheus peer: {ip}: {err:#}");
            }
            peer.conns.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

pub fn authorized(dir: &Path, key: &str) -> bool {
    let hash = auth::sha256_hex(key.as_bytes());
    // Every line is compared, so the time taken does not say which matched.
    hub_key_hashes(dir)
        .unwrap_or_default()
        .iter()
        .fold(false, |found, stored| auth::ct_eq(hash.as_bytes(), stored.as_bytes()) | found)
}

fn connection(peer: &Peer, tls: &Arc<ServerConfig>, tcp: TcpStream, ip: IpAddr) -> Result<()> {
    let (conn, tcp) = tls::accept(tls, tcp)?;
    let mut local = tls::bridged(conn, tcp)?;
    local.set_read_timeout(Some(tls::CONNECT_TIMEOUT))?;
    let hello: Hello = read_msg(&mut local)?.context("closed before its hello")?;
    if hello.v != PEER_VERSION || !authorized(&peer.dir, &hello.key) {
        if let Ok(mut limiter) = peer.limiter.lock() {
            limiter.fail(ip, Instant::now());
        }
        let refusal = if hello.v != PEER_VERSION {
            format!("peer version {} differs from this host's {PEER_VERSION}; reinstall it", hello.v)
        } else {
            "this hub's key is not the one recorded here; run `cepheus hosts install` again".to_string()
        };
        let _ = write_msg(&mut local, &Welcome { ok: false, error: Some(refusal.clone()), ..Welcome::default() });
        bail!("refused: {refusal}");
    }
    if let Ok(mut limiter) = peer.limiter.lock() {
        limiter.succeed(ip);
    }
    local.set_read_timeout(None)?;
    let (build, built) = hosts::this_build().clone();
    let welcome = Welcome {
        ok: true,
        error: None,
        version: PROTOCOL_VERSION,
        host: peer.name.clone(),
        start_dir: peer.start_dir.to_string_lossy().into_owned(),
        build,
        built,
        capabilities: plugin_capabilities(&Config::load()),
    };
    write_msg(&mut local, &welcome)?;
    match hello.channel {
        Channel::Deck => {
            let service = match &peer.socket {
                Some(socket) => UnixStream::connect(socket).context("the service socket")?,
                None => client::connect_local_or_spawn()?,
            };
            pipe(local, service)
        }
        Channel::Host => requests(peer, local, ip),
    }
}

/// Copy bytes both ways until either end closes, then close both.
pub fn pipe(a: UnixStream, b: UnixStream) -> Result<()> {
    let (mut a_read, mut b_write) = (a.try_clone()?, b.try_clone()?);
    let forward = thread::spawn(move || {
        let _ = io::copy(&mut a_read, &mut b_write);
        let _ = b_write.shutdown(Shutdown::Both);
        let _ = a_read.shutdown(Shutdown::Both);
    });
    let (mut b_read, mut a_write) = (b, a);
    let _ = io::copy(&mut b_read, &mut a_write);
    let _ = a_write.shutdown(Shutdown::Both);
    let _ = b_read.shutdown(Shutdown::Both);
    let _ = forward.join();
    Ok(())
}

fn request_plugin(req: &HostReq) -> Option<&'static str> {
    match req {
        HostReq::MirrorFetch => Some("git-mirror"),
        HostReq::Logins
        | HostReq::GetLogin { .. }
        | HostReq::PutLogin { .. }
        | HostReq::ProfileLogins
        | HostReq::GetProfileLogin { .. }
        | HostReq::PutProfileLogin { .. } => Some("login-sync"),
        _ => None,
    }
}
fn plugin_capabilities(loaded: &(Config, Option<String>)) -> Vec<String> {
    let mut capabilities =
        vec!["plugin-capabilities-v1".into(), phone_forward::CAPABILITY.into(), client::coordinator::CAPABILITY.into()];
    let (config, error) = loaded;
    if error.is_some() {
        return capabilities;
    }
    if config.plugin_enabled("git-mirror") && mirror::Settings::from_config(config).is_ok() {
        capabilities.push(mirror::CAPABILITY.into());
    }
    if config.sync_logins && config.plugin_enabled("login-sync") && logins::Settings::from_config(config).is_ok() {
        capabilities.push("login-sync".into());
        capabilities.push("login-sync-profiles-v1".into());
    }
    capabilities
}
fn authorize_sync(req: &HostReq, loaded: &(Config, Option<String>)) -> Result<()> {
    let Some(plugin) = request_plugin(req) else { return Ok(()) };
    let (config, error) = loaded;
    if error.is_some() || !config.plugin_enabled(plugin) {
        bail!("{plugin} is disabled or its configuration is unavailable");
    }
    if plugin == "git-mirror" {
        mirror::Settings::from_config(config)?;
        return Ok(());
    }
    if !config.sync_logins {
        bail!("login-sync is disabled");
    }
    let settings = logins::Settings::from_config(config)?;
    match req {
        HostReq::GetProfileLogin { profile, tool } | HostReq::PutProfileLogin { profile, tool, .. } => {
            settings.accounts.profile(profile, tool.provider())?;
        }
        _ => {}
    }
    match req {
        HostReq::GetLogin { tool }
        | HostReq::PutLogin { tool, .. }
        | HostReq::GetProfileLogin { tool, .. }
        | HostReq::PutProfileLogin { tool, .. }
            if !settings.tools.contains(tool) =>
        {
            bail!("tool is not enabled for login-sync")
        }
        HostReq::GetLogin { .. } | HostReq::GetProfileLogin { .. } if !settings.direction.pull() => {
            bail!("login-sync policy does not allow host-to-hub transfers")
        }
        HostReq::PutLogin { .. } | HostReq::PutProfileLogin { .. } if !settings.direction.push() => {
            bail!("login-sync policy does not allow hub-to-host changes")
        }
        _ => {}
    }
    Ok(())
}

fn requests(peer: &Peer, local: UnixStream, source: IpAddr) -> Result<()> {
    #[cfg(test)]
    if let Some(config) = &peer.test_config {
        return requests_from(peer, local, || (config.clone(), None), Some(source));
    }
    requests_from(peer, local, Config::load, Some(source))
}

#[cfg(test)]
fn requests_using(peer: &Peer, local: UnixStream, load_config: impl Fn() -> (Config, Option<String>)) -> Result<()> {
    requests_from(peer, local, load_config, None)
}

fn requests_from(
    peer: &Peer,
    mut local: UnixStream,
    load_config: impl Fn() -> (Config, Option<String>),
    source: Option<IpAddr>,
) -> Result<()> {
    let failed = |err: anyhow::Error| HostResp::Error { text: format!("{err:#}") };
    // The chats the hub's phone gateway follows on this channel, and the
    // logs its searches read.
    let mut chats = chat::Cache::default();
    let mut index = search::Index::default();
    let mut forwarded = phone_forward::Relay::default();
    while let Some(req) = read_msg::<_, HostReq>(&mut local)? {
        let req = match req {
            HostReq::PhoneForward { host, request } => {
                let len = if host.is_none() {
                    phone_forward::native_payload_len(&request)?
                } else {
                    phone_forward::payload_len(&request)?
                };
                if let Some(host) = host {
                    let bytes = (len > 0).then(|| read_payload(&mut local, len)).transpose()?;
                    let (config, error) = load_config();
                    let response = if error.is_some() {
                        failed(anyhow::anyhow!("coordinator host configuration is unavailable"))
                    } else {
                        match forwarded.relay(
                            &config,
                            peer.dir.parent().context("peer state directory")?,
                            &host,
                            &request,
                            bytes.as_deref(),
                        ) {
                            Ok(response) => response,
                            Err(error) => failed(error),
                        }
                    };
                    write_msg(&mut local, &response)?;
                    continue;
                }
                *request
            }
            request => request,
        };
        let loaded = request_plugin(&req).map(|_| load_config());
        if let Some(config) = &loaded {
            if let Err(error) = authorize_sync(&req, config) {
                write_msg(&mut local, &failed(error))?;
                continue;
            }
        }
        let config = loaded.as_ref().map(|(config, _)| config);
        let resp = match req {
            HostReq::PhoneForward { .. } => unreachable!("forwarding is unwrapped once"),
            HostReq::PhoneStats => HostResp::Stats { stats: peer.stats(false) },
            HostReq::PhoneModels => HostResp::PhoneModels {
                tools: serde_json::to_value(remote::models::all(&remote::models::Homes {
                    claude: peer.logins.claude_dir.clone(),
                    codex: peer.logins.codex_dir.clone(),
                    ..remote::models::Homes::here()
                }))?,
            },
            HostReq::ViewerIdentity => {
                match peer.dir.parent().context("peer state directory").and_then(hosts::hub_key_hash) {
                    Ok(sha256) => HostResp::ViewerIdentity { sha256 },
                    Err(error) => failed(error),
                }
            }
            HostReq::AuthorizeViewer { sha256 } => match record_hub_key_hash(&peer.dir, &sha256) {
                Ok(()) => HostResp::Done,
                Err(error) => failed(error),
            },
            HostReq::ConfigureCoordinator { host } => match source
                .context("coordinator enrollment needs an authenticated source")
                .and_then(|source| client::coordinator::configure_viewer(&peer.config_file, host, source))
            {
                Ok(()) => HostResp::Done,
                Err(error) => failed(error),
            },
            HostReq::ScanReport { id } => match remote::hub::control::read_report(&peer.scans_dir, &id) {
                Ok(text) => HostResp::ScanReport { text },
                Err(err) => failed(err),
            },
            HostReq::Stats { resources_only } => HostResp::Stats { stats: peer.stats(resources_only) },
            HostReq::ApiCost { instance, cursor } => {
                let state = peer.dir.parent().context("peer state directory")?;
                match crate::api_cost::peer_export(state, &crate::api_cost::Roots::from_env(), &instance, cursor) {
                    Ok(export) => HostResp::ApiCost { export },
                    Err(err) => failed(err),
                }
            }
            HostReq::Update { len, sha256, built } => {
                let bytes = read_payload(&mut local, len)?;
                match replace_program(&peer.program, &bytes, &sha256, built) {
                    Ok(()) => HostResp::Updated,
                    Err(err) => failed(err),
                }
            }
            HostReq::MirrorFetch => {
                let settings = mirror::Settings::from_config(config.context("missing git-mirror policy")?)?;
                match mirror::fetch(&peer.start_dir, &settings.remote) {
                    Ok(ff) => HostResp::Mirrored { ff },
                    Err(error) => failed(error),
                }
            }
            HostReq::Logins => {
                let now = logins::now_ms();
                let settings = logins::Settings::from_config(config.context("missing login-sync policy")?)?;
                let logins = settings
                    .tools
                    .iter()
                    .map(|&tool| {
                        Ok(ToolLogin { tool, state: logins::state_with_settings(&peer.logins, tool, now, &settings)? })
                    })
                    .collect::<Result<Vec<_>>>();
                match logins {
                    Ok(logins) => HostResp::Logins { logins },
                    Err(error) => failed(error),
                }
            }
            HostReq::GetLogin { tool } => {
                match logins::Settings::from_config(config.context("missing login-sync policy")?)
                    .and_then(|settings| logins::export_with_settings(&peer.logins, tool, &settings))
                {
                    Ok(login) => HostResp::Login { login },
                    Err(err) => failed(err),
                }
            }
            HostReq::PutLogin { tool, login } => {
                match logins::Settings::from_config(config.context("missing login-sync policy")?)
                    .and_then(|settings| logins::import_with_settings(&peer.logins, tool, &login, &settings))
                {
                    Ok(()) => HostResp::Done,
                    Err(err) => failed(err),
                }
            }
            HostReq::ProfileLogins => {
                let now = logins::now_ms();
                let settings = logins::Settings::from_config(config.context("missing login-sync policy")?)?;
                let logins = settings
                    .accounts
                    .profiles
                    .iter()
                    .filter(|(_, profile)| settings.tools.contains(&profile.login_tool()))
                    .map(|(id, profile)| ProfileLogin {
                        profile: id.clone(),
                        tool: profile.login_tool(),
                        state: logins::state(profile, profile.login_tool(), now),
                    })
                    .collect();
                HostResp::ProfileLogins { logins }
            }
            HostReq::GetProfileLogin { profile, tool } => {
                let settings = logins::Settings::from_config(config.context("missing login-sync policy")?)?;
                let profile = settings.accounts.profile(&profile, tool.provider())?;
                match logins::export_profile(profile) {
                    Ok(login) => HostResp::Login { login },
                    Err(error) => failed(error),
                }
            }
            HostReq::PutProfileLogin { profile, tool, login } => {
                let settings = logins::Settings::from_config(config.context("missing login-sync policy")?)?;
                let profile = settings.accounts.profile(&profile, tool.provider())?;
                match logins::import_profile(profile, &login, logins::now_ms()) {
                    Ok(()) => HostResp::Done,
                    Err(error) => failed(error),
                }
            }
            HostReq::Chat { session, from, generation } => {
                HostResp::Chat { chunk: chats.chunk(&session, &peer.chat_dirs(), from, generation) }
            }
            HostReq::Image { session, key } => {
                let source = chats.get(&session, &peer.chat_dirs()).map(|l| l.source.clone());
                match source.context("no such conversation").and_then(|s| s.image(&key)) {
                    Ok((media, data)) => HostResp::Image { media, data },
                    Err(err) => failed(err),
                }
            }
            HostReq::Search { sessions, words } => match search::words(&words.join(" ")) {
                Ok(words) => HostResp::Search { found: index.search(&sessions, &peer.chat_dirs(), &words) },
                Err(err) => failed(err),
            },
            HostReq::Commands { tool, cwd } => {
                let cwd = cwd.map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(|| peer.start_dir.clone());
                let home = paths::home();
                match remote::commands::all(tool, &cwd, &home, &peer.logins.claude_dir, &peer.logins.codex_dir) {
                    Ok(entries) => HostResp::Commands { entries: serde_json::to_value(entries)? },
                    Err(err) => failed(err),
                }
            }
            HostReq::PutFile { name, len } => {
                if len > uploads::FILE_MAX {
                    bail!("a file of {len} bytes is more than the phone may attach");
                }
                let bytes = read_payload(&mut local, len)?;
                match keep_upload(&peer.uploads_dir(), &name, &bytes) {
                    Ok(path) => HostResp::Put { path: path.to_string_lossy().into_owned() },
                    Err(err) => failed(err),
                }
            }
        };
        write_msg(&mut local, &resp)?;
        local.flush()?;
    }
    Ok(())
}

/// How long a file the phone attached through the hub is kept here, as the
/// gateway keeps its own (`uploads.rs`).
const UPLOAD_KEEP: Duration = Duration::from_secs(7 * 86_400);

/// Keep a file the phone attached under its own name (made safe, and
/// numbered when taken), first clearing out week-old ones.
pub fn keep_upload(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    util::ensure_private_dir(dir)?;
    for entry in fs::read_dir(dir)?.flatten() {
        let old = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok());
        if old.is_some_and(|age| age > UPLOAD_KEEP) {
            let _ = fs::remove_file(entry.path());
        }
    }
    let path = uploads::free_path(dir, &uploads::safe_name(name));
    util::write_atomic(&path, bytes).context("could not keep the file")?;
    Ok(path)
}

/// The largest program or bundle a request may carry.
pub const MAX_PAYLOAD: u64 = 512 << 20;

/// The `len` raw bytes that follow a request.
pub fn read_payload<R: Read>(r: &mut R, len: u64) -> Result<Vec<u8>> {
    if len > MAX_PAYLOAD {
        bail!("{len} bytes is more than a request may carry");
    }
    let mut bytes = vec![0u8; len as usize];
    r.read_exact(&mut bytes).context("reading the request's bytes")?;
    Ok(bytes)
}

/// Put the hub's build in place of `program`. It must be what the hub sent
/// and run here, or the program stays as it was: a build that cannot start
/// would leave the host unreachable. The service then updates in place and
/// this listener exits for the service to restart it (`watch_exit`).
pub fn replace_program(program: &Path, bytes: &[u8], sha256: &str, built: u64) -> Result<()> {
    if auth::sha256_hex(bytes) != sha256 {
        bail!("the program arrived damaged");
    }
    let dir = program.parent().context("the program's directory")?;
    // Each request stages a file of its own, created here and removed only
    // by it: hubs' connections run side by side, and one name per process
    // let a failed update delete or overwrite another's finished stage before
    // it was put in place (OPS-S11801).
    static STAGE: AtomicU64 = AtomicU64::new(0);
    let mut staged = None;
    for _ in 0..64 {
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = dir.join(format!(".cepheus.update.{}.{}", std::process::id(), STAGE.fetch_add(1, Ordering::Relaxed)));
        match fs::OpenOptions::new().write(true).create_new(true).mode(0o755).open(&tmp) {
            Ok(file) => {
                staged = Some((tmp, file));
                break;
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err).context("staging the new build"),
        }
    }
    let (tmp, file) = staged.context("staging the new build: no free update file name")?;
    let result = (|| -> Result<()> {
        {
            let mut file = file;
            file.write_all(bytes)?;
            file.sync_all()?;
            let when = std::time::UNIX_EPOCH + Duration::from_secs(built);
            file.set_modified(when)?;
        }
        // Another thread forking while the file was still open for writing
        // leaves it briefly busy to exec.
        let mut tries = 0;
        let out = loop {
            match std::process::Command::new(&tmp).arg("--version").output() {
                Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) && tries < 20 => {
                    tries += 1;
                    thread::sleep(Duration::from_millis(50));
                }
                other => break other.context("running the new build")?,
            }
        };
        if !out.status.success() {
            bail!("the new build does not run here: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        fs::rename(&tmp, program)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

impl Peer {
    fn stats(&self, resources_only: bool) -> HostStats {
        let resources = match self.sampler.lock() {
            Ok(mut sampler) => {
                sampler.sample(&self.start_dir);
                sampler.now.clone()
            }
            Err(_) => Default::default(),
        };
        let (status, head, branches) = if resources_only {
            (None, None, Vec::new())
        } else {
            let mut git = self.git.lock().unwrap_or_else(|e| e.into_inner());
            match git.as_ref() {
                Some((at, status, head, branches)) if at.elapsed() < GIT_FRESH => {
                    (status.clone(), head.clone(), branches.clone())
                }
                _ => {
                    let status = gitwt::branch_status(&self.start_dir);
                    let head = hosts::git_text(&self.start_dir, &["rev-parse", "HEAD"]);
                    let branches = gitwt::branches(&self.start_dir);
                    *git = Some((Instant::now(), status.clone(), head.clone(), branches.clone()));
                    (status, head, branches)
                }
            }
        };
        HostStats {
            cpu: resources.cpu,
            memory: resources.memory,
            disk_used: resources.disk.map(|d| d.used),
            disk_available: resources.disk.map(|d| d.available),
            worktrees: status.as_ref().map_or(0, |s| s.worktrees),
            branch: status.map(|s| s.branch),
            head,
            branches,
        }
    }
}

#[cfg(test)]
#[path = "../tests/hosts_peer.rs"]
mod tests;
