//! Shared helpers for unit tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// Tear down only sessions owned by the isolated service at this fixture socket.
/// Public Stop deliberately cannot be used to kill a test's live sessions.
pub fn shutdown_service(socket: &Path) -> anyhow::Result<()> {
    finish_fixture_service(socket, true)
}

pub fn close_fixture_sessions(socket: &Path) -> anyhow::Result<()> {
    finish_fixture_service(socket, false)
}

fn finish_fixture_service(socket: &Path, stop: bool) -> anyhow::Result<()> {
    use crate::protocol::{read_msg, write_msg, ClientMsg, ClientRole, ServerMsg, PROTOCOL_VERSION};
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(scaled_timeout(std::time::Duration::from_secs(10))))?;
    write_msg(&mut stream, &ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control })?;
    let mut reader = BufReader::new(stream.try_clone()?);
    // A protocol-20 client gets the whole state once, then patches of it.
    let mut state = loop {
        match read_msg::<_, ServerMsg>(&mut reader)? {
            Some(ServerMsg::State(state)) => break state,
            Some(_) => {}
            None => return Ok(()),
        }
    };
    let live: Vec<u64> = state.sessions.iter().filter(|s| s.alive).map(|s| s.id).collect();
    for id in &live {
        write_msg(&mut stream, &ClientMsg::Close { id: *id })?;
    }
    while state.sessions.iter().any(|s| s.alive) {
        match read_msg::<_, ServerMsg>(&mut reader)? {
            Some(ServerMsg::State(whole)) => state = whole,
            Some(ServerMsg::StatePatch(patch)) => state.apply(patch),
            Some(ServerMsg::Notice { text, error: true }) => anyhow::bail!("{text}"),
            Some(_) => {}
            None => return Ok(()),
        }
    }
    if !stop {
        return Ok(());
    }
    write_msg(&mut stream, &ClientMsg::Shutdown)?;
    while let Some(reply) = read_msg::<_, ServerMsg>(&mut reader)? {
        if let ServerMsg::Notice { text, error: true } = reply {
            anyhow::bail!("{text}");
        }
    }
    Ok(())
}

/// Scale socket hang guards for slower or heavily loaded test machines.
pub fn scaled_timeout(base: std::time::Duration) -> std::time::Duration {
    let configured = std::env::var("CEPHEUS_TEST_TIMEOUT_SCALE").unwrap_or_default();
    let configured = configured.trim();
    let factor = if !configured.is_empty() && !configured.eq_ignore_ascii_case("auto") {
        let factor: f64 = configured.parse().expect("invalid test timeout scale");
        assert!(factor.is_finite() && factor >= 1.0, "test timeout scale must be at least one");
        factor
    } else {
        let load = fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|text| text.split_whitespace().next().and_then(|load| load.parse::<f64>().ok()))
            .unwrap_or(0.0);
        let cpus = std::thread::available_parallelism().map_or(1, usize::from) as f64;
        (4.0 * load / cpus).clamp(1.0, 10.0)
    };
    base.mul_f64(factor)
}

