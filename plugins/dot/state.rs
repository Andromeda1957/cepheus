//! Private local bridge state. No credential is ever serialized here.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::Error;

/// The largest private state or metadata file the bridge reads.
pub const FILE_MAX: u64 = 8 << 20;

/// The parts of a URL, as Python's `urlsplit` divides them.
pub struct Url<'a> {
    pub scheme: String,
    pub netloc: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub fragment: &'a str,
}

pub fn split_url(value: &str) -> Url<'_> {
    let mut scheme = String::new();
    let mut rest = value;
    if let Some(at) = value.find(':') {
        let candidate = &value[..at];
        let valid = candidate.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && candidate.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
        if valid {
            scheme = candidate.to_ascii_lowercase();
            rest = &value[at + 1..];
        }
    }
    let mut netloc = "";
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = &after[..end];
        rest = &after[end..];
    }
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    Url { scheme, netloc, path, query, fragment }
}

pub fn chat_url(value: &str) -> Result<(), Error> {
    let url = split_url(value);
    if url.scheme != "https"
        || url.netloc != "chatgpt.com"
        || !url.query.is_empty()
        || !url.fragment.is_empty()
        || value.contains('\\')
        || value.chars().any(|c| (c as u32) < 33)
    {
        return Err(Error::Invalid("Use an https://chatgpt.com/ URL without query parameters or fragments.".into()));
    }
    Ok(())
}

pub fn dot_url(value: &str) -> Result<String, Error> {
    chat_url(value)?;
    let path = split_url(value).path;
    match path.strip_prefix("/dots/") {
        Some(id) if !id.trim_matches('/').is_empty() => Ok(value.to_string()),
        _ => Err(Error::Invalid("Use your existing Dot URL under https://chatgpt.com/dots/.".into())),
    }
}

/// Python's `urllib.parse.unquote`: invalid escapes stay as they are.
pub fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode every byte outside the unreserved set and `keep`:
/// `quote(text, safe="")` with `keep = "_.-~"`, `encodeURIComponent` with
/// `keep = "-_.!~*'()"`.
pub fn quote(text: &str, keep: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || keep.as_bytes().contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `~` expanded and made absolute, without resolving links or `..`.
pub fn absolute(path: &Path) -> PathBuf {
    let path = match path.to_str().and_then(|text| text.strip_prefix('~')) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            crate::paths::home().join(rest.trim_start_matches('/'))
        }
        _ => path.to_path_buf(),
    };
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map(|dir| dir.join(&path)).unwrap_or(path)
    }
}

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// Create or verify a private directory: no linked parent, owned by this
/// user, mode 0700.
pub fn private_dir(path: &Path) -> Result<PathBuf, Error> {
    let path = absolute(path);
    if path.ancestors().any(is_link) {
        return Err(Error::Invalid("Bridge state must not use symlinked directories.".into()));
    }
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&path)?;
    // SAFETY: getuid has no preconditions.
    if fs::metadata(&path)?.uid() != unsafe { libc::getuid() } {
        return Err(Error::Invalid("Bridge state must belong to the current user.".into()));
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

/// A private JSON file, or `None` when it does not exist. Links are refused.
pub fn read_json(path: &Path) -> Result<Option<Value>, Error> {
    let mut file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if !file.metadata()?.is_file() || file.metadata()?.len() > FILE_MAX {
        return Err(Error::Invalid("Invalid bridge state file.".into()));
    }
    let mut data = Vec::new();
    (&mut file).take(FILE_MAX + 1).read_to_end(&mut data)?;
    if data.len() as u64 > FILE_MAX {
        return Err(Error::Invalid("Invalid bridge state file.".into()));
    }
    serde_json::from_slice(&data).map(Some).map_err(|_| Error::Invalid("Invalid bridge state file.".into()))
}

/// Replace `path` with `data` through a new 0600 file beside it, so a reader
/// never sees part of a file and concurrent writers never share a temporary.
pub fn write_bytes(path: &Path, data: &[u8]) -> Result<(), Error> {
    replace(path, data, true)
}

/// `durable` forces the data to disk first; a file the bridge rebuilds from
/// the service on its next refresh does not need that.
fn replace(path: &Path, data: &[u8], durable: bool) -> Result<(), Error> {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("state");
    let unique = crate::util::uuid_v4()?.replace('-', "");
    let temporary = path.with_file_name(format!(".{name}-{unique}"));
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(data)?;
        if durable {
            file.sync_all()?;
        }
        fs::rename(&temporary, path)
    })();
    let _ = fs::remove_file(&temporary);
    result.map_err(Error::from)
}

