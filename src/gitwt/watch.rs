//! The header's branch status and worktree count, read again only when the repository changes. One thread holds
//! an inotify watch on the checkout's HEAD, the branch refs, the linked-worktree list and the folder beside the
//! main checkout. It sleeps until one of them changes, waits for the burst to settle, then runs `branch_status`
//! once. Nothing is read on a timer.

use super::{branch_status, BranchStatus};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

/// How long a burst of changes (a commit, a checkout, a fetch) must be
/// quiet before `git` reads the result.
const SETTLE: Duration = Duration::from_millis(300);

/// The checkout's own git directory: its `HEAD`.
const HEAD: u8 = 1;
/// The shared git directory: `packed-refs` and the `worktrees` list.
const COMMON: u8 = 2;
/// A directory of branch or remote-branch refs.
const REFS: u8 = 4;
/// The shared directory's linked-worktree list.
const WORKTREES: u8 = 8;
/// The folder that holds the main checkout and its sibling worktrees.
const BESIDE: u8 = 16;

const MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_CLOSE_WRITE
    | libc::IN_ONLYDIR;

/// The latest branch status of one directory, kept by its own thread.
pub struct Watch {
    dir: PathBuf,
    readings: Receiver<Option<BranchStatus>>,
    /// Closing the write end of the pipe (dropping the watch) ends the thread.
    _stop: OwnedFd,
}

impl Watch {
    pub fn start(dir: &Path) -> io::Result<Watch> {
        let (wake, stop) = pipe()?;
        let (tx, readings) = mpsc::channel();
        let path = dir.to_path_buf();
        thread::Builder::new().name("deck-git".into()).spawn(move || run(&path, &tx, &wake))?;
        Ok(Watch { dir: dir.to_path_buf(), readings, _stop: stop })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The newest reading since the last call, if one came.
    pub fn latest(&self) -> Option<Option<BranchStatus>> {
        self.readings.try_iter().last()
    }
}

fn run(dir: &Path, tx: &Sender<Option<BranchStatus>>, stop: &OwnedFd) {
    // Watch first, so nothing that changes during the first reading is missed.
    let watcher = Watcher::new(dir);
    if tx.send(branch_status(dir)).is_err() {
        return;
    }
    // Not a repository: the one reading stands until the deck moves on.
    let Ok(mut watcher) = watcher else {
        let _ = wait(None, stop, None);
        return;
    };
    loop {
        match watcher.next(stop, None) {
            Woke::Stop => return,
            Woke::Changed => {}
            Woke::Quiet => continue,
        }
        loop {
            match watcher.next(stop, Some(SETTLE)) {
                Woke::Stop => return,
                Woke::Changed => continue,
                Woke::Quiet => break,
            }
        }
        if tx.send(branch_status(dir)).is_err() {
            return;
        }
    }
}

enum Woke {
    Stop,
    /// Something the header shows may have changed.
    Changed,
    /// The wait ran out, or only unrelated files changed.
    Quiet,
}

struct Watcher {
    fd: File,
    common: PathBuf,
    /// What each watched directory is, and where.
    watches: HashMap<i32, (u8, PathBuf)>,
}

impl Watcher {
    fn new(dir: &Path) -> io::Result<Watcher> {
        let limit = Duration::from_secs(10);
        let text =
            crate::run::git(dir, &["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir"], limit)
                .ok_or_else(|| io::Error::other("not a git checkout"))?;
        let mut lines = text.lines();
        let (Some(git_dir), Some(common)) = (lines.next(), lines.next()) else {
            return Err(io::Error::other("git named no git directory"));
        };
        // SAFETY: the flags request a new nonblocking close-on-exec instance only.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful call returned a fresh descriptor this file now owns.
        let fd = unsafe { File::from_raw_fd(fd) };
        let common = PathBuf::from(common);
        let mut watcher = Watcher { fd, common: common.clone(), watches: HashMap::new() };
        watcher.add(Path::new(git_dir), HEAD)?;
        watcher.add(&common, COMMON)?;
        for refs in ["refs/heads", "refs/remotes", "reftable"] {
            watcher.add_tree(&common.join(refs));
        }
        let _ = watcher.add(&common.join("worktrees"), WORKTREES);
        // A non-bare repository's main checkout holds `.git`; its worktrees
        // are counted beside it (`super::sibling_worktrees`).
        if common.file_name().is_some_and(|name| name == ".git") {
            if let Some(beside) = common.parent().and_then(Path::parent) {
                let _ = watcher.add(beside, BESIDE);
            }
        }
        Ok(watcher)
    }

