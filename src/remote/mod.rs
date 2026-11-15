//! The phone gateway, `cepheus remote`. A process of its own beside the service, so a fault here never touches
//! a hosted session: it connects to the service as a `remote` client and serves the phone over TLS on the VPN
//! address only. The service starts it when the config sets `remote`; it exits when the program is replaced,
//! `remote` is turned off or the service is gone.

pub mod agy;
pub mod apk;
pub mod auth;
pub mod chat;
pub mod commands;
pub mod hub;
pub mod links;
mod live;
pub mod models;
pub mod questionpick;
pub mod search;
pub mod server;
pub mod store;
#[path = "../../plugins/providers/streaming.rs"]
pub(crate) mod streaming;
pub mod uploads;
pub mod wire;

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::{paths, prompt, util};

/// Hold the gateway's lock for as long as this process runs; none when
/// another gateway holds it.
pub fn take_lock(dir: &Path) -> io::Result<Option<fs::File>> {
    util::ensure_private_dir(dir)?;
    let file =
        fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join(store::LOCK))?;
    // SAFETY: flock on a descriptor this process owns.
    let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    Ok(held.then_some(file))
}

/// Whether a gateway runs, from its lock.
pub fn running(dir: &Path) -> bool {
    matches!(take_lock(dir), Ok(None))
}

/// Start `exe remote` detached and reparented, so no service build waits
/// for it (a handover runs a new build in the same process). Output goes
/// to `remote.log` in the state directory.
pub fn spawn_detached(exe: &Path, state_dir: &Path) -> Result<()> {
    spawn_detached_as(exe, state_dir, "remote")
}

/// The same for any helper process, `exe <command>`, whose output goes to
/// `<command>.log`: the gateway and the peer listener (`hosts/peer.rs`).
pub fn spawn_detached_as(exe: &Path, state_dir: &Path, command: &str) -> Result<()> {
    util::ensure_private_dir(state_dir)?;
    let log_path = state_dir.join(format!("{command}.log"));
    if fs::metadata(&log_path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = fs::rename(&log_path, state_dir.join(format!("{command}.log.1")));
    }
    let log = fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&log_path)?;
    let mut cmd = Command::new(exe);
    cmd.arg(command).current_dir(paths::home()).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid, fork and _exit are async-signal-safe and touch no
    // Rust state. The child forks once more and exits, so the gateway is
    // reparented to init and never left a zombie of this service.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            match libc::fork() {
                -1 => Err(io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
    let mut child = cmd.spawn().with_context(|| format!("starting `cepheus {command}`"))?;
    let _ = child.wait();
    Ok(())
}

pub fn run() -> i32 {
    match run_gateway() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("cepheus remote: {err:#}");
            1
        }
    }
}

fn run_gateway() -> Result<()> {
    let (config, err) = Config::load();
    if let Some(err) = err {
        anyhow::bail!("config: {err}");
    }
    if !config.remote {
        anyhow::bail!("\"remote\" is off in {}; set it to true to use the phone app", paths::config_path().display());
    }
    let dir = store::remote_dir(&paths::state_dir());
    let Some(_lock) = take_lock(&dir)? else {
        eprintln!("cepheus remote: another gateway is running");
        return Ok(());
    };
    let allow = hub::parse_allow(&config.remote_allow);
    let host = config.remote_listen.rsplit_once(':').map_or("", |(h, _)| h).trim_matches(['[', ']']);
    let identity = store::load_or_create_identity(&dir, host)?;
    let tls = server::tls_config(&identity.cert_der, &identity.key_der)?;
    // Public, and the same as the pairing screen shows: lets the log confirm
    // which certificate a phone pinned.
    eprintln!("cepheus remote: certificate SHA-256 {}", identity.fingerprint);
    let opts = hub::Options {
        socket: paths::socket_path(),
        dir,
        listen: config.remote_listen.clone(),
        apk_listen: config.apk_listen.clone(),
        allow,
        claude_dir: paths::claude_dir(),
        codex_dir: paths::codex_dir(),
        opencode_db: paths::opencode_db(),
        antigravity_dir: paths::antigravity_dir(),
        sqlite3: vec!["sqlite3".into()],
        order_file: paths::order_file(),
        uploads_dir: paths::uploads_dir(),
        ping_every: Duration::from_secs(25),
        config_file: Some(paths::config_path()),
        watch_exe: true,
        service_grace: Duration::from_secs(120),
        here: config.local_host_label(),
        hosts: config.hosts.clone(),
        hub_key: crate::hosts::existing_hub_key(&paths::state_dir()),
    };
    let epoch = prompt::hex(&auth::random_bytes(8)?);
    let hub = Arc::new(hub::Hub::new(opts, epoch));
    start_threads(&hub);
    if !config.apk_listen.is_empty() {
        let (listen, dir) = (config.apk_listen.clone(), apk::dir());
        thread::Builder::new().name("apk".into()).spawn(move || apk::serve(listen, dir))?;
    }
    server::serve(hub, tls);
    Ok(())
}

/// The service connection and the poller that sends what changes.
pub fn start_threads(hub: &Arc<hub::Hub>) {
    let bridge = Arc::clone(hub);
    thread::spawn(move || bridge.bridge());
    live::start(hub);
    let poller = Arc::clone(hub);
    thread::spawn(move || {
        let mut tick = 0u64;
        loop {
            thread::sleep(Duration::from_millis(700));
            tick += 1;
            if tick.is_multiple_of(3) {
                poller.housekeeping();
            }
            poller.update_prompts();
        }
    });
}

#[cfg(test)]
#[path = "../tests/remote.rs"]
mod tests;
