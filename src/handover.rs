//! Updating the service in place. and "make it so the service just auto updates in place".
//!
//! When its binary is replaced (`cepheus install` renames a new one over it), the service runs the new binary
//! in its own process with `exec`. The process id stays the same, so every hosted program stays its child; each
//! session's terminal descriptor, the listening socket and the single-instance lock survive `exec`. This module
//! holds the state the old build writes for the new one (`Handover`), how a replaced binary is found, and the
//! check that the new binary accepts the state before anything is handed over.

use std::fs;
use std::io::Read;
use std::os::fd::RawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::monitor::SessionFacts;
use crate::protocol::{LogEntry, Tool, Usage};
use crate::status::Tracker;
use crate::util;

/// Bumped whenever `Handover` changes in a way an older or newer build
/// cannot read; the new binary then refuses the handover and the old build
/// keeps running.
fn unknown_draft() -> bool {
    true
}

pub const HANDOVER_VERSION: u32 = 2;

/// How long the new binary gets to accept the handover file.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything the next build needs to carry on where this one stops.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Handover {
    pub version: u32,
    /// The listening socket and the single-instance lock, both inherited.
    pub listener_fd: RawFd,
    pub lock_fd: RawFd,
    pub view: (u16, u16),
    pub errors: Vec<LogEntry>,
    pub usage: Usage,
    pub dev: Option<DevHandover>,
    pub sessions: Vec<SessionHandover>,
    #[serde(default)]
    pub scheduler: crate::scheduler::Saved,
    #[serde(default)]
    pub scheduler_fds: Vec<(u64, RawFd)>,
    #[serde(default)]
    pub extensions: ExtensionHandover,
    /// Connection outages keep their age and deduplication across hub updates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_outages: Vec<HostOutageHandover>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct HostOutageHandover {
    pub config: crate::config::HostConfig,
    pub down_for_ms: u64,
    pub reported: bool,
    pub reason: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
pub struct ExtensionHandover {
    #[serde(default)]
    pub processes: Vec<crate::extensions::ProcessSnapshot>,
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<u64, std::collections::BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub titles: std::collections::BTreeMap<u64, (String, String)>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct DevHandover {
    pub pid: u32,
    pub started_at: u64,
    pub log: PathBuf,
    pub stopping: bool,
}

/// One running session: its program, terminal and what the deck knows of it.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct SessionHandover {
    #[serde(default)]
    pub account_root: Option<PathBuf>,
    #[serde(default)]
    pub account: Option<String>,
    pub id: u64,
    pub tool: Tool,
    pub pid: u32,
    /// The terminal's master descriptor, inherited.
    pub fd: RawFd,
    pub cols: u16,
    pub rows: u16,
    /// Escape sequences that redraw its screen and scrollback.
    pub screen: String,
    pub conversation: Option<String>,
    pub term_title: Option<String>,
    pub title: String,
    pub created_at: u64,
    pub spawned_ms: u64,
    /// Input custody survives replacement; older state cannot attest it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<u64>,
    pub tracker: Tracker,
    pub facts: SessionFacts,
    pub remove_when_ended: bool,
    /// Automatic "continue" bookkeeping, as milliseconds before the handover.
    pub retry_seen_ago_ms: Option<u64>,
    pub retry_sent_ago_ms: Vec<u64>,
    pub retry_gave_up: bool,
    #[serde(default)]
    pub retry_quota_reset: Option<u64>,
    #[serde(default)]
    pub retry_quota_unknown: bool,
    /// Older state cannot attest the absence of an unsubmitted owner draft.
    #[serde(default = "unknown_draft")]
    pub owner_draft: bool,
    /// Conversations it has left; a build before this field sends none.
    #[serde(default)]
    pub left: Vec<String>,
    /// Its program's update notice was already announced (`Sess::update_notice`),
    /// so the next build does not run another update for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub update_notice: bool,
    /// The owner's name for its chat (`:rename`); a build before this field
    /// sends none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Where it runs, when not the start directory, the command a terminal
    /// session runs, and the job it belongs to (a security scan); a build
    /// before these fields sends none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// Full input and bounded retry state survive binary replacement.
    #[serde(default)]
    pub recovery: crate::service::recovery::ClientRecovery,
}

