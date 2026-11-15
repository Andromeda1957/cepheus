//! A public interface selects an enrolled coordinator by name. Its existing
//! host inventory remains the authority for endpoints and certificate pins.

use std::fs::File;
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Config, HostConfig};
use crate::hosts::wire::{Channel, HostReq, HostResp};
use crate::protocol::{read_msg, write_msg};
use crate::{hosts, paths, util};

pub const CAPABILITY: &str = "coordinator-viewers-v1";
const MAX_DECLARATION: u64 = 4096;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    host: Target,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Target {
    Remote(String),
    Local(()),
}

/// Enrolled fleets automatically offer their native service as a coordinator.
/// A configured listener takes precedence; standalone decks remain local-only.
pub fn listener(config: &Config) -> Option<SocketAddr> {
    if !config.peer_listen.trim().is_empty() {
        config.peer_listen.parse().ok()
    } else if !config.hosts.is_empty() {
        Some(SocketAddr::from(([0, 0, 0, 0], crate::config::PEER_PORT)))
    } else {
        None
    }
}

/// Default incoming sources are exact IPs from the already enrolled fleet.
/// An explicit peer allowlist retains its existing meaning and takes precedence.
pub fn allowed_sources(config: &Config) -> Vec<String> {
    if !config.peer_allow.is_empty() {
        return config.peer_allow.clone();
    }
    let mut addresses: Vec<String> = config
        .hosts
        .iter()
        .flat_map(|host| host.addresses.iter())
        .filter_map(|address| address.parse::<SocketAddr>().ok())
        .map(|address| crate::remote::auth::plain_ip(address.ip()).to_string())
        .collect();
    addresses.sort();
    addresses.dedup();
    addresses
}

/// Sidecar beside the existing config; it contains no keys or endpoint trust.
pub fn declaration_path(config: &Path) -> PathBuf {
    config.with_file_name("coordinator.json")
}

fn read_declaration(config: &Path) -> Result<Option<Declaration>> {
    let path = declaration_path(config);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_DECLARATION + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DECLARATION {
        bail!("coordinator declaration is too large");
    }
    let declaration: Declaration = serde_json::from_slice(&bytes).context("invalid coordinator declaration")?;
    if let Target::Remote(name) = &declaration.host {
        if !crate::config::valid_host_name(name) {
            bail!("coordinator must name an enrolled host");
        }
    }
    Ok(Some(declaration))
}

fn declared_host(config: &Path) -> Result<Option<String>> {
    Ok(read_declaration(config)?.and_then(|declaration| match declaration.host {
        Target::Remote(name) => Some(name),
        Target::Local(()) => None,
    }))
}

fn enrolled(config: &Config, name: &str) -> Result<HostConfig> {
    let mut matches = config.hosts.iter().filter(|host| host.name == name);
    let host = matches.next().with_context(|| format!("shared coordinator {name:?} is not an enrolled host"))?;
    if matches.next().is_some()
        || host.addresses.is_empty()
        || host.fingerprint.len() != 64
        || !host.fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("shared coordinator {name:?} has an invalid or ambiguous enrollment");
    }
    Ok(host.clone())
}

pub fn selected() -> Result<Option<HostConfig>> {
    let Some(name) = declared_host(&paths::config_path())? else { return Ok(None) };
    let (config, error) = Config::load();
    if let Some(error) = error {
        bail!("shared coordinator configuration is unavailable: {error}");
    }
    Ok(Some(enrolled(&config, &name)?))
}

/// Only an undeclared fleet coordinator may automatically provision viewers.
/// Explicit null selects native use and opts out of automatic enrollment.
pub fn is_coordinator() -> bool {
    matches!(read_declaration(&paths::config_path()), Ok(None))
}

pub fn selected_from(config_path: &Path, config: &Config) -> Result<Option<HostConfig>> {
    declared_host(config_path)?.map(|name| enrolled(config, &name)).transpose()
}

pub fn is_remote() -> bool {
    declared_host(&paths::config_path()).ok().flatten().is_some()
}

