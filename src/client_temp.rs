//! A private temporary directory for each agent client.
//!
//! Clients, hooks, MCP servers and subprocesses receive a disk-backed `TMPDIR`, including Java temporary-file
//! settings. This keeps client-created temporary files out of shared RAM-backed directories. `sweep` removes a
//! directory after no process uses it.

use std::collections::HashSet;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};

/// A new folder is kept this long though nothing uses it yet: the client it
/// is made for starts just after.
const GRACE: Duration = Duration::from_secs(120);

/// Make a new folder for one client and return the variables that point the
/// client, and everything it starts, at it. `label` says what the client is,
/// in lowercase letters and digits, for anyone reading the folder names.
pub fn vars(label: &str) -> Result<Vec<(String, String)>> {
    vars_with(label, std::env::var("JAVA_TOOL_OPTIONS").ok())
}

/// `vars`, adding to the Java options `java` rather than to this process's.
pub fn vars_with(label: &str, java: Option<String>) -> Result<Vec<(String, String)>> {
    let dir = create(&crate::paths::client_temp_root(), label, crate::util::now_ms())?;
    Ok(vars_for(&dir, java.as_deref()))
}

/// Start `command` with a new folder of its own (`vars`).
pub fn contain(command: &mut Command, label: &str) -> Result<()> {
    command.envs(vars(label)?);
    Ok(())
}

fn vars_for(dir: &Path, java: Option<&str>) -> Vec<(String, String)> {
    let dir = dir.to_string_lossy().into_owned();
    // Java ignores TMPDIR, and writes its performance data under /tmp
    // whatever java.io.tmpdir says, so that is turned off. Every JVM reads
    // this variable, Gradle's and Kotlin's daemons included. A folder an
    // earlier client was given is dropped: the new one replaces it.
    let mut options: Vec<&str> = java
        .unwrap_or_default()
        .split_whitespace()
        .filter(|option| !option.starts_with("-Djava.io.tmpdir=") && *option != "-XX:-UsePerfData")
        .collect();
    let tmpdir = format!("-Djava.io.tmpdir={dir}");
    options.extend([tmpdir.as_str(), "-XX:-UsePerfData"]);
    let java = options.join(" ");
    let mut vars: Vec<(String, String)> = ["TMPDIR", "TMP", "TEMP", "CLAUDE_CODE_TMPDIR"]
        .into_iter()
        .map(|name| (name.to_string(), dir.clone()))
        .collect();
    vars.push(("JAVA_TOOL_OPTIONS".to_string(), java));
    vars
}

/// Make `<root>/<label>-<unix ms>-<n>`, for this user only.
fn create(root: &Path, label: &str, now_ms: u64) -> Result<PathBuf> {
    debug_assert!(valid_label(label), "client folder label {label:?}");
    private_root(root, true)?;
    for n in 0..1000 {
        let dir = root.join(format!("{label}-{now_ms}-{n}"));
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("creating {}", dir.display())),
        }
    }
    anyhow::bail!("no free temporary folder name under {}", root.display())
}

fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Check that only this user can reach `root`, making it first when `make`
/// and it is missing. In a shared `/var/tmp` another user could have made it,
/// or put a link there, first, and read or replace what clients write.
fn private_root(root: &Path, make: bool) -> Result<()> {
    if make {
        match DirBuilder::new().mode(0o700).create(root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).with_context(|| format!("creating {}", root.display())),
        }
    }
    let meta = fs::symlink_metadata(root).with_context(|| format!("reading {}", root.display()))?;
    let shown = root.display();
    anyhow::ensure!(
        meta.file_type().is_dir(),
        "{shown}, where agent clients get their temporary folders, is not a folder"
    );
    anyhow::ensure!(
        meta.uid() == uid(),
        "{shown}, where agent clients get their temporary folders, belongs to another user"
    );
    anyhow::ensure!(
        meta.mode() & 0o077 == 0,
        "{shown}, where agent clients get their temporary folders, is open to other users; make it private with chmod 700"
    );
    Ok(())
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

/// What one sweep removed, and what it could not.
#[derive(Debug, Default)]
pub struct Swept {
    pub removed: Vec<PathBuf>,
    pub failed: Vec<String>,
}

