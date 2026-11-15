//! Running a helper program the deck depends on (git, an agent's own CLI)
//! with a time limit and a cap on what it prints. Commands are argument
//! lists, never a shell line; the program leads a process group of its own
//! so a time limit ends whatever it started too (`git clone` runs
//! `git-remote-https`).

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
thread_local! {static CANCELLATION:std::cell::RefCell<Option<Arc<AtomicBool>>>=const {std::cell::RefCell::new(None)};}
struct CancelScope(Option<Arc<AtomicBool>>);
impl Drop for CancelScope {
    fn drop(&mut self) {
        CANCELLATION.with(|current| {
            current.replace(self.0.take());
        });
    }
}
/// Propagate service shutdown through nested native helper invocations on this worker.
pub fn with_cancellation<T>(token: Arc<AtomicBool>, work: impl FnOnce() -> T) -> T {
    let previous = CANCELLATION.with(|current| current.replace(Some(token)));
    let _guard = CancelScope(previous);
    work()
}
fn cancelled() -> bool {
    CANCELLATION.with(|current| current.borrow().as_ref().is_some_and(|token| token.load(Ordering::SeqCst)))
}

/// The most of each stream kept.
pub const OUTPUT_CAP: usize = 256 * 1024;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Ran {
    /// Its exit status; none when it was ended by a signal or the limit.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// The last line it printed about a failure, for a one-line report.
    pub fn reason(&self) -> String {
        if self.timed_out {
            return "it took too long and was stopped".to_string();
        }
        let last = |s: &str| s.lines().rev().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string());
        last(&self.stderr).or_else(|| last(&self.stdout)).unwrap_or_else(|| match self.code {
            Some(code) => format!("it exited with status {code}"),
            None => "it was ended by a signal".to_string(),
        })
    }
}

/// Keep draining after the retained output reaches its cap, without letting a
/// noisy stream postpone cancellation or the other stream indefinitely.
struct Output<R> {
    pipe: R,
    kept: Vec<u8>,
    eof: bool,
}

