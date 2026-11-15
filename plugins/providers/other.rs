//! Grok and opencode, the New dialog's "Other" programs. The deck picks each new conversation's id itself, so
//! reopening a session resumes the same conversation, and reads only the title each program gives it, the end
//! of each completed turn, and the list of earlier conversations `Alt+r` and the phone resume from. It never
//! opens their sign-in files (`~/.grok/auth.json`, opencode's `auth.json`).

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::codex;
use crate::monitor::TurnEnd;
use crate::procs::{self, ProcInfo};
use crate::protocol::{Conversation, Tool};
use crate::util;

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// How much of the end of Grok's `prompt_history.jsonl` is read.
const GROK_HISTORY_TAIL: u64 = 2 << 20;

/// The newest opencode sessions read for the resume list, of every folder,
/// before those of other folders are left out.
const OPENCODE_SCAN: usize = 500;

/// An opencode session id in opencode's own form: `ses_`, then the creation
/// time in milliseconds times 4096 with its bits inverted (12 hex digits, so
/// newer sessions sort first in opencode's list), then 14 random base62
/// characters. `opencode -s <id>` creates the session when it does not exist.
pub fn opencode_session_id(now_ms: u64, random: &[u8; 14]) -> String {
    let time = !now_ms.wrapping_mul(0x1000) & 0xffff_ffff_ffff;
    let tail: String = random.iter().map(|b| char::from(BASE62[usize::from(*b) % BASE62.len()])).collect();
    format!("ses_{time:012x}{tail}")
}

/// A new opencode session id for now.
pub fn new_opencode_session_id() -> io::Result<String> {
    let mut random = [0u8; 14];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(opencode_session_id(util::now_ms(), &random))
}

