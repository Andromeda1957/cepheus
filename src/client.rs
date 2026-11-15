//! Public clients share the declared coordinator; native service links stay local.

pub mod coordinator;

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{Config, START_DIR_ENV};
use crate::service::current_exe_clean;
use crate::{paths, util};

/// Connect to the selected shared coordinator, without starting a service.
pub fn connect() -> io::Result<UnixStream> {
    match coordinator::selected().map_err(|err| io::Error::other(format!("{err:#}")))? {
        Some(host) => coordinator::connect(&host, &paths::state_dir()),
        None => connect_local(),
    }
}

/// Public gateway connections preserve an explicit native socket override
/// (used by isolated gateways/tests), while the normal path follows selection.
pub fn connect_public_socket(socket: &Path) -> io::Result<UnixStream> {
    if socket == paths::socket_path() {
        connect()
    } else {
        UnixStream::connect(socket)
    }
}

/// Whether this physical interface uses another machine's coordinator.
pub fn is_remote_coordinator() -> bool {
    coordinator::is_remote()
}

/// Connect only to this machine's execution service, without starting it.
/// Hooks, local admission and native peer channels must use this API.
pub fn connect_local() -> io::Result<UnixStream> {
    UnixStream::connect(paths::socket_path())
}

/// Connect to the shared coordinator. Only a local coordinator may be started;
/// an unavailable remote coordinator never creates a second session registry.
pub fn connect_or_spawn() -> Result<UnixStream> {
    match coordinator::selected()? {
        Some(host) => Ok(coordinator::connect(&host, &paths::state_dir())?),
        None => connect_local_or_spawn(),
    }
}

/// Connect to this machine's native service, starting it when necessary.
pub fn connect_local_or_spawn() -> Result<UnixStream> {
    if let Ok(stream) = connect_local() {
        return Ok(stream);
    }
    // An unreachable user manager must not lock the owner out of the deck. The
    // service's single-instance lock refuses a duplicate if the unit starts too.
    let managed_wait = crate::startup::request_start(&Config::load().0).unwrap_or_else(|error| {
        eprintln!("cepheus: the startup service could not be started ({error:#}); starting the service directly");
        None
    });
    if managed_wait.is_none() {
        spawn_service()?;
    }
    let deadline = Instant::now() + managed_wait.unwrap_or(Duration::from_secs(8));
    loop {
        match connect_local() {
            Ok(stream) => return Ok(stream),
            Err(err) if Instant::now() > deadline => {
                let log = paths::state_dir().join("service.log");
                return Err(err).with_context(|| format!("the service did not start; see {}", log.display()));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Start `cepheus service` detached from this terminal, logging to the
/// state directory. Sessions inherit this environment (display, D-Bus, PATH).
fn spawn_service() -> Result<()> {
    // Resolve here, in the deck's own directory, so a missing repository is
    // reported in this terminal rather than only in the service log.
    let start_dir = Config::load().0.resolve_start_dir()?;
    let state = paths::state_dir();
    util::ensure_private_dir(&state)?;
    let log_path = state.join("service.log");
    if fs::metadata(&log_path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = fs::rename(&log_path, state.join("service.log.1"));
    }
    let log = fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&log_path)?;
    let mut cmd = Command::new(current_exe_clean());
    cmd.arg("service")
        .current_dir(paths::home())
        .env(START_DIR_ENV, &start_dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting the Cepheus service")?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