    fn add(&mut self, path: &Path, kind: u8) -> io::Result<()> {
        let name = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: the descriptor is owned and the path is NUL-terminated.
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), name.as_ptr(), MASK) };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        self.watches.entry(wd).or_insert_with(|| (0, path.to_path_buf())).0 |= kind;
        Ok(())
    }

    /// Watch a ref directory and the ones below it (`refs/heads/agent/`).
    fn add_tree(&mut self, root: &Path) {
        let mut dirs = vec![root.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            if self.add(&dir, REFS).is_err() {
                continue;
            }
            if let Ok(entries) = fs::read_dir(&dir) {
                dirs.extend(entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()));
            }
        }
    }

    fn next(&mut self, stop: &OwnedFd, timeout: Option<Duration>) -> Woke {
        match wait(Some(&self.fd), stop, timeout) {
            Ready::Stop => Woke::Stop,
            Ready::Timeout => Woke::Quiet,
            Ready::Events => {
                if self.drain() {
                    Woke::Changed
                } else {
                    Woke::Quiet
                }
            }
        }
    }

    /// Read every waiting event; whether any can change the header.
    fn drain(&mut self) -> bool {
        let mut buffer = [0u8; 16 * 1024];
        let mut changed = false;
        let mut new_dirs = Vec::new();
        loop {
            let count = match self.fd.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut offset = 0;
            while offset + 16 <= count {
                let field = |at: usize| -> [u8; 4] { buffer[offset + at..offset + at + 4].try_into().unwrap() };
                let wd = i32::from_ne_bytes(field(0));
                let mask = u32::from_ne_bytes(field(4));
                let len = u32::from_ne_bytes(field(12)) as usize;
                let end = (offset + 16 + len).min(count);
                let raw = &buffer[offset + 16..end];
                let name = &raw[..raw.iter().position(|b| *b == 0).unwrap_or(raw.len())];
                offset = end;
                if mask & libc::IN_Q_OVERFLOW != 0 {
                    changed = true;
                    continue;
                }
                if mask & libc::IN_IGNORED != 0 {
                    self.watches.remove(&wd);
                    continue;
                }
                let Some((kind, dir)) = self.watches.get(&wd) else { continue };
                let is_dir = mask & libc::IN_ISDIR != 0;
                let appeared = mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0;
                if kind & REFS != 0 && !name.ends_with(b".lock") {
                    if is_dir && appeared {
                        new_dirs.push((dir.join(std::ffi::OsStr::from_bytes(name)), REFS));
                    }
                    changed = true;
                }
                if kind & HEAD != 0 && name == b"HEAD" {
                    changed = true;
                }
                if kind & COMMON != 0 && (name == b"packed-refs" || (name == b"worktrees" && is_dir)) {
                    if name == b"worktrees" && appeared {
                        new_dirs.push((self.common.join("worktrees"), WORKTREES));
                    }
                    changed = true;
                }
                if kind & (WORKTREES | BESIDE) != 0 && is_dir && mask & libc::IN_CLOSE_WRITE == 0 {
                    changed = true;
                }
            }
        }
        for (dir, kind) in new_dirs {
            if kind == REFS {
                self.add_tree(&dir);
            } else {
                let _ = self.add(&dir, kind);
            }
        }
        changed
    }
}

enum Ready {
    Stop,
    Timeout,
    Events,
}

/// Sleep until `events` has something to read, the watch is dropped, or
/// `timeout` passes.
fn wait(events: Option<&File>, stop: &OwnedFd, timeout: Option<Duration>) -> Ready {
    let mut fds = [
        libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: events.map_or(-1, AsRawFd::as_raw_fd), events: libc::POLLIN, revents: 0 },
    ];
    let millis = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
    loop {
        // SAFETY: both entries are valid pollfd structs for the call; a
        // negative descriptor is ignored by poll.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Ready::Stop;
        }
        if ready == 0 {
            return Ready::Timeout;
        }
        if fds[0].revents != 0 {
            return Ready::Stop;
        }
        return Ready::Events;
    }
}

/// A close-on-exec pipe: (read end, write end).
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 returns.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 succeeded, so both are fresh descriptors owned here.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}
