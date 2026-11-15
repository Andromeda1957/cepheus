//! One-way repository mirror from the hub to its agent hosts.
//!
//! The hub watches its repository's branch and tag refs with inotify and, once
//! they have settled, asks each agent host to fetch. The host fetches with
//! ordinary `git fetch` from a remote named in its own configuration, into
//! `refs/remotes/<remote>/*`, then fast-forwards its `master` only when that is
//! safe. Nothing here pushes, merges, rebases or touches uncommitted work; a
//! held or diverged `master` is reported. Retrieve Git LFS payloads separately.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Advertised by a host whose configuration names its mirror remote.
pub const CAPABILITY: &str = "git-mirror-v1";
pub const MASTER: &str = "refs/heads/master";
/// The longest a host's fetch may run; the hub waits a little longer.
pub const FETCH_LIMIT: Duration = Duration::from_secs(600);
/// Refs must be quiet this long before hosts are asked to fetch.
const QUIET: Duration = Duration::from_secs(2);
/// A busy repository is still mirrored at least this often.
const LONGEST_WAIT: Duration = Duration::from_secs(10);

/// The receiving host's policy: the Git remote it fetches the hub's
/// repository from. The hub never supplies a remote, URL or refspec.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub remote: String,
}
impl Settings {
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        let value = config
            .plugin_settings
            .get("git-mirror")
            .context("git-mirror needs plugin_settings.git-mirror.remote on this host")?;
        let settings: Self = serde_json::from_value(value.clone()).context("invalid git-mirror settings")?;
        let name = &settings.remote;
        if name.is_empty()
            || name.len() > 64
            || name.starts_with(['-', '.'])
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            bail!("git-mirror remote must be a plain Git remote name");
        }
        Ok(settings)
    }
}

/// What a fast-forward of `master` did.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Ff {
    UpToDate,
    Forwarded {
        from: String,
        to: String,
    },
    /// `master` has commits the hub's lacks.
    Ahead,
    /// Each side has commits the other lacks: a merge is needed.
    Diverged,
    /// It could have moved but was left: why.
    Held {
        why: String,
    },
}

/// The line the owner sees about a host's mirror, none when all is well.
pub fn note(label: &str, ff: &Ff) -> Option<String> {
    match ff {
        Ff::UpToDate | Ff::Forwarded { .. } => None,
        Ff::Ahead => Some(format!("{label}'s master has commits this machine's lacks; integrate them here")),
        Ff::Diverged => Some(format!("{label}'s master has diverged from this machine's; it was left unchanged")),
        Ff::Held { why } => Some(format!("{label}'s master was not fast-forwarded: {why}")),
    }
}