/// How long the service trusts a working state; it is renewed sooner.
const ACTIVITY_RENEW: Duration = Duration::from_secs(4);

pub fn write_json(path: &Path, value: &Value) -> Result<(), Error> {
    write_bytes(path, &serde_json::to_vec(value).map_err(io::Error::other)?)
}

/// The bridge's private directory: settings, the pinned selection, the
/// delivery journal, the feed, activity and the profile card.
pub struct State {
    pub directory: PathBuf,
    lock: Mutex<Option<File>>,
    /// `None` until the first notice of this process is set.
    notice: Mutex<Option<Option<String>>>,
    publishing: Mutex<()>,
    /// The last `(known, working)` written and when.
    activity: Mutex<Option<(bool, bool, Instant)>>,
}

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl State {
    pub fn new(directory: &Path) -> Result<State, Error> {
        Ok(State {
            directory: private_dir(directory)?,
            lock: Mutex::new(None),
            notice: Mutex::new(None),
            publishing: Mutex::new(()),
            activity: Mutex::new(None),
        })
    }

    /// Become the only controller of this Dot.
    pub fn acquire(&self) -> Result<(), Error> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.directory.join("bridge.lock"))?;
        if !file.metadata()?.is_file() {
            return Err(Error::Invalid("Invalid Dot controller lock.".into()));
        }
        // SAFETY: flock on an open descriptor this function owns.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Err(Error::Invalid(
                    "Dot is already connected in another bridge. Select that deck session.".into(),
                ));
            }
            return Err(err.into());
        }
        *guard(&self.lock) = Some(file);
        Ok(())
    }

    pub fn holds_lock(&self) -> bool {
        guard(&self.lock).is_some()
    }

    pub fn close(&self) {
        guard(&self.lock).take();
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.json"))
    }

    pub fn read(&self, name: &str) -> Result<Option<Value>, Error> {
        read_json(&self.path(name))
    }

    pub fn write(&self, name: &str, value: &Value) -> Result<(), Error> {
        write_json(&self.path(name), value)
    }

    /// The saved settings object, or an empty one.
    pub fn settings(&self) -> Result<serde_json::Map<String, Value>, Error> {
        match self.read("settings")? {
            None => Ok(serde_json::Map::new()),
            Some(Value::Object(map)) => Ok(map),
            Some(_) => Err(Error::Invalid("Invalid bridge state file.".into())),
        }
    }

    /// The one actionable line shown below the conversation, if any. True
    /// when the feed must be published again to show it.
    pub fn set_notice(&self, notice: Option<String>) -> bool {
        let mut current = guard(&self.notice);
        let changed = current.as_ref() != Some(&notice);
        *current = Some(notice);
        changed
    }

    /// Write the feed: `items`, or the current chat messages when `None`, with
    /// the notice last. An unchanged feed is not rewritten.
    pub fn publish(&self, items: Option<Vec<Value>>) -> Result<(), Error> {
        let _publishing = guard(&self.publishing);
        let current = match self.read("transcript")? {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        };
        let mut items = items.unwrap_or_else(|| {
            current
                .iter()
                .filter(|m| matches!(m.get("k").and_then(Value::as_str), Some("user" | "assistant")))
                .cloned()
                .collect()
        });
        if let Some(notice) = guard(&self.notice).clone().flatten() {
            items.push(json!({"k": "notice", "text": notice}));
        }
        if current != items {
            let data = serde_json::to_vec(&Value::Array(items)).map_err(io::Error::other)?;
            replace(&self.path("transcript"), &data, false)?;
        }
        Ok(())
    }

    /// Record whether this controller knows Dot is working. Only trusted
    /// bridge activity counts; conversation and composer text never do.
    /// Only a change is written, and a working state is renewed before the
    /// service stops trusting it.
    pub fn activity(&self, snapshot: Option<&Value>) -> Result<(), Error> {
        let known = snapshot.is_some_and(|snap| {
            snap.is_object()
                && snap.get("activity_known").is_none_or(|known| known == &Value::Bool(true))
                && snap.get("working").is_some_and(Value::is_boolean)
        });
        let working = known && snapshot.and_then(|snap| snap.get("working")) == Some(&Value::Bool(true));
        let mut last = guard(&self.activity);
        if last.is_some_and(|(k, w, at)| (k, w) == (known, working) && (!working || at.elapsed() < ACTIVITY_RENEW)) {
            return Ok(());
        }
        let value = json!({"pid": std::process::id(), "at": crate::util::now_ms(), "known": known, "working": working});
        replace(&self.path("activity"), &serde_json::to_vec(&value).map_err(io::Error::other)?, false)?;
        *last = Some((known, working, Instant::now()));
        Ok(())
    }
}