impl<R: Read + AsRawFd> Output<R> {
    fn new(pipe: R) -> io::Result<Self> {
        // SAFETY: the pipe owns a valid descriptor; fcntl only changes its flags.
        unsafe {
            let flags = libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL);
            if flags < 0 || libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { pipe, kept: Vec::new(), eof: false })
    }

    fn read_available(&mut self) -> io::Result<bool> {
        if self.eof {
            return Ok(false);
        }
        let mut progress = false;
        let mut buf = [0u8; 8192];
        for _ in 0..8 {
            match self.pipe.read(&mut buf) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => {
                    let room = OUTPUT_CAP.saturating_sub(self.kept.len());
                    self.kept.extend_from_slice(&buf[..n.min(room)]);
                    progress = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(progress)
    }

    fn into_text(self) -> String {
        String::from_utf8_lossy(&self.kept).into_owned()
    }
}

fn stop(child: &mut Child) {
    // SAFETY: this command started its own process group. End its descendants
    // even when its leader has exited while a descendant still holds a pipe.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    // Also stop the direct child if it moved out of that process group.
    let _ = child.kill();
    let _ = child.wait();
}

/// Run `argv` in `cwd` (or here), with no input, for at most `limit`.
pub fn run(
    argv: &[String],
    cwd: Option<&std::path::Path>,
    env: &[(String, String)],
    limit: Duration,
) -> io::Result<Ran> {
    run_with(argv, cwd, env, limit, None, None)
}

/// The longest line `stream` hands on; a longer one is skipped whole.
pub const LINE_CAP: usize = 4 * 1024 * 1024;

/// `run`, handing each line `argv` prints to `line` as it arrives and ending
/// it, and whatever it started, as soon as `stop` is set. Its output is not
/// kept (`Ran::stdout` is empty); its errors are, for `Ran::reason`.
pub fn stream(
    argv: &[String],
    cwd: Option<&std::path::Path>,
    env: &[(String, String)],
    limit: Duration,
    stop: &AtomicBool,
    line: &mut dyn FnMut(&str),
) -> io::Result<Ran> {
    let mut pending: Vec<u8> = Vec::new();
    let mut skipping = false;
    let mut split = |bytes: &[u8]| {
        for chunk in bytes.split_inclusive(|b| *b == b'\n') {
            if !skipping {
                pending.extend_from_slice(chunk);
                if pending.len() > LINE_CAP {
                    pending.clear();
                    skipping = true;
                }
            }
            if chunk.ends_with(b"\n") {
                if !skipping {
                    line(String::from_utf8_lossy(&pending).trim_end_matches(['\n', '\r']));
                }
                pending.clear();
                skipping = false;
            }
        }
    };
    let ran = run_with(argv, cwd, env, limit, Some(stop), Some(&mut split))?;
    if !skipping && !pending.is_empty() {
        line(String::from_utf8_lossy(&pending).trim_end_matches('\r'));
    }
    Ok(ran)
}

fn run_with(
    argv: &[String],
    cwd: Option<&std::path::Path>,
    env: &[(String, String)],
    limit: Duration,
    halt: Option<&AtomicBool>,
    mut output: Option<&mut dyn FnMut(&[u8])>,
) -> io::Result<Ran> {
    let stopped = || cancelled() || halt.is_some_and(|halt| halt.load(Ordering::SeqCst));
    if stopped() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "service is stopping"));
    }
    let (program, args) = argv.split_first().ok_or_else(|| io::Error::other("no program to run"))?;
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn()?;
    let result = (|| {
        let mut out = Output::new(child.stdout.take().expect("piped stdout"))?;
        let mut err = Output::new(child.stderr.take().expect("piped stderr"))?;
        let deadline = Instant::now() + limit;
        // What a streamed program printed goes on as it comes, never kept.
        let mut pass_on = |out: &mut Output<_>| {
            if let Some(output) = output.as_mut() {
                output(&out.kept);
                out.kept.clear();
            }
        };
        loop {
            let stopping = stopped();
            if stopping || Instant::now() >= deadline {
                stop(&mut child);
                // One bounded final read; an escaped descendant may still hold
                // a writer, so neither EOF nor a reader thread is waited on.
                out.read_available()?;
                pass_on(&mut out);
                err.read_available()?;
                return Ok(Ran { code: None, stdout: out.into_text(), stderr: err.into_text(), timed_out: !stopping });
            }
            let out_progress = out.read_available()?;
            pass_on(&mut out);
            let err_progress = err.read_available()?;
            if out.eof && err.eof {
                // Keep an exited leader unreaped while a descendant holds a
                // pipe. Its reserved PID prevents signalling a reused group ID
                // when cancellation later ends that original process group.
                if let Some(status) = child.try_wait()? {
                    return Ok(Ran {
                        code: status.code(),
                        stdout: out.into_text(),
                        stderr: err.into_text(),
                        timed_out: false,
                    });
                }
            }
            if !out_progress && !err_progress {
                thread::sleep(Duration::from_millis(20));
            }
        }
    })();
    if result.is_err() {
        stop(&mut child);
    }
    result
}

/// `run` for a list of string slices.
pub fn run_args(argv: &[&str], cwd: Option<&std::path::Path>, limit: Duration) -> io::Result<Ran> {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    run(&argv, cwd, &[], limit)
}

/// `git -C dir args…`, its output when it succeeds.
pub fn git(dir: &std::path::Path, args: &[&str], limit: Duration) -> Option<String> {
    let mut argv = vec!["git".to_string(), "-C".to_string(), dir.to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|s| s.to_string()));
    run(&argv, None, &[], limit).ok().filter(Ran::ok).map(|r| r.stdout)
}

#[cfg(test)]
#[path = "tests/run.rs"]
mod tests;