fn git_command(repo: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    // The configured checkout is authoritative even if the service was
    // launched by a Git hook or a client carrying repository-routing variables.
    for variable in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_PREFIX",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ] {
        cmd.env_remove(variable);
    }
    cmd.arg("-C").arg(repo).args(args).stdin(Stdio::null());
    // The start directory must be the checkout itself, never a directory
    // inside some other repository that git would find above it.
    if let Some(parent) = repo.parent() {
        cmd.env("GIT_CEILING_DIRECTORIES", parent);
    }
    // Never stop to ask for anything: there is nobody at this terminal.
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .env("GIT_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no");
    cmd
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = git_command(repo, args).output().context("running git")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed");
        bail!("git {}: {first}", args.first().copied().unwrap_or(""));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_ok(repo: &Path, args: &[&str]) -> bool {
    git_command(repo, args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// The commit `name` points at.
pub fn rev(repo: &Path, name: &str) -> Option<String> {
    git(repo, &["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")]).ok().filter(|s| s.len() >= 40)
}

fn is_ancestor(repo: &Path, ancestor: &str, of: &str) -> bool {
    git_ok(repo, &["merge-base", "--is-ancestor", ancestor, of])
}

/// Fetch the hub's branches and tags from `remote`, then fast-forward
/// `master` when safe. Branches the hub deleted leave `refs/remotes/<remote>/`
/// too; this host's own branches and tags are never removed.
pub fn fetch(repo: &Path, remote: &str) -> Result<Ff> {
    let tracking = format!("+refs/heads/*:refs/remotes/{remote}/*");
    let argv: Vec<String> = ["fetch", "--quiet", "--prune", "--tags", "--no-write-fetch-head", remote, &tracking]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut child = git_command(repo, &argv.iter().map(String::as_str).collect::<Vec<_>>())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git fetch")?;
    let mut stderr = child.stderr.take().context("git fetch output")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let Some(status) = wait_limited(&mut child, FETCH_LIMIT).context("waiting for git fetch")? else {
        bail!("git fetch from {remote} took longer than {} seconds", FETCH_LIMIT.as_secs());
    };
    let err = reader.join().unwrap_or_default();
    if !status.success() {
        let first = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed");
        bail!("git fetch from {remote}: {first}");
    }
    Ok(fast_forward(repo, &format!("refs/remotes/{remote}/master")))
}

/// Wait for `child` to exit, blocked in the kernel on its pidfd; after
/// `limit` it is killed and None returned.
fn wait_limited(child: &mut Child, limit: Duration) -> std::io::Result<Option<ExitStatus>> {
    // SAFETY: the child is reaped only by `wait` below, so its pid is its own.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pidfd_open returned a new descriptor this OwnedFd then owns.
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let deadline = Instant::now() + limit;
    loop {
        let mut poll = libc::pollfd { fd: pidfd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let left = deadline.saturating_duration_since(Instant::now());
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut poll, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            child.kill()?;
            child.wait()?;
            return Ok(None);
        }
        return child.wait().map(Some);
    }
}

/// Move `master` forward to `target` when that is all it takes, never
/// touching uncommitted work or an operation in progress.
pub fn fast_forward(repo: &Path, target: &str) -> Ff {
    let held = |why: String| Ff::Held { why };
    let (Some(ours), Some(theirs)) = (rev(repo, MASTER), rev(repo, target)) else {
        return held("there is no master to move".into());
    };
    if ours == theirs || is_ancestor(repo, &theirs, &ours) {
        return if ours == theirs { Ff::UpToDate } else { Ff::Ahead };
    }
    if !is_ancestor(repo, &ours, &theirs) {
        return Ff::Diverged;
    }
    let checked_out_here = git(repo, &["symbolic-ref", "-q", "HEAD"]).ok().as_deref() == Some(MASTER);
    if !checked_out_here {
        if let Some(path) = checked_out_elsewhere(repo) {
            return held(format!("master is checked out in {path}"));
        }
        // Nothing has master checked out: move the branch alone, and only
        // from the commit just read.
        return match git(repo, &["update-ref", "-m", "cepheus: master mirror", MASTER, &theirs, &ours]) {
            Ok(_) => Ff::Forwarded { from: ours, to: theirs },
            Err(err) => held(format!("{err:#}")),
        };
    }
    if let Some(op) = operation_in_progress(repo) {
        return held(format!("a {op} is in progress"));
    }
    // Even non-overlapping edits belong to the active checkout. A background
    // mirror leaves the entire dirty tree alone.
    match git(repo, &["status", "--porcelain", "--untracked-files=normal"]) {
        Ok(status) if status.is_empty() => {}
        Ok(_) => return held("the checkout has uncommitted work".into()),
        Err(error) => return held(error.to_string()),
    }
    match git(repo, &["merge", "--ff-only", "--no-overwrite-ignore", "-q", &theirs]) {
        Ok(_) => Ff::Forwarded { from: ours, to: theirs },
        Err(err) => held(format!("{err:#}")),
    }
}

fn operation_in_progress(repo: &Path) -> Option<&'static str> {
    // Read the operation markers used by `git status`. Exclude REBASE_HEAD because a finished or abandoned
    // rebase may leave it behind.
    let markers = [
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("BISECT_LOG", "bisect"),
    ];
    markers.iter().find_map(|(marker, op)| {
        let path = git(repo, &["rev-parse", "--git-path", marker]).ok()?;
        let path = if Path::new(&path).is_absolute() { Path::new(&path).to_path_buf() } else { repo.join(path) };
        path.exists().then_some(*op)
    })
}

fn checked_out_elsewhere(repo: &Path) -> Option<String> {
    let list = git(repo, &["worktree", "list", "--porcelain"]).ok()?;
    let mut path = None;
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p.to_string());
        } else if line == format!("branch {MASTER}") {
            return path;
        }
    }
    None
}

/// The repository's shared Git directory, read from disk without running git:
/// `.git` itself, or for a linked worktree the `commondir` its `gitdir` names.
pub fn common_dir(repo: &Path) -> Result<PathBuf> {
    let dot = repo.join(".git");
    if dot.is_dir() {
        return Ok(dot);
    }
    let text = fs::read_to_string(&dot).with_context(|| format!("{} is not a Git checkout", repo.display()))?;
    let gitdir = text.trim().strip_prefix("gitdir:").context("unreadable .git file")?.trim();
    let gitdir = repo.join(gitdir);
    match fs::read_to_string(gitdir.join("commondir")) {
        Ok(common) => Ok(gitdir.join(common.trim())),
        Err(_) => Ok(gitdir),
    }
}

