//! `deck.fs` and `deck.sha256`: what a plugin needs to follow files on this
//! machine without polling them. A watch is one inotify descriptor that the
//! engine drains on its tick; a script hears of a change once the writes
//! settle, never once per write.
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

use mlua::{Lua, Table, Value};

/// The most one read returns, and what it returns unless asked.
pub const READ_MAX: usize = 16 << 20;
const READ_DEFAULT: usize = 1 << 20;
/// The most entries one listing returns.
pub const LIST_MAX: usize = 10_000;
/// At most this many watches at once, each over at most this many folders.
pub const MAX_WATCHES: usize = 16;
pub const MAX_WATCHED: usize = 64;

/// Folders whose changes a script hears of once they settle.
pub struct Watch {
    file: File,
    settle: Duration,
    dirty: Option<Instant>,
}

impl Watch {
    /// Watch each existing folder in `paths`; the count is how many are
    /// watched. A folder made later needs a new watch.
    pub fn new(paths: &[String], settle: Duration) -> std::io::Result<(Watch, usize)> {
        // SAFETY: the flags request a fresh nonblocking close-on-exec descriptor.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a successful call returned a descriptor nothing else owns.
        let file = unsafe { File::from_raw_fd(fd) };
        let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_MOVED_FROM | libc::IN_DELETE;
        let mut watched = 0;
        for path in paths {
            let Ok(name) = std::ffi::CString::new(path.as_bytes()) else { continue };
            // SAFETY: the descriptor is owned here and the path is NUL-terminated.
            if unsafe { libc::inotify_add_watch(file.as_raw_fd(), name.as_ptr(), mask) } >= 0 {
                watched += 1;
            }
        }
        Ok((Watch { file, settle, dirty: None }, watched))
    }

    /// Take the events that arrived; true once a change has settled.
    pub fn due(&mut self, now: Instant) -> bool {
        let mut buffer = [0u8; 8192];
        while let Ok(count) = (&self.file).read(&mut buffer) {
            if count == 0 {
                break;
            }
            self.dirty.get_or_insert(now);
        }
        if self.dirty.is_some_and(|at| now >= at + self.settle) {
            self.dirty = None;
            return true;
        }
        false
    }

    /// When a change that arrived settles.
    pub fn next_due(&self) -> Option<Instant> {
        self.dirty.map(|at| at + self.settle)
    }
}

/// A file's kind, size, inode and modification time in nanoseconds.
fn stat(lua: &Lua, meta: &fs::Metadata) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set(
        "kind",
        if meta.is_dir() {
            "dir"
        } else if meta.is_file() {
            "file"
        } else {
            "other"
        },
    )?;
    t.set("size", meta.size() as i64)?;
    t.set("ino", meta.ino() as i64)?;
    t.set("mtime", meta.mtime().saturating_mul(1_000_000_000).saturating_add(meta.mtime_nsec()))?;
    Ok(t)
}

/// `deck.fs` without `watch`, which the engine adds.
pub fn table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set(
        "list",
        lua.create_function(|lua, dir: String| {
            let Ok(entries) = fs::read_dir(&dir) else { return Ok(Value::Nil) };
            let out = lua.create_table()?;
            for entry in entries.flatten().take(LIST_MAX) {
                let Ok(meta) = entry.metadata() else { continue };
                let item = stat(lua, &meta)?;
                item.set("name", entry.file_name().to_string_lossy().into_owned())?;
                out.push(item)?;
            }
            Ok(Value::Table(out))
        })?,
    )?;
    t.set(
        "stat",
        lua.create_function(|lua, path: String| match fs::metadata(&path) {
            Ok(meta) => Ok(Value::Table(stat(lua, &meta)?)),
            Err(_) => Ok(Value::Nil),
        })?,
    )?;
    t.set(
        "read",
        lua.create_function(|lua, (path, max): (String, Option<usize>)| {
            let max = max.unwrap_or(READ_DEFAULT).min(READ_MAX);
            let Ok(file) = File::open(&path) else { return Ok(Value::Nil) };
            let mut bytes = Vec::new();
            if file.take(max as u64 + 1).read_to_end(&mut bytes).is_err() || bytes.len() > max {
                return Ok(Value::Nil);
            }
            Ok(Value::String(lua.create_string(&bytes)?))
        })?,
    )?;
    t.set(
        "realpath",
        lua.create_function(|_, path: String| {
            Ok(fs::canonicalize(path).ok().map(|path| path.to_string_lossy().into_owned()))
        })?,
    )?;
    Ok(t)
}

/// The SHA-256 of a string, in hex.
pub fn sha256(_: &Lua, text: mlua::String) -> mlua::Result<String> {
    let digest = ring::digest::digest(&ring::digest::SHA256, &text.as_bytes());
    Ok(digest.as_ref().iter().map(|b| format!("{b:02x}")).collect())
}
