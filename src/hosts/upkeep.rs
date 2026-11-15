//! The hub's host channel to one host, kept open by a thread of its own:
//!
//! - a host on an older build gets this hub's program (`peer::replace_program`);
//! - its CPU, memory, disk and repository every `STATS_EVERY`;
//! - after the hub's refs change, an agent host fetches them (`git-mirror`);
//! - the login plugin runs on its configured schedule for agent hosts.
//!
//! A build host is asked for its CPU, memory and disk only. With no session
//! channel to it, this channel's state is its connection state.
//!
//! Disabling a plugin never disables stats, updates or session channels.
//!
//! What the owner should know is reported to the service once until it
//! changes; nothing secret is ever part of it.

use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{Config, HostConfig};
use crate::hosts;
use crate::hosts::link::{pause, report, short, HostEv};
#[cfg(test)]
use crate::hosts::logins::LoginTool;
use crate::hosts::logins::{self, LoginPaths};
use crate::hosts::mirror;
#[cfg(test)]
use crate::hosts::peer;
use crate::hosts::wire::{Channel, HostReq, HostResp, Welcome};
use crate::protocol::{read_msg, write_msg};
use crate::service::Ev;

/// How often a host's stats are read.
pub const STATS_EVERY: Duration = Duration::from_secs(2);
/// A failed mirror fetch is tried again after this long, or sooner when the
/// hub's refs change.
const MIRROR_RETRY: Duration = Duration::from_secs(60);

/// What the hub's links need from the hub itself.
#[derive(Clone, Debug)]
pub struct HubCtx {
    pub logins: LoginPaths,
    pub sync_logins: bool,
    /// Grows each time the hub repository's branch and tag refs settle after
    /// a change (`mirror::watch`).
    pub refs: Arc<AtomicU64>,
}

/// The channel broke: reconnect.
pub type Broken = String;

pub(crate) fn ask(stream: &mut UnixStream, req: &HostReq, payload: Option<&[u8]>) -> Result<HostResp, Broken> {
    write_msg(stream, req).map_err(|e| e.to_string())?;
    if let Some(bytes) = payload {
        stream.write_all(bytes).map_err(|e| e.to_string())?;
    }
    match read_msg::<_, HostResp>(stream) {
        Ok(Some(resp)) => Ok(resp),
        Ok(None) => Err("the host closed the channel".into()),
        Err(err) => Err(err.to_string()),
    }
}

/// Names the answer's kind only: a sign-in's contents never reach a message.
pub(crate) fn unexpected(resp: &HostResp) -> Broken {
    let kind =
        serde_json::to_value(resp).ok().and_then(|v| v.get("t")?.as_str().map(str::to_string)).unwrap_or_default();
    format!("unexpected answer {kind:?}")
}

/// Say `note` once: again only after it changed or cleared.
fn say(tx: &Sender<Ev>, tag: u16, last: &mut Option<String>, note: Option<String>) {
    if note != *last {
        if let Some(text) = &note {
            report(tx, HostEv::Report { tag, text: text.clone() });
        }
        *last = note;
    }
}

/// `say` for a sign-in note, which also takes back the note it replaces:
/// a copy that later succeeds no longer reports the host's login as failing.
fn say_login(tx: &Sender<Ev>, tag: u16, last: &mut Option<String>, note: Option<String>) {
    if note != *last {
        if let Some(text) = last.take() {
            report(tx, HostEv::Withdraw { tag, text });
        }
        say(tx, tag, last, note);
    }
}

/// Ask the host to fetch the hub's refs once they changed since its last
/// successful fetch. Err: the channel broke.
fn mirror_round(
    host: &HostConfig,
    stream: &mut UnixStream,
    welcome: &Welcome,
    refs: &AtomicU64,
    done: &mut Option<u64>,
    retry_at: &mut Option<Instant>,
) -> Result<Option<String>, Broken> {
    let current = refs.load(Ordering::Acquire);
    if *done == Some(current) && retry_at.is_none_or(|at| Instant::now() < at) {
        return Ok(None);
    }
    if !welcome.capabilities.iter().any(|c| c == mirror::CAPABILITY) {
        *done = Some(current);
        *retry_at = None;
        return Ok(Some(format!(
            "{} does not accept the repository mirror: enable git-mirror there and set its remote",
            host.label()
        )));
    }
    let _ = stream.set_read_timeout(Some(mirror::FETCH_LIMIT + Duration::from_secs(60)));
    let answer = ask(stream, &HostReq::MirrorFetch, None);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    *done = Some(current);
    match answer? {
        HostResp::Mirrored { ff } => {
            *retry_at = None;
            Ok(mirror::note(&host.label(), &ff))
        }
        HostResp::Error { text } => {
            *retry_at = Some(Instant::now() + MIRROR_RETRY);
            Ok(Some(format!("{} repository mirror: {text}", host.label())))
        }
        other => Err(unexpected(&other)),
    }
}

