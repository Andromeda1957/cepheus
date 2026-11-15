//! Files the phone attaches to a message (`README.md`, "Phone", "Attachments"). A file arrives in pieces, is
//! checked against its SHA-256 and kept in `<dir>/<session>/`, readable only by the owner. Sending the message
//! types the stored paths after it. Files go after seven days, or with their session.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};

use crate::util;

/// Largest file a message may carry, and how many it may carry.
pub const FILE_MAX: u64 = 25 << 20;
pub const FILES_MAX: usize = 10;
/// Uploads a phone may have open or waiting for their message at once.
const OPEN_MAX: usize = 2 * FILES_MAX;
/// Longest stored name, extension included.
pub const NAME_MAX: usize = 100;
/// How long a stored file is kept.
const KEEP: Duration = Duration::from_secs(7 * 86_400);
/// How long an upload may wait for its last piece or its message.
const OPEN_FOR: Duration = Duration::from_secs(3600);
/// How often the folder is swept.
const SWEEP_EVERY: Duration = Duration::from_secs(60);
const PIECE_PREFIX: &str = ".part-";

struct Upload {
    device: String,
    session: u64,
    name: String,
    size: u64,
    sha256: String,
    received: u64,
    piece: PathBuf,
    file: Option<File>,
    digest: ring::digest::Context,
    started: Instant,
    /// Where the whole file was kept.
    stored: Option<PathBuf>,
}

pub struct Uploads {
    dir: PathBuf,
    open: HashMap<String, Upload>,
    swept: Option<Instant>,
}

impl Uploads {
    pub fn new(dir: PathBuf) -> Uploads {
        Uploads { dir, open: HashMap::new(), swept: None }
    }

    /// Begin a file for `session`; its upload id.
    pub fn start(&mut self, device: &str, session: u64, name: &str, size: u64, sha256: &str) -> Result<String> {
        if size > FILE_MAX {
            bail!("a file may be at most 25 MB");
        }
        let sha256 = sha256.to_ascii_lowercase();
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("the checksum is not a SHA-256");
        }
        if self.open.values().filter(|u| u.device == device).count() >= OPEN_MAX {
            bail!("too many files waiting to be sent; send or remove some first");
        }
        let folder = self.folder(session)?;
        let token = format!("u-{}", crate::prompt::hex(&crate::remote::auth::random_bytes(12)?));
        let piece = folder.join(format!("{PIECE_PREFIX}{token}"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&piece)
            .with_context(|| format!("could not create {}", piece.display()))?;
        self.open.insert(
            token.clone(),
            Upload {
                device: device.to_string(),
                session,
                name: safe_name(name),
                size,
                sha256,
                received: 0,
                piece,
                file: Some(file),
                digest: ring::digest::Context::new(&ring::digest::SHA256),
                started: Instant::now(),
                stored: None,
            },
        );
        if size == 0 {
            return self.finish(&token).map(|_| token);
        }
        Ok(token)
    }

    /// Add a piece at `offset`; the stored path once the file is whole.
    pub fn piece(&mut self, device: &str, token: &str, offset: u64, data: &[u8]) -> Result<Option<PathBuf>> {
        let upload = match self.open.get_mut(token) {
            Some(u) if u.device == device && u.stored.is_none() => u,
            _ => bail!("no such upload"),
        };
        if offset != upload.received {
            bail!("expected the piece at offset {}, not {offset}", upload.received);
        }
        let fits = upload.received + data.len() as u64 <= upload.size;
        match upload.file.as_mut().filter(|_| fits).map(|f| f.write_all(data)) {
            Some(Ok(())) => {
                upload.digest.update(data);
                upload.received += data.len() as u64;
                if upload.received < upload.size {
                    return Ok(None);
                }
            }
            Some(Err(err)) => {
                self.drop_upload(token);
                return Err(err).context("could not write the file");
            }
            None => {
                self.drop_upload(token);
                bail!("more bytes than the file's size");
            }
        }
        self.finish(token).map(Some)
    }

    fn finish(&mut self, token: &str) -> Result<PathBuf> {
        let upload = self.open.get_mut(token).context("no such upload")?;
        let file = upload.file.take().context("no such upload")?;
        let sum = crate::prompt::hex(upload.digest.clone().finish().as_ref());
        if sum != upload.sha256 {
            self.drop_upload(token);
            bail!("the file did not arrive whole (checksum mismatch); send it again");
        }
        let synced = file.sync_all();
        drop(file);
        if let Err(err) = synced {
            self.drop_upload(token);
            return Err(err).context("could not save the file");
        }
        let folder = upload.piece.parent().map(Path::to_path_buf).unwrap_or_else(|| self.dir.clone());
        let path = free_path(&folder, &upload.name);
        if let Err(err) = fs::rename(&upload.piece, &path) {
            self.drop_upload(token);
            return Err(err).context("could not save the file");
        }
        upload.stored = Some(path.clone());
        Ok(path)
    }

