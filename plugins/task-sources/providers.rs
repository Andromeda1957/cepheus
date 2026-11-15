//! `cepheus task-source`: read-only, bounded task snapshots for registered
//! Markdown and GitHub sources, run by the core's source supervisor.
//!
//! Successful stdout is a complete JSON task array, never executable queue
//! input. Markdown identities hash the repository-relative canonical file and
//! NFC title with collapsed whitespace, followed by the occurrence of an
//! identical title. Checking or reordering items preserves identities;
//! changing a title changes its identity. Identical duplicates are
//! distinguishable only by their occurrence. Failures print one category,
//! never provider payloads.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

pub const MAX_BYTES: usize = 256 * 1024;
pub const MAX_TASKS: usize = 1000;
/// UTF-8 bytes, matching the task-source API.
pub const TITLE_LIMIT: usize = 200;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// A safe categorical failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceError(pub &'static str);

const INVALID_CONFIGURATION: SourceError = SourceError("invalid-configuration");
const INVALID_PATH: SourceError = SourceError("invalid-path");
const MALFORMED: SourceError = SourceError("malformed-content");
const TOO_LARGE: SourceError = SourceError("snapshot-too-large");

#[derive(Serialize, Debug, PartialEq)]
pub struct Task {
    pub source_id: String,
    pub id: String,
    pub title: String,
    pub state: &'static str,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub next_step: Option<String>,
}

/// The GitHub issue reader: argv and working directory in, stdout out.
pub type Gh<'a> = &'a dyn Fn(&[String], &Path) -> Result<Vec<u8>, SourceError>;

pub fn run(args: &[String]) -> i32 {
    let (code, out, err) = invoke(args, &|argv, cwd| run_gh(argv, cwd, COMMAND_TIMEOUT));
    let _ = std::io::stdout().write_all(out.as_bytes());
    let _ = std::io::stderr().write_all(err.as_bytes());
    code
}

/// The exit status, stdout and stderr of one invocation.
pub fn invoke(args: &[String], gh: Gh) -> (i32, String, String) {
    match snapshot(args, gh) {
        Ok(payload) => (0, payload, String::new()),
        Err(SourceError(category)) => (1, String::new(), format!("task-source: {category}\n")),
    }
}

struct Args {
    provider: String,
    source_id: String,
    root: String,
    file: Option<String>,
    github_repo: Option<String>,
}

fn parse(args: &[String]) -> Result<Args, SourceError> {
    let mut values: HashMap<&str, String> = HashMap::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        let name = ["--provider", "--source-id", "--repository-root", "--file", "--github-repo"]
            .into_iter()
            .find(|known| *known == name)
            .ok_or(INVALID_CONFIGURATION)?;
        let value = match inline {
            Some(value) => value,
            // A following option is a missing value, as for argparse.
            None => rest.next().filter(|v| !v.starts_with('-') || v.len() == 1).ok_or(INVALID_CONFIGURATION)?.clone(),
        };
        values.insert(name, value);
    }
    let provider = values.remove("--provider").ok_or(INVALID_CONFIGURATION)?;
    if provider != "markdown" && provider != "github" {
        return Err(INVALID_CONFIGURATION);
    }
    Ok(Args {
        provider,
        source_id: values.remove("--source-id").ok_or(INVALID_CONFIGURATION)?,
        root: values.remove("--repository-root").ok_or(INVALID_CONFIGURATION)?,
        file: values.remove("--file"),
        github_repo: values.remove("--github-repo"),
    })
}