/// Whether `id` has the form of an opencode session id: safe to pass to
/// `opencode -s` and to put in a query.
pub fn is_opencode_id(id: &str) -> bool {
    id.strip_prefix("ses_")
        .is_some_and(|rest| !rest.is_empty() && rest.len() <= 64 && rest.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Whether `id` is a UUID, the form of a Grok session id and safe as a path
/// component.
fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id
            .bytes()
            .enumerate()
            .all(|(i, b)| if [8, 13, 18, 23].contains(&i) { b == b'-' } else { b.is_ascii_hexdigit() })
}

/// The title Grok gave conversation `id`: `generated_title` (else
/// `session_summary`) in `sessions/<encoded folder>/<id>/summary.json` under
/// Grok's home.
pub fn grok_title(home: &Path, id: &str) -> Option<String> {
    if !is_uuid(id) {
        return None;
    }
    let summary = fs::read_dir(home.join("sessions"))
        .ok()?
        .flatten()
        .map(|folder| folder.path().join(id).join("summary.json"))
        .find(|path| path.is_file())?;
    summary_title(&summary)
}

/// `generated_title`, else `session_summary`, of a Grok `summary.json`.
fn summary_title(summary: &Path) -> Option<String> {
    let v: Value = serde_json::from_slice(&fs::read(summary).ok()?).ok()?;
    ["generated_title", "session_summary"]
        .iter()
        .filter_map(|key| v.get(key).and_then(Value::as_str))
        .map(util::one_line)
        .find(|title| !title.is_empty())
}

/// A folder name with its `%XX` escapes decoded: Grok names each folder of
/// `sessions/` after the directory its conversations ran in, `/` as `%2F`.
fn percent_decode(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(u8::from_str_radix(name.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Grok's conversations in `start_dir`, newest first, for `Alt+r` and the phone's resume list. Each one is a
/// conversation the folder's `prompt_history.jsonl` records a prompt for: Grok also keeps a folder for every
/// start that was never prompted, with nothing in it to resume. The title is the one Grok generated, else the
/// first prompt; the times are the prompts' and the chat log's.
pub fn grok_conversations(home: &Path, start_dir: &Path, limit: usize) -> Vec<Conversation> {
    let start = start_dir.to_string_lossy();
    let start = start.trim_end_matches('/');
    let Ok(folders) = fs::read_dir(home.join("sessions")) else { return Vec::new() };
    let Some(folder) = folders.flatten().map(|e| e.path()).find(|path| {
        path.is_dir()
            && path
                .file_name()
                .and_then(OsStr::to_str)
                .and_then(percent_decode)
                .is_some_and(|dir| dir.trim_end_matches('/') == start)
    }) else {
        return Vec::new();
    };
    let Ok(tail) = util::read_tail(&folder.join("prompt_history.jsonl"), GROK_HISTORY_TAIL) else {
        return Vec::new();
    };
    let mut found: BTreeMap<String, Conversation> = BTreeMap::new();
    for line in tail.split(|&b| b == b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        let Some(id) = v.get("session_id").and_then(Value::as_str).filter(|id| is_uuid(id)) else { continue };
        let at = v.get("timestamp").and_then(Value::as_str).and_then(codex::parse_rfc3339_ms).unwrap_or(0) / 1000;
        let entry = found.entry(id.to_string()).or_insert_with(|| Conversation {
            search_words: Vec::new(),
            tool: Tool::Grok,
            id: id.to_string(),
            title: String::new(),
            created_at: at,
            updated_at: at,
            open_in: None,
            host: None,
        });
        entry.created_at = entry.created_at.min(at);
        entry.updated_at = entry.updated_at.max(at);
        let shell = v.get("is_bash").and_then(Value::as_bool).unwrap_or(false);
        if entry.title.is_empty() && !shell {
            entry.title = util::one_line(v.get("prompt").and_then(Value::as_str).unwrap_or(""));
        }
    }
    let mut out: Vec<Conversation> = found
        .into_values()
        .map(|mut c| {
            let dir = folder.join(&c.id);
            if let Some(ms) = util::modified_ms(&dir.join("chat_history.jsonl")) {
                c.updated_at = c.updated_at.max(ms / 1000);
            }
            if let Some(title) = summary_title(&dir.join("summary.json")) {
                c.title = title;
            }
            if c.title.is_empty() {
                c.title = "Grok".to_string();
            }
            c
        })
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    out.truncate(limit);
    out
}

/// opencode's sessions in `start_dir`, newest first, for `Alt+r` and the
/// phone's resume list: those a person started (no parent session) and has
/// not archived, read from its database with the `sqlite3` command, opened
/// read-only. opencode creates a session only with its first message.
pub fn opencode_conversations(db: &Path, start_dir: &Path, limit: usize) -> Vec<Conversation> {
    opencode_conversations_by(&[OsStr::new("sqlite3")], db, start_dir, limit)
}

/// [`opencode_conversations`], running `sqlite3` (the program and any leading
/// arguments) in place of the `sqlite3` program; tests pass a stand-in.
fn opencode_conversations_by(sqlite3: &[&OsStr], db: &Path, start_dir: &Path, limit: usize) -> Vec<Conversation> {
    let Some((program, lead)) = sqlite3.split_first() else { return Vec::new() };
    if !db.is_file() {
        return Vec::new();
    }
    // The folder is compared below, so no path is ever part of the query.
    let query = format!(
        "select id, directory, title, time_created, time_updated from session_v2 \
         where parent_id is null and time_archived is null order by time_updated desc limit {OPENCODE_SCAN}"
    );
    let Ok(out) = Command::new(program)
        .args(lead)
        .args(["-readonly", "-bail", "-json", "-cmd", ".timeout 500"])
        .arg(db)
        .arg(query)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    // `-json` prints nothing at all when no row matches.
    let rows: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let start = start_dir.to_string_lossy();
    let start = start.trim_end_matches('/');
    rows.iter()
        .filter(|row| row.get("directory").and_then(Value::as_str).is_some_and(|d| d.trim_end_matches('/') == start))
        .filter_map(|row| {
            let id = row.get("id").and_then(Value::as_str).filter(|id| is_opencode_id(id))?;
            let title = util::one_line(row.get("title").and_then(Value::as_str).unwrap_or(""));
            let placeholder = ["New session - ", "Child session - "].iter().any(|p| title.starts_with(p));
            let created_at = row.get("time_created").and_then(util::json_epoch_secs).unwrap_or(0);
            Some(Conversation {
                search_words: Vec::new(),
                tool: Tool::OpenCode,
                id: id.to_string(),
                title: if title.is_empty() || placeholder { "opencode".to_string() } else { title },
                created_at,
                updated_at: row.get("time_updated").and_then(util::json_epoch_secs).unwrap_or(created_at),
                open_in: None,
                host: None,
            })
        })
        .take(limit)
        .collect()
}

/// Whether the Grok at `pid` is in a turn. Grok keeps the machine awake
/// while it works (`sleep_prevention`, on by default) by running
/// `systemd-inhibit --what=idle --who=grok --why=agent turn in progress`
/// under itself until the turn and its background work end (Grok 1.0.70).
/// Its screen has no fixed line that says so.
pub fn grok_turn_running(procs: &HashMap<u32, ProcInfo>, pid: u32) -> bool {
    procs::descendants(procs, pid).into_iter().any(|d| {
        let args = procs::cmdline(d);
        args.first().is_some_and(|a| a.rsplit('/').next() == Some("systemd-inhibit"))
            && args.iter().any(|a| a == "--who=grok")
    })
}

/// Grok's record of conversation `id`'s events, `events.jsonl` in its folder
/// under `sessions/` (the one `grok_title` reads).
pub fn grok_events(home: &Path, id: &str) -> Option<PathBuf> {
    if !is_uuid(id) {
        return None;
    }
    fs::read_dir(home.join("sessions"))
        .ok()?
        .flatten()
        .map(|folder| folder.path().join(id).join("events.jsonl"))
        .find(|path| path.is_file())
}

/// The latest completed turn in the tail of Grok's `events.jsonl`: a
/// `turn_ended` record with outcome "completed" (Grok 1.0.70), named by its
/// time stamp. A turn begun, or ended otherwise ("error"), after it
/// supersedes it.
pub fn grok_turn_end(tail: &[u8]) -> Option<TurnEnd> {
    let mut superseded = false;
    for line in tail.split(|&b| b == b'\n').rev() {
        // Most lines are `phase_changed`; only turn records are parsed.
        if !line.windows(6).any(|w| w == b"\"turn_") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        match v.get("type").and_then(Value::as_str) {
            Some("turn_ended") if v.get("outcome").and_then(Value::as_str) == Some("completed") => {
                let ts = v.get("ts").and_then(Value::as_str)?;
                let at_ms = codex::parse_rfc3339_ms(ts).unwrap_or(0);
                return Some(TurnEnd { id: ts.to_string(), at_ms, superseded });
            }
            Some("turn_ended" | "turn_started") => superseded = true,
            _ => {}
        }
    }
    None
}

/// The latest completed turn of opencode session `id`: its newest "idle"
/// message whose outcome is "succeeded", named by its number in the session.
/// A message from the person after it supersedes it.
pub fn opencode_turn_end(db: &Path, id: &str) -> Option<TurnEnd> {
    if !is_opencode_id(id) || !db.is_file() {
        return None;
    }
    let query = format!(
        "select i.seq, i.time_created, exists(select 1 from session_message u where u.session_id = i.session_id \
         and u.type = 'user' and u.seq > i.seq) from session_message i where i.session_id = '{id}' \
         and i.type = 'idle' and json_extract(i.data, '$.outcome') = 'succeeded' order by i.seq desc limit 1"
    );
    let args = ["-init", "/dev/null", "-readonly", "-bail", "-noheader", "-cmd", ".timeout 100", db.to_str()?, &query];
    let out = crate::external::run_with_timeout("sqlite3", &args, std::time::Duration::from_millis(750))?;
    opencode_turn_row(std::str::from_utf8(&out).ok()?)
}

/// [`opencode_turn_end`]'s row as `sqlite3` prints it: number|time|later.
fn opencode_turn_row(out: &str) -> Option<TurnEnd> {
    let mut fields = out.trim().split('|');
    let seq: u64 = fields.next()?.parse().ok()?;
    let at_ms: u64 = fields.next()?.parse().ok()?;
    let superseded = fields.next()? == "1";
    Some(TurnEnd { id: seq.to_string(), at_ms, superseded })
}

/// The title opencode gave session `id`, read from its database with the
/// `sqlite3` command, opened read-only. opencode names a session
/// "New session - <time>" until it generates a title; that is no title.
pub fn opencode_title(db: &Path, id: &str) -> Option<String> {
    opencode_title_by(&[OsStr::new("sqlite3")], db, id)
}

/// [`opencode_title`], read with the `sqlite3` command line `sqlite3`.
fn opencode_title_by(sqlite3: &[&OsStr], db: &Path, id: &str) -> Option<String> {
    if !is_opencode_id(id) || !db.is_file() {
        return None;
    }
    let (program, lead) = sqlite3.split_first()?;
    let out = Command::new(program)
        .args(lead)
        .args(["-readonly", "-bail", "-cmd", ".timeout 500"])
        .arg(db)
        .arg(format!("select title from session_v2 where id = '{id}'"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let title = util::one_line(&String::from_utf8_lossy(&out.stdout));
    let placeholder = ["New session - ", "Child session - "].iter().any(|p| title.starts_with(p));
    (out.status.success() && !title.is_empty() && !placeholder).then_some(title)
}

#[cfg(test)]
#[path = "../../src/tests/other.rs"]
mod tests;