/// The existing pinned TLS/UnixStream bridge carries the ordinary deck protocol.
/// This path never starts a local service or falls back to a different registry.
pub fn connect(host: &HostConfig, state: &Path) -> io::Result<UnixStream> {
    let key = hosts::existing_hub_key(state)
        .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "shared coordinator viewer is not enrolled"))?;
    hosts::open(host, &key, Channel::Deck)
        .map(|(stream, _)| stream)
        .map_err(|error| io::Error::other(format!("shared coordinator {} unavailable: {error:#}", host.name)))
}

fn configuration_lock(config_path: &Path) -> Result<File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let path = declaration_path(config_path);
    util::ensure_private_dir(path.parent().context("coordinator configuration directory")?)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path.with_file_name("coordinator.lock"))?;
    // SAFETY: flock uses this call's live descriptor. No network I/O happens
    // while automatic provisioning and explicit owner selection serialize.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error()).context("locking coordinator selection");
    }
    Ok(lock)
}

fn write_selection(config_path: &Path, config: &Config, name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        enrolled(config, name)?;
    }
    let host = name.map_or(Target::Local(()), |name| Target::Remote(name.to_string()));
    util::write_atomic(&declaration_path(config_path), &serde_json::to_vec_pretty(&Declaration { host })?)?;
    Ok(())
}

/// Explicit selection may replace a prior choice; automatic provisioning cannot.
/// Null is a persistent native-only choice that opts out of automatic enrollment.
pub fn select(config_path: &Path, config: &Config, name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        enrolled(config, name)?;
    }
    let _selection = configuration_lock(config_path)?;
    write_selection(config_path, config, name)
}

/// Run on the coordinator: trust a physical interface's own key by public hash.
/// Its existing pinned host channel authenticates discovery; no private key moves.
#[cfg(test)]
pub fn enroll_viewer(host: &HostConfig, state: &Path) -> Result<()> {
    let key = hosts::hub_key(state)?;
    let (mut stream, welcome) = hosts::open(host, &key, Channel::Host)?;
    if !welcome.capabilities.iter().any(|capability| capability == CAPABILITY) {
        bail!("host does not support shared coordinator enrollment; update its deck first");
    }
    stream.set_read_timeout(Some(hosts::tls::CONNECT_TIMEOUT))?;
    write_msg(&mut stream, &HostReq::ViewerIdentity)?;
    let hash = match read_msg(&mut stream)?.context("host closed during viewer enrollment")? {
        HostResp::ViewerIdentity { sha256 } => sha256,
        HostResp::Error { text } => bail!("viewer enrollment: {text}"),
        _ => bail!("unexpected viewer enrollment response"),
    };
    hosts::peer::record_hub_key_hash(&hosts::peer::peer_dir(state), &hash)
}

/// Provision only the callback endpoint belonging to the authenticated caller.
/// The caller may add trust, but cannot silently replace an existing pin.
pub fn configure_viewer(config_path: &Path, mut host: HostConfig, source: IpAddr) -> Result<()> {
    let _selection = configuration_lock(config_path)?;
    if let Some(declaration) = read_declaration(config_path)? {
        match declaration.host {
            Target::Remote(ref name) if name == &host.name => {}
            Target::Remote(name) => bail!("physical interface already selects coordinator {name:?}"),
            Target::Local(()) => bail!("physical interface explicitly selects native-only use"),
        }
    }
    let (mut config, error) = Config::load_from(config_path);
    if let Some(error) = error {
        bail!("viewer config: {error}");
    }
    let inventory = crate::config::hosts_path(config_path);
    if inventory.exists() {
        let recorded = crate::config::read_hosts_file(config_path)
            .context("existing viewer host inventory is unavailable; it was preserved")?;
        if recorded.hosts != config.hosts {
            bail!("existing viewer host inventory is invalid or ambiguous; it was preserved");
        }
    } else {
        let recorded = crate::config::read_hosts(config_path);
        if !recorded.is_empty() && recorded != config.hosts {
            bail!("existing viewer host inventory is invalid or ambiguous; it was preserved");
        }
    }
    if !crate::config::valid_host_name(&host.name)
        || host.addresses.len() != 1
        || host.fingerprint.len() != 64
        || !host.fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("invalid coordinator enrollment");
    }
    let endpoint: SocketAddr = host.addresses[0].parse().context("coordinator endpoint must be a numeric address")?;
    if endpoint.port() == 0 || crate::remote::auth::plain_ip(endpoint.ip()) != crate::remote::auth::plain_ip(source) {
        bail!("coordinator endpoint does not belong to the authenticated caller");
    }
    if let Some(existing) = config.hosts.iter().find(|existing| existing.name == host.name) {
        if !existing.fingerprint.eq_ignore_ascii_case(&host.fingerprint) {
            bail!("coordinator enrollment conflicts with an existing certificate pin");
        }
        host.tag = existing.tag;
        host.ssh = existing.ssh.clone();
    } else {
        host.tag = (1..=crate::protocol::MAX_HOST_TAG)
            .find(|tag| !config.hosts.iter().any(|existing| existing.tag == *tag))
            .context("no host identity is available for the coordinator")?;
    }
    let name = host.name.clone();
    config.hosts.retain(|existing| existing.name != name);
    config.hosts.push(host);
    // Publish the remote choice first. A failed hosts write then stays
    // unavailable instead of electing another coordinator on the viewer.
    write_selection(config_path, &config, Some(&name))?;
    crate::config::write_hosts(config_path, &config.hosts)
}

