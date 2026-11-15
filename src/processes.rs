//! Process-group supervision shared by service plugins.
//! Children have their own process group, private logs and a graceful stop.
//! Only a process started by this service (or inherited across its exec) is owned.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::util;

/// How long a stopping server gets before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// The server is waited for by process id rather than through a `Child`, so
/// the service's next build can take over a running server after an update
/// (`ManagedProcess::adopt`): it stays this process's child across `exec`.
pub struct ManagedProcess {
    pub pid: u32,
    pub started_at: u64,
    started: Instant,
    stopping: Option<Instant>,
    killed: bool,
    pub log: PathBuf,
}

/// How a server that is no longer running ended.
#[derive(Debug)]
pub enum Ended {
    /// Stopped from the deck.
    Stopped,
    /// Exited by itself: its status, run time and the end of its log.
    Failed { status: ExitStatus, ran: Duration, tail: String },
}

/// The port a server command listens on: the port of its last `host:port`
/// or bare-number argument (`runserver 0.0.0.0:8000`, `--port 9000`).
pub fn port_of(argv: &[String]) -> Option<u16> {
    argv.iter().skip(1).rev().find_map(|arg| {
        let tail = arg.rsplit(':').next().unwrap_or(arg);
        if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        tail.parse::<u16>().ok().filter(|p| *p > 0)
    })
}

/// Whether a `/proc/net/tcp`-format table has a listening socket on `port`.
pub fn table_listens(table: &str, port: u16) -> bool {
    table.lines().skip(1).any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (Some(local), Some(state)) = (fields.get(1), fields.get(3)) else { return false };
        // State 0A is TCP_LISTEN.
        *state == "0A" && local.rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok()) == Some(port)
    })
}

/// Whether anything on this machine listens on TCP `port`, read from the
/// kernel's socket tables without connecting to it.
pub fn port_listening(port: u16) -> bool {
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .any(|path| fs::read_to_string(path).is_ok_and(|table| table_listens(&table, port)))
}

/// The last `lines` lines of a log file, read from at most its last 64 KiB.
pub fn log_tail(path: &Path, lines: usize) -> String {
    let Ok(mut file) = fs::File::open(path) else { return String::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(64 << 10)));
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Reap child `pid`: its status once it has ended, `None` while it runs
/// (`block` waits for it instead).
fn reap(pid: u32, block: bool) -> std::io::Result<Option<ExitStatus>> {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: waitpid writes the status of our own child into `status`.
        let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, if block { 0 } else { libc::WNOHANG }) };
        return match rc {
            0 => Ok(None),
            rc if rc > 0 => Ok(Some(ExitStatus::from_raw(status))),
            _ => {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                Err(err)
            }
        };
    }
}

impl ManagedProcess {
    /// Start `argv` in `cwd` in a new session and process group, with its
    /// output replacing the previous run's log.
    pub fn start(argv: &[String], cwd: &Path, log: &Path, env_remove: &[String]) -> Result<ManagedProcess> {
        let program = argv.first().context("no process command is configured")?;
        let out = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(log)
            .with_context(|| format!("opening {}", log.display()))?;
        // `.venv/bin/python` names a file in `cwd`, not in the service's own
        // directory; a bare name is looked up on PATH.
        let resolved = if program.contains('/') { cwd.join(program) } else { PathBuf::from(program) };
        let mut cmd = Command::new(&resolved);
        cmd.args(&argv[1..]).current_dir(cwd).stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
        for name in env_remove {
            cmd.env_remove(name);
        }
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().with_context(|| format!("starting {program} in {}", cwd.display()))?;
        Ok(ManagedProcess {
            pid: child.id(),
            started_at: util::now_secs(),
            started: Instant::now(),
            stopping: None,
            killed: false,
            log: log.to_path_buf(),
        })
    }

    /// Take over a server the previous build of the service started; it is
    /// still this process's child. A stop under way gets a fresh grace period.
    pub fn adopt(pid: u32, started_at: u64, log: PathBuf, stopping: bool) -> ManagedProcess {
        let ran = Duration::from_secs(util::now_secs().saturating_sub(started_at));
        let now = Instant::now();
        ManagedProcess {
            pid,
            started_at,
            started: now.checked_sub(ran).unwrap_or(now),
            stopping: stopping.then_some(now),
            killed: false,
            log,
        }
    }

    pub fn stopping(&self) -> bool {
        self.stopping.is_some()
    }

    /// Ask the whole process group to stop, as Ctrl-C would.
    pub fn stop(&mut self) {
        if self.stopping.is_none() {
            self.stopping = Some(Instant::now());
            self.signal(libc::SIGINT);
            self.signal(libc::SIGTERM);
        }
    }

    /// End the whole group now and reap the server, as the service exits.
    pub fn kill(&mut self) {
        self.signal(libc::SIGKILL);
        let _ = reap(self.pid, true);
    }

    fn signal(&self, sig: libc::c_int) {
        // SAFETY: plain syscall on the group this process leads.
        unsafe {
            libc::killpg(self.pid as libc::pid_t, sig);
        }
    }

    /// Reap the server if it ended, killing a stopping one that outlives
    /// its grace period.
    pub fn poll(&mut self, now: Instant) -> Option<Ended> {
        match reap(self.pid, false) {
            Ok(Some(status)) => {
                // The autoreloader's child shares the group; end it with its parent.
                self.signal(libc::SIGKILL);
                Some(if self.stopping.is_some() {
                    Ended::Stopped
                } else {
                    Ended::Failed { status, ran: self.started.elapsed(), tail: log_tail(&self.log, 12) }
                })
            }
            Ok(None) => {
                if let Some(at) = self.stopping {
                    if !self.killed && now.duration_since(at) > STOP_GRACE {
                        self.killed = true;
                        self.signal(libc::SIGKILL);
                    }
                }
                None
            }
            Err(_) => Some(Ended::Stopped),
        }
    }
}

#[cfg(test)]
#[path = "tests/devserver.rs"]
mod tests;