/// Remove each client folder under `root` past its grace period that no
/// process uses: none has it in its environment, as its working folder or as
/// an open file. A process this user cannot read keeps nothing, as unrelated
/// unreadable processes keep no scratch (memory-maintenance.md Session
/// Artifact Cleanup). Entries not named as a client folder, links, and
/// anything not this user's own are left alone.
pub fn sweep(root: &Path, now_ms: u64) -> Result<Swept> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Swept::default()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", root.display())),
    };
    private_root(root, false)?;
    let mut due = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", root.display()))?;
        let name = entry.file_name();
        let Some(created) = name.to_str().and_then(created_ms) else { continue };
        if now_ms.saturating_sub(created) < GRACE.as_millis() as u64 {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else { continue };
        if meta.file_type().is_dir() && meta.uid() == uid() {
            due.push(name.to_string_lossy().into_owned());
        }
    }
    let mut swept = Swept::default();
    if due.is_empty() {
        return Ok(swept);
    }
    let used = in_use(root)?;
    for name in due.into_iter().filter(|name| !used.contains(name)) {
        let dir = root.join(name);
        match remove(&dir) {
            Ok(()) => swept.removed.push(dir),
            Err(error) => swept.failed.push(format!("removing {}: {error}", dir.display())),
        }
    }
    Ok(swept)
}

/// The creation time in a client folder's name, `<label>-<unix ms>-<n>`.
fn created_ms(name: &str) -> Option<u64> {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let mut parts = name.split('-');
    let (label, ms, n) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !valid_label(label) || !digits(ms) || !digits(n) {
        return None;
    }
    ms.parse().ok()
}

/// The names of the folders under `root` that a process this user can read
/// has in its environment, as its working folder or as an open file.
fn in_use(root: &Path) -> Result<HashSet<String>> {
    let mut roots = vec![root.to_path_buf()];
    // A working folder or an open file reads back with links resolved.
    if let Ok(real) = fs::canonicalize(root) {
        if real != root {
            roots.push(real);
        }
    }
    let prefixes: Vec<Vec<u8>> = roots
        .iter()
        .map(|root| {
            let mut prefix = root.as_os_str().as_bytes().to_vec();
            prefix.push(b'/');
            prefix
        })
        .collect();
    let mut used = HashSet::new();
    let mut note = |bytes: &[u8]| {
        for prefix in &prefixes {
            names_after(bytes, prefix, &mut used);
        }
    };
    for entry in fs::read_dir("/proc").context("reading /proc")?.flatten() {
        if !entry.file_name().as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let process = entry.path();
        if let Ok(environment) = fs::read(process.join("environ")) {
            note(&environment);
        }
        if let Ok(cwd) = fs::read_link(process.join("cwd")) {
            note(cwd.as_os_str().as_bytes());
        }
        let Ok(files) = fs::read_dir(process.join("fd")) else { continue };
        for file in files.flatten() {
            if let Ok(target) = fs::read_link(file.path()) {
                note(target.as_os_str().as_bytes());
            }
        }
    }
    Ok(used)
}

/// Add to `used` each folder name that follows `prefix` in `bytes`.
fn names_after(bytes: &[u8], prefix: &[u8], used: &mut HashSet<String>) {
    let mut rest = bytes;
    while let Some(at) = rest.windows(prefix.len()).position(|window| window == prefix) {
        let tail = &rest[at + prefix.len()..];
        let end = tail
            .iter()
            .position(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-'))
            .unwrap_or(tail.len());
        if end > 0 {
            used.insert(String::from_utf8_lossy(&tail[..end]).into_owned());
        }
        rest = tail;
    }
}

/// Remove `dir` and everything in it, never following a link. A tool may
/// have left a folder in it read-only, which keeps its contents, so then
/// this user's folders in it are made writable and it is tried again.
fn remove(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            writable(dir);
            fs::remove_dir_all(dir)
        }
        result => result,
    }
}

fn writable(dir: &Path) {
    let Ok(meta) = fs::symlink_metadata(dir) else { return };
    if !meta.file_type().is_dir() || meta.uid() != uid() {
        return;
    }
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(meta.mode() & 0o7777 | 0o700));
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        writable(&entry.path());
    }
}

#[cfg(test)]
#[path = "tests/client_temp.rs"]
mod tests;