/// Idempotent installer operation run on the coordinator after normal host
/// enrollment: authorize the viewer's digest and install the reverse pinned route.
pub fn provision_viewer(config: &Config, host: &HostConfig, state: &Path) -> Result<()> {
    if !is_coordinator() {
        bail!("only the selected coordinator may provision physical interfaces");
    }
    let listener = listener(config).context("coordinator peer listener must be configured")?;
    let fingerprint = crate::remote::store::fingerprint(&hosts::peer::peer_dir(state))
        .context("coordinator peer certificate is not initialized")?;
    let name = config.local_host_label().to_ascii_lowercase();
    if !crate::config::valid_host_name(&name) {
        bail!("coordinator host name must be a stable lowercase host identifier");
    }
    let key = hosts::hub_key(state)?;
    let (mut stream, welcome, source) = hosts::open_with_local_addr(host, &key, Channel::Host)?;
    if !welcome.capabilities.iter().any(|capability| capability == CAPABILITY) {
        bail!("host does not support shared coordinator enrollment; update its deck first");
    }
    if listener.port() == 0 || (!listener.ip().is_unspecified() && listener.ip() != source.ip()) {
        bail!("coordinator peer listener does not serve the interface used to reach the viewer");
    }
    stream.set_read_timeout(Some(hosts::tls::CONNECT_TIMEOUT))?;
    write_msg(&mut stream, &HostReq::ViewerIdentity)?;
    match read_msg(&mut stream)?.context("host closed during viewer enrollment")? {
        HostResp::ViewerIdentity { sha256 } => {
            hosts::peer::record_hub_key_hash(&hosts::peer::peer_dir(state), &sha256)?
        }
        HostResp::Error { text } => bail!("viewer enrollment: {text}"),
        _ => bail!("unexpected viewer enrollment response"),
    }
    let coordinator = HostConfig {
        role: Default::default(),
        name,
        label: Some(config.local_host_label()),
        tag: 1,
        addresses: vec![SocketAddr::new(source.ip(), listener.port()).to_string()],
        ssh: None,
        fingerprint,
    };
    write_msg(&mut stream, &HostReq::ConfigureCoordinator { host: coordinator })?;
    match read_msg(&mut stream)?.context("host closed during coordinator configuration")? {
        HostResp::Done => Ok(()),
        HostResp::Error { text } => bail!("coordinator configuration: {text}"),
        _ => bail!("unexpected coordinator configuration response"),
    }
}

/// Fixed native installer entry point, dispatched as `hosts coordinator`.
pub fn run(args: &[String]) -> i32 {
    let result = (|| -> Result<()> {
        let (config, error) = Config::load();
        if let Some(error) = error {
            bail!("config: {error}");
        }
        match args {
            [clear] if clear == "--clear" => select(&paths::config_path(), &config, None),
            [enroll, name] if enroll == "enroll" => {
                provision_viewer(&config, &enrolled(&config, name)?, &paths::state_dir())
            }
            [name] => select(&paths::config_path(), &config, Some(name)),
            _ => bail!("usage: cepheus hosts coordinator <enrolled-host>|--clear|enroll <viewer-host>"),
        }
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("cepheus coordinator: {error:#}");
            1
        }
    }
}

#[cfg(test)]
#[path = "../tests/client_coordinator.rs"]
mod tests;