impl Handover {
    /// Write the state privately for the next build.
    pub fn write(&self, path: &Path) -> Result<()> {
        let data = serde_json::to_vec(self)?;
        util::write_atomic(path, &data).with_context(|| format!("writing {}", path.display()))
    }

    /// Read state written by the previous build, refusing another version.
    pub fn read(path: &Path) -> Result<Handover> {
        let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let value: serde_json::Value = serde_json::from_slice(&data).context("the handover is not JSON")?;
        let version = value.get("version").and_then(serde_json::Value::as_u64).unwrap_or(0);
        if version != 1 && version != u64::from(HANDOVER_VERSION) {
            bail!("handover version {version}; this build reads version {HANDOVER_VERSION}");
        }
        serde_json::from_value(value).context("reading the handover")
    }

    /// Read the state and remove its file, which holds screen contents.
    pub fn take(path: &Path) -> Result<Handover> {
        let handover = Handover::read(path);
        let _ = fs::remove_file(path);
        handover
    }
}

/// Where this process writes its handover: the private runtime directory
/// that holds the socket.
pub fn handover_path(socket: &Path) -> PathBuf {
    let dir = socket.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("handover-{}.json", std::process::id()))
}

/// A binary file as found on disk, to tell one install from the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinaryId {
    pub dev: u64,
    pub ino: u64,
    pub mtime: i64,
}

/// The binary that replaced this running one, from the link the kernel keeps
/// to a program whose file was replaced: `/path/cepheus (deleted)`.
pub fn replaced_binary() -> Option<(PathBuf, BinaryId)> {
    let link = fs::read_link("/proc/self/exe").ok()?;
    replacement_of(&link)
}

/// The executable now at the path a deleted program `link` ran from.
pub fn replacement_of(link: &Path) -> Option<(PathBuf, BinaryId)> {
    binary_at(Path::new(link.to_str()?.strip_suffix(" (deleted)")?))
}

/// The executable at `path`, for a restart that runs the program again.
pub fn binary_at(path: &Path) -> Option<(PathBuf, BinaryId)> {
    let path = path.to_path_buf();
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return None;
    }
    Some((path, BinaryId { dev: meta.dev(), ino: meta.ino(), mtime: meta.mtime() }))
}

/// The line `service --check-handover` prints for state it can take over.
pub fn accepted_line() -> String {
    format!("ok {HANDOVER_VERSION}")
}

/// `cepheus service --check-handover <file>`: whether this build can take
/// over the state in `file`.
pub fn check_command(file: &Path) -> i32 {
    match Handover::read(file) {
        Ok(snapshot) => {
            println!("ok {}", snapshot.version);
            0
        }
        Err(err) => {
            eprintln!("cepheus: {err:#}");
            1
        }
    }
}

/// Ask the new binary `exe` whether it can take over the state in `file`.
pub fn check(exe: &Path, file: &Path) -> Result<()> {
    let mut child = Command::new(exe)
        .arg("service")
        .arg("--check-handover")
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", exe.display()))?;
    let deadline = Instant::now() + CHECK_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} did not answer within {}s", exe.display(), CHECK_TIMEOUT.as_secs());
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut err);
    }
    if status.success() && out.trim() == accepted_line() {
        return Ok(());
    }
    let why = util::one_line(if err.trim().is_empty() { out.trim() } else { err.trim() });
    bail!("the new build cannot take over ({}): {why}", if why.is_empty() { "no answer" } else { "refused" })
}

#[cfg(test)]
#[path = "tests/handover.rs"]
mod tests;
