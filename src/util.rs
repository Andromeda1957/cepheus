//! Small shared helpers: time, files, encoding and text width.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// When a file was last modified, in unix ms.
pub fn modified_ms(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as u64)
}

/// Which version of a file is on disk: its inode and modification time, so
/// two atomic writes within one millisecond still differ.
pub fn file_stamp(path: &Path) -> Option<(u64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).ok()?;
    Some((meta.ino(), meta.mtime(), meta.mtime_nsec()))
}

/// Create a directory (and parents) readable only by the owner.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Replace `path` atomically with `data`, readable only by the owner.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    fs::File::open(dir)?.sync_all()
}

/// Read at most `limit` bytes from the end of a file.
pub fn read_tail(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(limit);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    if start > 0 {
        // Drop the partial first line.
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=pos);
        } else {
            buf.clear();
        }
    }
    Ok(buf)
}

/// Read at most `limit` bytes from the start of a file, ending at a line boundary.
pub fn read_head(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let f = fs::File::open(path)?;
    let mut buf = Vec::new();
    f.take(limit).read_to_end(&mut buf)?;
    if buf.len() as u64 == limit {
        if let Some(pos) = buf.iter().rposition(|&b| b == b'\n') {
            buf.truncate(pos + 1);
        }
    }
    Ok(buf)
}

/// Read bytes `[from, len)` of a file, up to `limit` bytes.
pub fn read_range(path: &Path, from: u64, limit: u64) -> io::Result<(Vec<u8>, u64)> {
    use std::io::{Seek, SeekFrom};
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    if from >= len {
        return Ok((Vec::new(), len));
    }
    let start = if len - from > limit { len - limit } else { from };
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.take(len - start).read_to_end(&mut buf)?;
    let end = start + buf.len() as u64;
    Ok((buf, end))
}

/// The bytes of a file from `from` up to `to`.
pub fn read_between(path: &Path, from: u64, to: u64) -> io::Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut f = fs::File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::with_capacity(to.saturating_sub(from) as usize);
    f.take(to.saturating_sub(from)).read_to_end(&mut buf)?;
    Ok(buf)
}

/// An owner's prompt read as the task a session is on.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskPrompt {
    /// The title it gives (`task_title`).
    pub title: String,
    /// When it was recorded (unix ms; 0 when the record has no time).
    pub at_ms: u64,
    /// Where its line starts in the text it was read from.
    pub pos: usize,
}

/// Whether `needle` occurs in `haystack`.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// The last task prompt among whole JSON lines. Only lines `keep` passes are
/// parsed; `prompt` gives the text a record's owner typed.
pub fn last_task_prompt(
    lines: &[u8],
    keep: impl Fn(&[u8]) -> bool,
    prompt: impl Fn(&serde_json::Value) -> Option<String>,
) -> Option<TaskPrompt> {
    let mut last = None;
    let mut pos = 0;
    for line in lines.split(|&b| b == b'\n') {
        let start = pos;
        pos += line.len() + 1;
        if !keep(line) {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
        let Some(title) = prompt(&record).as_deref().and_then(task_title) else { continue };
        let at_ms = record.get("timestamp").and_then(|t| t.as_str()).and_then(crate::codex::parse_rfc3339_ms);
        last = Some(TaskPrompt { title, at_ms: at_ms.unwrap_or(0), pos: start });
    }
    last
}

/// A prompt as a session title, when it names a task: one line, without the markers of attached images ("[Image
/// #3]") or a leading "and" or "also". A reply such as "continue" or "yes do" names none, nor does a slash
/// command or a message a program wrapped in tags.
pub fn task_title(text: &str) -> Option<String> {
    /// Fewer words than this are a reply, not a task.
    const TASK_WORDS: usize = 3;
    let mut plain = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("[Image #") {
        let after = &rest[i + "[Image #".len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && after[digits..].starts_with(']') {
            plain.push_str(&rest[..i]);
            rest = &after[digits + 1..];
        } else {
            plain.push_str(&rest[..i + "[Image #".len()]);
            rest = after;
        }
    }
    plain.push_str(rest);
    let line = one_line(&plain);
    let mut task = line.as_str();
    for lead in ["and ", "also "] {
        if task.get(..lead.len()).is_some_and(|start| start.eq_ignore_ascii_case(lead)) {
            task = task[lead.len()..].trim_start();
        }
    }
    if task.starts_with(['/', '<']) || task.split(' ').count() < TASK_WORDS {
        return None;
    }
    // A question asks about the work instead of assigning it, unless it requests an action such as "can you
    // make …?".
    let request = ["can you ", "could you ", "would you ", "will you ", "can we ", "could we "]
        .iter()
        .any(|p| task.get(..p.len()).is_some_and(|start| start.eq_ignore_ascii_case(p)));
    if task.ends_with('?') && !request {
        return None;
    }
    Some(truncate_width(task, 100))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// The bytes `base64` wrote; None for anything else.
pub fn unbase64(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let value = |c: u8| B64.iter().position(|&b| b == c).map(|v| v as u32);
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, quad) in bytes.chunks(4).enumerate() {
        let last = i + 1 == bytes.len() / 4;
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = (n << 6) | value(c)?;
        }
        n <<= 6 * pad as u32;
        let decoded = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&decoded[..3 - pad]);
    }
    Some(out)
}

/// A random RFC 4122 version 4 UUID.
pub fn uuid_v4() -> io::Result<String> {
    let mut b = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// Quote a word for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+:=@%".contains(&b)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Compact duration such as `45s`, `12m`, `3h05m` or `2d04h`.
pub fn short_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86400, (secs % 86400) / 3600)
    }
}

/// Truncate to a display width, adding an ellipsis when shortened.
pub fn truncate_width(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    out
}

/// Collapse whitespace and strip control characters from free text.
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            space = !out.is_empty();
        } else if !ch.is_control() {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Interpret a JSON value as unix seconds (numbers in ms are scaled down).
pub fn json_epoch_secs(v: &serde_json::Value) -> Option<u64> {
    let n = match v {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let secs = if n > 1e12 { n / 1000.0 } else { n };
    Some(secs as u64)
}

/// This machine's host name, without its domain.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    name.split('.').next().unwrap_or_default().to_string()
}

#[cfg(test)]
#[path = "tests/util.rs"]
mod tests;