    /// The stored paths of `tokens`, all whole files of `session` from
    /// `device`; an error, and nothing to type, otherwise.
    pub fn paths(&self, device: &str, session: u64, tokens: &[String]) -> Result<Vec<PathBuf>> {
        if tokens.len() > FILES_MAX {
            bail!("a message may carry at most {FILES_MAX} files");
        }
        tokens
            .iter()
            .map(|t| match self.open.get(t) {
                Some(u) if u.device == device && u.session == session => {
                    u.stored.clone().context("an upload is not finished; send it again")
                }
                _ => bail!("no such upload; send the file again"),
            })
            .collect()
    }

    /// The files went with their message: each goes with one message only.
    pub fn sent(&mut self, tokens: &[String]) {
        for t in tokens {
            self.open.remove(t);
        }
    }

    fn drop_upload(&mut self, token: &str) {
        if let Some(u) = self.open.remove(token) {
            drop(u.file);
            let _ = fs::remove_file(&u.piece);
        }
    }

    fn folder(&self, session: u64) -> Result<PathBuf> {
        util::ensure_private_dir(&self.dir).context("could not create the uploads folder")?;
        let folder = self.dir.join(session.to_string());
        util::ensure_private_dir(&folder).context("could not create the uploads folder")?;
        Ok(folder)
    }

    /// Remove files over a week old, the folders of sessions no longer in
    /// `live` (none when the deck's sessions are not known yet), and uploads
    /// left unfinished or unsent for an hour. At most once a minute unless
    /// `now`.
    pub fn sweep(&mut self, live: Option<&[u64]>, now: bool) {
        if !now && self.swept.is_some_and(|at| at.elapsed() < SWEEP_EVERY) {
            return;
        }
        self.swept = Some(Instant::now());
        let stale: Vec<String> = self
            .open
            .iter()
            .filter(|(_, u)| u.started.elapsed() > OPEN_FOR || live.is_some_and(|l| !l.contains(&u.session)))
            .map(|(t, _)| t.clone())
            .collect();
        for t in stale {
            self.drop_upload(&t);
        }
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        let open: Vec<&Path> = self.open.values().map(|u| u.piece.as_path()).collect();
        for entry in entries.flatten() {
            let folder = entry.path();
            let session = entry.file_name().to_str().and_then(|n| n.parse::<u64>().ok());
            // Only the folders this module makes: a session's number.
            let Some(session) = session.filter(|_| folder.is_dir() && !folder.is_symlink()) else { continue };
            if live.is_some_and(|l| !l.contains(&session)) {
                let _ = fs::remove_dir_all(&folder);
                continue;
            }
            let Ok(files) = fs::read_dir(&folder) else { continue };
            for file in files.flatten() {
                let path = file.path();
                let old = file
                    .metadata()
                    .ok()
                    .filter(|m| m.is_file())
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .is_some_and(|age| age > KEEP);
                if old && !open.contains(&path.as_path()) {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

/// A file name that stays inside its folder and pastes into a prompt without
/// quoting: letters, digits, `.`, `_` and `-`, not hidden, at most
/// [`NAME_MAX`] long with its extension kept.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out = String::new();
    for c in base.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' };
        if !(c == '_' && out.ends_with('_')) {
            out.push(c);
        }
    }
    let out = out.trim_start_matches('.');
    if out.is_empty() || out.chars().all(|c| c == '.' || c == '_') {
        return "file".into();
    }
    if out.len() <= NAME_MAX {
        return out.to_string();
    }
    let ext = out.rfind('.').map(|i| &out[i..]).filter(|e| e.len() <= 16).unwrap_or("");
    format!("{}{ext}", &out[..NAME_MAX - ext.len()])
}

/// `name` in `dir`, or `name-2`, `name-3`… before its extension when taken.
pub fn free_path(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut path = dir.join(name);
    let mut n = 2;
    while path.symlink_metadata().is_ok() {
        path = dir.join(format!("{stem}-{n}{ext}"));
        n += 1;
    }
    path
}

/// The message as typed: its text, a blank line, then each file's path on a
/// line of its own. Claude Code and Codex read a file from its path.
pub fn with_files(text: &str, paths: &[PathBuf]) -> String {
    let text = text.trim_end();
    let files: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    match (text.trim().is_empty(), files.is_empty()) {
        (_, true) => text.to_string(),
        (true, false) => files.join("\n"),
        (false, false) => format!("{text}\n\n{}", files.join("\n")),
    }
}

#[cfg(test)]
#[path = "../tests/remote_uploads.rs"]
mod tests;