fn snapshot(args: &[String], gh: Gh) -> Result<String, SourceError> {
    let args = parse(args)?;
    // Keep the registrar's namespaced identity verbatim; an invalid identity
    // fails rather than being clipped into a different source.
    if args.source_id.is_empty() || args.source_id.chars().any(separator) || args.source_id.len() > 256 {
        return Err(INVALID_CONFIGURATION);
    }
    let root = repository_root(&args.root)?;
    let tasks = if args.provider == "markdown" {
        if args.github_repo.is_some() {
            return Err(INVALID_CONFIGURATION);
        }
        markdown_tasks(&root, &args.source_id, args.file.as_deref().unwrap_or("TASKS.md"))?
    } else {
        match (&args.file, &args.github_repo) {
            (None, Some(repo)) if !repo.is_empty() => github_tasks(&root, &args.source_id, repo, gh)?,
            _ => return Err(INVALID_CONFIGURATION),
        }
    };
    let payload = serde_json::to_string(&tasks).map_err(|_| MALFORMED)? + "\n";
    if payload.len() > MAX_BYTES {
        return Err(TOO_LARGE);
    }
    Ok(payload)
}

/// Unicode Cc, Cs, Zl or Zp: a line or control character.
fn separator(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}'
}

/// Unicode Cf (format) characters.
fn format_char(c: char) -> bool {
    matches!(c as u32,
        0xAD | 0x600..=0x605 | 0x61C | 0x6DD | 0x70F | 0x890..=0x891 | 0x8E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF
        | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// Terminal controls and bidi overrides go; Unicode text, combining marks and
/// emoji joiners stay. Whitespace collapses, then NFC.
pub fn normalized_title(value: &str) -> String {
    let text: String = value
        .chars()
        .filter_map(|c| {
            if separator(c) {
                Some(' ')
            } else if format_char(c) && c != '\u{200C}' && c != '\u{200D}' {
                None
            } else {
                Some(c)
            }
        })
        .collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ").nfc().collect()
}

fn task(source_id: &str, id: String, title: String, state: &'static str) -> Result<Task, SourceError> {
    if title.is_empty() {
        return Err(MALFORMED);
    }
    let title = if title.len() > TITLE_LIMIT {
        let mut end = TITLE_LIMIT - '…'.len_utf8();
        while !title.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &title[..end])
    } else {
        title
    };
    Ok(Task { source_id: source_id.into(), id, title, state, worktree: None, branch: None, next_step: None })
}

fn repository_root(value: &str) -> Result<PathBuf, SourceError> {
    let root = std::fs::canonicalize(if value.is_empty() { "." } else { value }).map_err(|_| INVALID_CONFIGURATION)?;
    if !root.is_dir() {
        return Err(INVALID_CONFIGURATION);
    }
    Ok(root)
}

fn open_at(directory: i32, name: &std::ffi::OsStr, flags: i32) -> Result<File, SourceError> {
    let name = CString::new(name.as_bytes()).map_err(|_| INVALID_PATH)?;
    // SAFETY: a valid directory descriptor and a NUL-terminated name.
    let fd = unsafe { libc::openat(directory, name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(INVALID_PATH);
    }
    // SAFETY: a fresh descriptor this function owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Resolve containment, then open canonical components without following
/// links. A contained symlink is allowed, but a swapped path component cannot
/// redirect the descriptor walk outside the registered root. A nonblocking
/// open rejects FIFOs and devices without waiting on them.
fn markdown_bytes(root: &Path, filename: &str) -> Result<(String, String), SourceError> {
    let configured = Path::new(filename);
    if configured.is_absolute() || configured.components().any(|c| c == Component::ParentDir) {
        return Err(INVALID_PATH);
    }
    let target = std::fs::canonicalize(root.join(configured)).map_err(|_| INVALID_PATH)?;
    let relative = target.strip_prefix(root).map_err(|_| INVALID_PATH)?;
    let parts: Vec<_> = relative.iter().collect();
    let Some((last, folders)) = parts.split_last() else { return Err(INVALID_PATH) };
    let directory_flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    let mut directory = open_at(libc::AT_FDCWD, root.as_os_str(), directory_flags)?;
    for part in folders {
        directory = open_at(directory.as_raw_fd(), part, directory_flags)?;
    }
    let mut file = open_at(directory.as_raw_fd(), last, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)?;
    drop(directory);
    let stamp = |file: &File| {
        use std::os::unix::fs::MetadataExt;
        file.metadata().map(|m| (m.is_file(), m.size(), m.mtime(), m.mtime_nsec())).map_err(|_| INVALID_PATH)
    };
    let before = stamp(&file)?;
    if !before.0 {
        return Err(INVALID_PATH);
    }
    if before.1 > MAX_BYTES as u64 {
        return Err(TOO_LARGE);
    }
    let mut content = Vec::new();
    (&mut file).take(MAX_BYTES as u64 + 1).read_to_end(&mut content).map_err(|_| INVALID_PATH)?;
    let after = stamp(&file)?;
    if content.len() > MAX_BYTES {
        return Err(TOO_LARGE);
    }
    if before != after {
        return Err(SourceError("source-changed"));
    }
    let text = String::from_utf8(content).map_err(|_| MALFORMED)?;
    let relative = parts.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("/");
    Ok((relative, text))
}

/// Python's `str.splitlines`: every line boundary it recognises, `\r\n` as one.
fn lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if matches!(
            c,
            '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        ) {
            out.push(&text[start..at]);
            start = at + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
                chars.next();
                start += 1;
            }
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// A fence's opening: its marker (three or more of one of `` ` `` or `~`) and
/// the rest of the line.
fn fence_opening(line: &str) -> Option<(char, usize, &str)> {
    let line = line.trim_start_matches(blank);
    let marker = line.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let length = line.len() - line.trim_start_matches(marker).len();
    (length >= 3).then(|| (marker, length, &line[length..]))
}

fn fence_closing(line: &str, marker: char, length: usize) -> bool {
    let line = line.trim_matches(blank);
    line.len() >= length && line.chars().all(|c| c == marker)
}

/// A checklist entry: whether it is checked, and its raw title.
fn checkbox(line: &str) -> Option<(bool, &str)> {
    let rest = line.trim_start_matches(blank);
    let rest = if let Some(rest) = rest.strip_prefix(['-', '+', '*']) {
        rest
    } else {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if !(1..=9).contains(&digits) {
            return None;
        }
        rest[digits..].strip_prefix(['.', ')'])?
    };
    let after = rest.trim_start_matches(blank);
    if after.len() == rest.len() {
        return None;
    }
    let mut chars = after.chars();
    if chars.next() != Some('[') {
        return None;
    }
    let mark = chars.next()?;
    if !matches!(mark, ' ' | 'x' | 'X') || chars.next() != Some(']') {
        return None;
    }
    let tail = &after[3..];
    let title = tail.trim_start_matches(blank);
    if title.len() == tail.len() && !title.is_empty() {
        return None;
    }
    Some((mark != ' ', title))
}

pub fn markdown_tasks(root: &Path, source_id: &str, filename: &str) -> Result<Vec<Task>, SourceError> {
    let (relative, content) = markdown_bytes(root, filename)?;
    let mut tasks = Vec::new();
    let mut occurrences: HashMap<String, usize> = HashMap::new();
    let mut fence: Option<(char, usize)> = None;
    for line in lines(&content) {
        if let Some((marker, length)) = fence {
            if fence_closing(line, marker, length) {
                fence = None;
            }
            continue;
        }
        if let Some((marker, length, info)) = fence_opening(line) {
            if marker != '`' || !info.contains('`') {
                fence = Some((marker, length));
                continue;
            }
        }
        let Some((checked, raw)) = checkbox(line) else { continue };
        let title = normalized_title(raw);
        let digest = ring::digest::digest(&ring::digest::SHA256, format!("{relative}\0{title}").as_bytes());
        let fingerprint: String = digest.as_ref()[..12].iter().map(|b| format!("{b:02x}")).collect();
        let occurrence = occurrences.entry(fingerprint.clone()).or_default();
        *occurrence += 1;
        let id = format!("md-{fingerprint}-{occurrence}");
        tasks.push(task(source_id, id, title, if checked { "stopped" } else { "ready" })?);
        if tasks.len() > MAX_TASKS {
            return Err(TOO_LARGE);
        }
    }
    Ok(tasks)
}

/// Read a fixed `gh` command with bounded output and elapsed time.
pub fn run_gh(argv: &[String], cwd: &Path, timeout: Duration) -> Result<Vec<u8>, SourceError> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => SourceError("command-unavailable"),
            _ => SourceError("command-failed"),
        })?;
    let deadline = Instant::now() + timeout;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or(SourceError("command-failed"))?;
        let mut output = Vec::new();
        let mut chunk = vec![0u8; 65536];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let mut poll = libc::pollfd { fd: stdout.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: one valid pollfd for the duration of the call.
            let ready = unsafe { libc::poll(&mut poll, 1, millis) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(SourceError("command-failed"));
            }
            if ready == 0 || remaining.is_zero() {
                return Err(SourceError("timeout"));
            }
            let want = (MAX_BYTES + 1 - output.len()).min(chunk.len());
            let read = stdout.read(&mut chunk[..want]).map_err(|_| SourceError("command-failed"))?;
            if read == 0 {
                break;
            }
            output.extend_from_slice(&chunk[..read]);
            if output.len() > MAX_BYTES {
                return Err(TOO_LARGE);
            }
        }
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok((status.code(), output)),
                Ok(None) if Instant::now() >= deadline => return Err(SourceError("timeout")),
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return Err(SourceError("command-failed")),
            }
        }
    })();
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
    }
    let _ = child.wait();
    match result? {
        (Some(0), output) => Ok(output),
        (Some(4), _) => Err(SourceError("authentication-required")),
        _ => Err(SourceError("command-failed")),
    }
}