pub fn run(host: &HostConfig, key: &str, tx: &Sender<Ev>, stop: &AtomicBool, hub: &HubCtx) {
    let mut update_failed = false;
    let mut update_note = None;
    let mut mirror_note = None;
    let mut login_notes: [Option<String>; 2] = [None, None];
    let agents = host.runs_agents();
    let down = |reason: String| agents || report(tx, HostEv::Down { tag: host.tag, reason });
    while !stop.load(Ordering::SeqCst) {
        let (mut stream, welcome) = match hosts::open(host, key, Channel::Host) {
            Ok(open) => open,
            Err(error) => {
                if !down(short(&error)) {
                    return;
                }
                pause(stop, Duration::from_secs(5));
                continue;
            }
        };
        if !update_failed {
            let (config, error) = Config::load();
            let policy = if error.is_none() { config.refresh.as_ref() } else { None };
            match update_host(host, &mut stream, &welcome, policy) {
                Ok(false) => {}
                Ok(true) => {
                    let _ = stream.shutdown(Shutdown::Both);
                    pause(stop, Duration::from_secs(5));
                    continue;
                }
                Err(err) => {
                    // Tried once per hub build; the host keeps its own.
                    say(
                        tx,
                        host.tag,
                        &mut update_note,
                        Some(format!("Could not bring {} up to this build: {err}", host.label())),
                    );
                    update_failed = true;
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
            }
        }
        let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
        let mut next_logins = Instant::now();
        // Every new connection fetches once: the host may have missed changes.
        let (mut mirrored, mut mirror_retry) = (None, None);
        while !stop.load(Ordering::SeqCst) {
            match ask(&mut stream, &HostReq::Stats { resources_only: !agents }, None) {
                Ok(HostResp::Stats { stats }) => {
                    if !report(tx, HostEv::Stats { tag: host.tag, stats }) {
                        return;
                    }
                }
                Ok(other) => {
                    let _ = down(unexpected(&other));
                    break;
                }
                Err(reason) => {
                    let _ = down(reason);
                    break;
                }
            }
            if !agents {
                pause(stop, STATS_EVERY);
                continue;
            }
            let (config, error) = Config::load();
            if error.is_none() {
                if config.plugin_enabled("git-mirror") {
                    match mirror_round(host, &mut stream, &welcome, &hub.refs, &mut mirrored, &mut mirror_retry) {
                        Ok(note) => say(tx, host.tag, &mut mirror_note, note),
                        Err(_) => break,
                    }
                }
                if hub.sync_logins
                    && config.sync_logins
                    && config.plugin_enabled("login-sync")
                    && Instant::now() >= next_logins
                {
                    match logins::Settings::from_config(&config) {
                        Ok(settings) => {
                            next_logins = Instant::now() + Duration::from_secs(settings.interval_seconds);
                            match logins::sync_with_settings(host, &mut stream, hub, &settings, welcome.version) {
                                Ok(notes) => {
                                    for (tool, note) in notes {
                                        let i = logins::TOOLS.iter().position(|t| *t == tool).unwrap_or(0);
                                        say_login(tx, host.tag, &mut login_notes[i], note);
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        Err(error) => say_login(tx, host.tag, &mut login_notes[0], Some(error.to_string())),
                    }
                }
            }
            pause(stop, STATS_EVERY);
        }
        let _ = stream.shutdown(Shutdown::Both);
        pause(stop, Duration::from_secs(5));
    }
}

/// Bring the host up to this hub's build when it runs an older one. Its
/// listener then restarts on the new build, closing this connection.
fn update_host(
    host: &HostConfig,
    stream: &mut UnixStream,
    welcome: &Welcome,
    policy: Option<&crate::config::RefreshPolicy>,
) -> Result<bool, String> {
    if !policy.is_some_and(|policy| policy.allows(&host.name)) {
        return Ok(false);
    }
    let ours = hosts::this_build();
    if !hosts::host_needs_update(ours, welcome.version, &welcome.build, welcome.built) {
        return Ok(false);
    }
    let bytes = std::fs::read(hosts::RUNNING_PROGRAM).map_err(|e| format!("reading this build: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let req = HostReq::Update { len: bytes.len() as u64, sha256: ours.0.clone(), built: ours.1 };
    match ask(stream, &req, Some(&bytes))? {
        HostResp::Updated => {
            eprintln!("cepheus: brought {} up to this build", host.name);
            Ok(true)
        }
        HostResp::Error { text } => Err(text),
        other => Err(unexpected(&other)),
    }
}

#[cfg(test)]
#[path = "../tests/hosts_upkeep.rs"]
mod tests;

// Compatibility export keeps existing callers on the owning plugin.
#[cfg(test)]
pub use crate::hosts::logins::sync_logins;