/// Start watching `repo`'s branch and tag refs. `generation` grows by one each
/// time they change and then stay quiet for `QUIET` (or after `LONGEST_WAIT`
/// of continuous change). The watches are set up here, so a setup failure is
/// returned; the thread itself blocks in the kernel between changes.
pub fn watch(repo: &Path, generation: Arc<AtomicU64>) -> Result<()> {
    let mut refs = RefWatch::new(&common_dir(repo)?)?;
    std::thread::Builder::new()
        .name("git-mirror-refs".into())
        .spawn(move || {
            if let Err(error) = refs.run(&generation) {
                eprintln!("cepheus: the repository mirror stopped watching refs: {error:#}");
            }
        })
        .context("starting the ref watcher")?;
    Ok(())
}

const DIR_MASK: u32 = libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_ONLYDIR;

pub struct RefWatch {
    inotify: File,
    root: i32,
    dirs: HashMap<i32, PathBuf>,
}
impl RefWatch {
    pub fn new(common: &Path) -> Result<Self> {
        // SAFETY: inotify_init1 returns a new descriptor this File then owns.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("inotify");
        }
        let inotify = unsafe { File::from_raw_fd(fd) };
        let mut watch = Self { inotify, root: -1, dirs: HashMap::new() };
        // `packed-refs` is replaced by renaming its lock file over it.
        watch.root = watch.add(common, libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_ONLYDIR)?;
        for sub in ["refs/heads", "refs/tags"] {
            watch.add_tree(&common.join(sub))?;
        }
        Ok(watch)
    }

    fn add(&self, path: &Path, mask: u32) -> Result<i32> {
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        // SAFETY: a valid descriptor and NUL-terminated path.
        let wd = unsafe { libc::inotify_add_watch(self.inotify.as_raw_fd(), name.as_ptr(), mask) };
        if wd < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| format!("watching {}", path.display()));
        }
        Ok(wd)
    }

    fn add_tree(&mut self, dir: &Path) -> Result<()> {
        let wd = self.add(dir, DIR_MASK)?;
        self.dirs.insert(wd, dir.to_path_buf());
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                self.add_tree(&entry.path())?;
            }
        }
        Ok(())
    }

    /// Wait up to `timeout` (forever when none) for events; true when any of
    /// them changed a ref.
    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<bool> {
        let mut poll = libc::pollfd { fd: self.inotify.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let millis = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut poll, 1, millis) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error).context("waiting for ref changes");
        }
        if ready == 0 {
            return Ok(false);
        }
        let mut buffer = [0u8; 16 * 1024];
        let len = self.inotify.read(&mut buffer).context("reading ref changes")?;
        let mut changed = false;
        let mut new_dirs = Vec::new();
        let mut offset = 0;
        let header = std::mem::size_of::<libc::inotify_event>();
        while offset + header <= len {
            // SAFETY: the kernel wrote whole events; read the header unaligned.
            let event: libc::inotify_event =
                unsafe { std::ptr::read_unaligned(buffer[offset..].as_ptr() as *const libc::inotify_event) };
            let name_bytes = &buffer[offset + header..offset + header + event.len as usize];
            let name = &name_bytes[..name_bytes.iter().position(|b| *b == 0).unwrap_or(name_bytes.len())];
            offset += header + event.len as usize;
            if event.mask & libc::IN_Q_OVERFLOW != 0 {
                changed = true;
                continue;
            }
            if event.mask & libc::IN_IGNORED != 0 {
                if event.wd == self.root {
                    bail!("the repository's Git directory went away");
                }
                self.dirs.remove(&event.wd);
                continue;
            }
            if event.wd == self.root {
                changed |= name == b"packed-refs";
                continue;
            }
            let Some(dir) = self.dirs.get(&event.wd) else { continue };
            if event.mask & libc::IN_ISDIR != 0 {
                if event.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                    new_dirs.push(dir.join(std::ffi::OsStr::from_bytes(name)));
                }
                changed = true;
            } else if !name.ends_with(b".lock") {
                changed = true;
            }
        }
        for dir in new_dirs {
            match self.add_tree(&dir) {
                Ok(()) => {}
                // A directory already gone again needs no watch.
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(changed)
    }

    fn run(&mut self, generation: &AtomicU64) -> Result<()> {
        loop {
            if !self.wait(None)? {
                continue;
            }
            let first = Instant::now();
            let mut last = first;
            loop {
                let quiet = QUIET.saturating_sub(last.elapsed());
                let longest = LONGEST_WAIT.saturating_sub(first.elapsed());
                let left = quiet.min(longest);
                if left.is_zero() {
                    break;
                }
                if self.wait(Some(left))? {
                    last = Instant::now();
                }
            }
            generation.fetch_add(1, Ordering::Release);
        }
    }
}

#[cfg(test)]
#[path = "../../src/tests/hosts_mirror.rs"]
mod tests;