fn valid_repository(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else { return false };
    let owner_ok = owner.len() <= 39
        && owner.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && owner.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let name_ok = (1..=100).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && name != "."
        && name != "..";
    owner_ok && name_ok
}

pub fn github_tasks(root: &Path, source_id: &str, repo: &str, gh: Gh) -> Result<Vec<Task>, SourceError> {
    if !valid_repository(repo) {
        return Err(INVALID_CONFIGURATION);
    }
    let argv: Vec<String> = [
        "gh",
        "issue",
        "list",
        "--repo",
        repo,
        "--state",
        "all",
        "--limit",
        &(MAX_TASKS + 1).to_string(),
        "--json",
        "number,title,state",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let output = gh(&argv, root)?;
    if output.len() > MAX_BYTES {
        return Err(TOO_LARGE);
    }
    let text = std::str::from_utf8(&output).map_err(|_| MALFORMED)?;
    let rows: serde_json::Value = serde_json::from_str(text).map_err(|_| MALFORMED)?;
    let rows = rows.as_array().ok_or(MALFORMED)?;
    if rows.len() > MAX_TASKS {
        return Err(TOO_LARGE);
    }
    let mut tasks = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
        let row = row.as_object().ok_or(MALFORMED)?;
        let number = row.get("number").and_then(|n| n.as_i64()).filter(|n| *n > 0).ok_or(MALFORMED)?;
        let title = row.get("title").and_then(|t| t.as_str()).ok_or(MALFORMED)?;
        let state = match row.get("state").and_then(|s| s.as_str()) {
            Some("OPEN") => "ready",
            Some("CLOSED") => "stopped",
            _ => return Err(MALFORMED),
        };
        if !seen.insert(number) {
            return Err(MALFORMED);
        }
        tasks.push(task(source_id, number.to_string(), normalized_title(title), state)?);
    }
    Ok(tasks)
}

#[cfg(test)]
#[path = "../../src/tests/task_source_providers.rs"]
mod tests;