/// A unique directory directly under the configured temporary root, removed
/// on drop whether the test passes or fails.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(name: &str) -> TempDir {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("cepheus-test-{}-{name}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test dir");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, rel: &str, data: impl AsRef<[u8]>) -> PathBuf {
        let p = self.path.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&p, data).expect("write test file");
        p
    }

    /// Known calm kernel readings for tests unrelated to resource admission.
    pub fn calm_probe(&self) -> crate::scheduler::probe::Probe {
        self.write("proc/meminfo", "MemTotal: 16777216 kB\nMemAvailable: 12582912 kB\n");
        self.write("proc/pressure/memory", "full avg10=0\n");
        self.write("proc/pressure/cpu", "some avg60=0\n");
        self.write("proc/loadavg", "0 0 0 1/1 1\n");
        crate::scheduler::probe::Probe { root: Some(self.path.join("proc")) }
    }

    /// `script` as an executable at `rel`, once it can run. A test thread
    /// that forks while the file is open for writing keeps a copy of that
    /// descriptor until its child execs, and running the file meanwhile
    /// fails with "Text file busy": the clipboard tests failed so in two of
    /// seven full runs on 2026-11-01. Running it once with no arguments
    /// proves no such copy is left, and no later fork can make one.
    pub fn executable(&self, rel: &str, script: impl AsRef<[u8]>) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};
        let p = self.write(rel, script);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).expect("make it executable");
        for _ in 0..500 {
            let run = Command::new(&p).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
            match run {
                Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                _ => return p,
            }
        }
        panic!("{} stayed busy for 5 seconds", p.display());
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A peer listener on loopback for host "fedora" (tag 1), serving
/// `start_dir` with the sign-ins and logs under `logins`, its deck channel
/// piped to `socket`. It admits the hub whose state is in `hub_state`;
/// returns the host as that hub's config names it, and the hub's key.
pub fn loopback_peer(
    peer_dir: PathBuf,
    hub_state: &Path,
    start_dir: PathBuf,
    logins: crate::hosts::logins::LoginPaths,
    socket: Option<PathBuf>,
) -> (crate::config::HostConfig, String) {
    use crate::{hosts, remote};
    let identity = remote::store::load_or_create_identity(&peer_dir, "127.0.0.1").unwrap();
    let key = hosts::hub_key(hub_state).unwrap();
    hosts::peer::record_hub_key(&peer_dir, &key).unwrap();
    let tls = remote::server::tls_config(&identity.cert_der, &identity.key_der).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let peer = std::sync::Arc::new(
        hosts::peer::Peer::new(
            peer_dir,
            hosts::parse_cidrs(&["127.0.0.0/8".into()]),
            "Fedora".into(),
            start_dir,
            socket,
        )
        .with_logins(logins),
    );
    std::thread::spawn(move || hosts::peer::accept_all(&peer, &tls, listener));
    let host = crate::config::HostConfig {
        role: Default::default(),
        name: "fedora".into(),
        label: Some("Fedora".into()),
        tag: 1,
        addresses: vec![address],
        ssh: None,
        fingerprint: identity.fingerprint,
    };
    (host, key)
}

/// A synthetic Kitty using the real DCS socket framing. Every exchange checks
/// the command and window selector before returning its next bounded reply.
pub fn kitty_replies(path: &Path, replies: Vec<(&'static str, serde_json::Value)>) -> std::thread::JoinHandle<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};
    let listener = UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        for (exchange, (command, response)) in replies.into_iter().enumerate() {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut socket = loop {
                if let Ok((socket, _)) = listener.accept() {
                    break socket;
                }
                assert!(Instant::now() < deadline, "no Kitty {command} request at exchange {exchange}");
                std::thread::sleep(Duration::from_millis(5));
            };
            socket.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\x1b\\") && request.len() < 8192 {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request: serde_json::Value = serde_json::from_slice(
                request.strip_prefix(b"\x1bP@kitty-cmd").unwrap().strip_suffix(b"\x1b\\").unwrap(),
            )
            .unwrap();
            assert_eq!(request["cmd"], command);
            assert_eq!(request["payload"]["match"], "id:2");
            assert_eq!(socket.read(&mut byte).unwrap(), 0, "client must finish the request before reading");
            socket.write_all(b"\x1bP@kitty-cmd").unwrap();
            socket.write_all(response.to_string().as_bytes()).unwrap();
            socket.write_all(b"\x1b\\").unwrap();
        }
    })
}

pub fn kitty_windows(pid: u32) -> serde_json::Value {
    serde_json::json!({"ok":true,"data":serde_json::json!([{"tabs":[{"windows":[{
        "id":2,"foreground_processes":[{"pid":pid}]
    }]}]}]).to_string()})
}
