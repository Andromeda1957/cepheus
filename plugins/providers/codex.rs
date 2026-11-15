//! Read-only views of Codex's rollout files and thread index.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::monitor::TurnEnd;
use crate::protocol::{Conversation, Tool, UsageWindow};
use crate::util;

#[derive(Debug, Clone, PartialEq)]
pub struct RolloutMeta {
    pub id: String,
    pub cwd: String,
    pub created_ms: u64,
    /// A person started the thread; Codex did not spawn it for a sub-agent
    /// or a review.
    pub user_thread: bool,
    /// The thread that spawned this one as a sub-agent, and the name and
    /// path Codex gave it (`agent_nickname`, `agent_path`).
    pub parent: Option<String>,
    pub nickname: Option<String>,
    pub agent_path: Option<String>,
}

fn sorted_dirs_desc(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(path) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs.reverse();
    dirs
}

/// The day folders searched for a conversation's rollout: more than a year of
/// days on which Codex ran.
const HISTORY_DAYS: usize = 400;

/// Day directories under `<codex>/sessions/YYYY/MM/DD`, newest first.
pub fn day_dirs(codex_dir: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for year in sorted_dirs_desc(&codex_dir.join("sessions")) {
        for month in sorted_dirs_desc(&year) {
            for day in sorted_dirs_desc(&month) {
                out.push(day);
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

/// Whether a path names a Codex rollout file.
pub fn is_rollout_path(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.starts_with("rollout-") && name.ends_with(".jsonl")
}

/// Rollout files in the given day directories with their modification time.
pub fn rollouts_in(dirs: &[PathBuf]) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if !is_rollout_path(&path) {
                continue;
            }
            let modified = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            out.push((path, modified));
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.1));
    out
}

/// Parse RFC 3339 timestamps such as `2026-10-25T17:12:53.123Z` to unix ms.
pub fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0i64)
    } else if let Some(pos) = rest.rfind(['+', '-']) {
        let (t, off) = rest.split_at(pos);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let mut parts = off[1..].split(':');
        let oh: i64 = parts.next()?.parse().ok()?;
        let om: i64 = parts.next().unwrap_or("0").parse().ok()?;
        (t, sign * (oh * 3600 + om * 60))
    } else {
        (rest, 0)
    };
    let mut tp = time.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let sec_str = tp.next().unwrap_or("0");
    let (sec, frac) = sec_str.split_once('.').unwrap_or((sec_str, ""));
    let sec: i64 = sec.parse().ok()?;
    let ms: i64 = format!("{:0<3}", &frac[..frac.len().min(3)]).parse().unwrap_or(0);
    // Days from civil (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = (if y2 >= 0 { y2 } else { y2 - 399 }) / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total = days * 86_400 + h * 3600 + mi * 60 + sec - offset_secs;
    (total >= 0).then(|| (total * 1000 + ms) as u64)
}

/// Whether a person started the thread (the TUI, an IDE, `codex exec`).
/// Codex marks the threads it spawns itself `thread_source: "subagent"` or
/// `"guardian_review"`; versions without that field give them a
/// `{"subagent": ...}` source.
fn started_by_user(payload: &Value) -> bool {
    match payload.get("thread_source").and_then(Value::as_str) {
        Some(source) => source == "user",
        None => payload.get("source").and_then(|s| s.get("subagent")).is_none(),
    }
}

pub fn parse_meta_line(line: &[u8]) -> Option<RolloutMeta> {
    let v: Value = serde_json::from_slice(line).ok()?;
    if v.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let p = v.get("payload")?;
    let id = p.get("id")?.as_str()?.to_string();
    let cwd = p.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
    let created_ms = p
        .get("timestamp")
        .and_then(Value::as_str)
        .or_else(|| v.get("timestamp").and_then(Value::as_str))
        .and_then(parse_rfc3339_ms)
        .unwrap_or(0);
    let spawn = p.get("source").and_then(|s| s.get("subagent")).and_then(|s| s.get("thread_spawn"));
    let text = |key: &str| {
        p.get(key)
            .or_else(|| spawn.and_then(|s| s.get(key)))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(RolloutMeta {
        id,
        cwd,
        created_ms,
        user_thread: started_by_user(p),
        parent: text("parent_thread_id"),
        nickname: text("agent_nickname"),
        agent_path: text("agent_path"),
    })
}

pub fn read_meta(path: &Path) -> Option<RolloutMeta> {
    let head = util::read_head(path, 256 * 1024).ok()?;
    let line = head.split(|&b| b == b'\n').next()?;
    parse_meta_line(line)
}

/// Fields that hold what a command or tool printed or returned, in every
/// Codex record that carries one: `item_completed` items (CommandExecution,
/// FileChange, McpToolCall, Extension), older `exec_command_end` and
/// `patch_apply_end` events and `*_output` response items.
const OUTPUT_FIELDS: [&str; 8] =
    ["aggregated_output", "formatted_output", "stdout", "stderr", "output", "result", "results", "error"];

fn drop_output(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|k, _| !OUTPUT_FIELDS.contains(&k.as_str()));
            fields.values_mut().for_each(drop_output);
        }
        Value::Array(items) => items.iter_mut().for_each(drop_output),
        _ => {}
    }
}

/// What a rollout record says the owner typed or Codex said and ran, or None
/// for context Codex or a hook supplied: session and turn settings,
/// `world_state` (which carries AGENTS.md), developer messages (where hook
/// context lands) and compaction summaries. Tool output is dropped too: a
/// `task_state.py list` at startup names every worktree.
pub fn conversation_part(mut record: Value) -> Option<Value> {
    match record.get("type").and_then(Value::as_str) {
        Some("event_msg") => {}
        Some("response_item") => {
            let field = |k: &str| record.get("payload").and_then(|p| p.get(k)).and_then(Value::as_str);
            if field("type") == Some("message") && matches!(field("role"), Some("developer" | "system")) {
                return None;
            }
        }
        _ => return None,
    }
    drop_output(&mut record);
    Some(record)
}

/// Whether the latest turn in a rollout tail is still running.
pub fn turn_state(tail: &[u8]) -> Option<bool> {
    for line in tail.split(|&b| b == b'\n').rev() {
        if !line.windows(9).any(|w| w == b"event_msg") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        match v.get("payload").and_then(|p| p.get("type")).and_then(Value::as_str) {
            Some("task_started") => return Some(true),
            Some("task_complete") | Some("turn_aborted") => return Some(false),
            _ => {}
        }
    }
    None
}

/// The latest completed turn (`task_complete`) in a rollout tail, named by
/// its turn id (else its time stamp). A turn begun or interrupted after it
/// supersedes it.
pub fn turn_end(tail: &[u8]) -> Option<TurnEnd> {
    let mut superseded = false;
    for line in tail.split(|&b| b == b'\n').rev() {
        let has = |word: &[u8]| line.windows(word.len()).any(|w| w == word);
        if !(has(b"task_complete") || has(b"task_started") || has(b"turn_aborted")) {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        match v.get("payload").and_then(|p| p.get("type")).and_then(Value::as_str) {
            Some("task_complete") => {
                let at = v.get("timestamp").and_then(Value::as_str);
                let id = v["payload"].get("turn_id").and_then(Value::as_str).or(at)?;
                let at_ms = at.and_then(parse_rfc3339_ms).unwrap_or(0);
                return Some(TurnEnd { id: id.to_string(), at_ms, superseded });
            }
            Some("task_started" | "turn_aborted") => superseded = true,
            _ => {}
        }
    }
    None
}

/// Threads whose persistent goal is still active, independently of turn ends.
/// Read only IDs and status from Codex's own database; never goal objectives.
/// Unavailable databases/tools return unknown, preserving the last known state.
pub fn active_goals(codex_dir: &Path) -> Option<HashSet<String>> {
    active_goals_by("sqlite3", &[], codex_dir)
}

fn active_goals_by(program: &str, lead: &[&str], codex_dir: &Path) -> Option<HashSet<String>> {
    let db = codex_dir.join("goals_1.sqlite");
    if !db.is_file() {
        return None;
    }
    let mut args = lead.to_vec();
    args.extend([
        "-init",
        "/dev/null",
        "-readonly",
        "-bail",
        "-noheader",
        "-cmd",
        ".timeout 100",
        db.to_str()?,
        "SELECT thread_id FROM thread_goals WHERE status = 'active'",
    ]);
    let out = crate::external::run_with_timeout(program, &args, std::time::Duration::from_millis(750))?;
    Some(std::str::from_utf8(&out).ok()?.lines().filter(|id| !id.is_empty()).map(str::to_string).collect())
}

fn window_label(minutes: u64) -> String {
    match minutes {
        300 => "5h".to_string(),
        10080 => "wk".to_string(),
        m if m >= 1440 && m % 1440 == 0 => format!("{}d", m / 1440),
        m if m >= 60 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

/// The latest rate-limit sample in a rollout tail: when Codex recorded it
/// (unix ms) and its windows. A record without its own `timestamp` is dated
/// `fallback_ms`. The time decides between samples from different sessions:
/// the file written last can hold the older sample.
pub fn rate_limits(tail: &[u8], fallback_ms: u64, now: u64) -> Option<(u64, Vec<UsageWindow>)> {
    for line in tail.split(|&b| b == b'\n').rev() {
        if !line.windows(11).any(|w| w == b"rate_limits") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        let Some(rl) = v.get("payload").and_then(|p| p.get("rate_limits")).filter(|r| r.is_object()) else { continue };
        let at_ms = v.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms).unwrap_or(fallback_ms);
        let updated_at = at_ms / 1000;
        let mut out = Vec::new();
        for key in ["primary", "secondary"] {
            let Some(w) = rl.get(key).filter(|w| w.is_object()) else { continue };
            let Some(used) = w.get("used_percent").and_then(Value::as_f64) else { continue };
            let minutes = w.get("window_minutes").and_then(Value::as_u64).unwrap_or(0);
            let resets_at = w.get("resets_at").and_then(util::json_epoch_secs);
            if resets_at.is_some_and(|r| r < now) {
                continue;
            }
            out.push(UsageWindow { label: window_label(minutes), used_percent: Some(used), resets_at, updated_at });
        }
        return Some((at_ms, out));
    }
    None
}

/// A thread's name and when Codex gave it.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadName {
    pub name: String,
    pub at_ms: Option<u64>,
}

impl ThreadName {
    /// Whether the owner gave the thread `task` after it was named: Codex
    /// names a thread after its first task and never again, so the name
    /// then describes work that is over.
    pub fn outdated_by(&self, task: &util::TaskPrompt) -> bool {
        self.at_ms.is_some_and(|at| task.at_ms > at)
    }
}

/// Latest thread names from `<codex>/session_index.jsonl`.
pub fn thread_names(codex_dir: &Path) -> HashMap<String, ThreadName> {
    let mut out = HashMap::new();
    let Ok(text) = fs::read_to_string(codex_dir.join("session_index.jsonl")) else { return out };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let (Some(id), Some(name)) =
            (v.get("id").and_then(Value::as_str), v.get("thread_name").and_then(Value::as_str))
        else {
            continue;
        };
        let name = util::one_line(name);
        if !name.is_empty() {
            let at_ms = v.get("updated_at").and_then(Value::as_str).and_then(parse_rfc3339_ms);
            out.insert(id.to_string(), ThreadName { name, at_ms });
        }
    }
    out
}

fn message_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .find(|t| !t.trim().is_empty())
            .map(str::to_string),
        _ => None,
    }
}

/// What the owner typed, from a rollout record: a user message, without the
/// instructions and context Codex sends as user messages of its own.
fn typed_text(v: &Value) -> Option<String> {
    let p = v.get("payload")?;
    let text = match (v.get("type").and_then(Value::as_str), p.get("type").and_then(Value::as_str)) {
        (Some("event_msg"), Some("user_message")) => p.get("message").and_then(Value::as_str).map(str::to_string),
        (Some("response_item"), Some("message")) if p.get("role").and_then(Value::as_str) == Some("user") => {
            p.get("content").and_then(message_text)
        }
        _ => None,
    }?;
    let text = text.trim();
    (!text.is_empty() && !text.starts_with('<') && !text.starts_with("# AGENTS.md")).then(|| text.to_string())
}

/// The last task the owner gave in whole rollout lines.
pub fn last_task(lines: &[u8]) -> Option<util::TaskPrompt> {
    let keep = |line: &[u8]| util::contains(line, br#""role":"user""#) || util::contains(line, b"\"user_message\"");
    util::last_task_prompt(lines, keep, typed_text)
}

/// The first prompt the owner typed in a rollout.
pub fn first_user_message(path: &Path) -> Option<String> {
    let head = util::read_head(path, 1024 * 1024).ok()?;
    for line in head.split(|&b| b == b'\n') {
        if !util::contains(line, b"user") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        let Some(text) = typed_text(&v) else { continue };
        return Some(util::truncate_width(&util::one_line(&text), 100));
    }
    None
}

/// Find a thread's rollout file, newest day directories first.
pub fn find_rollout(codex_dir: &Path, id: &str) -> Option<PathBuf> {
    find_recent_rollout(codex_dir, id, HISTORY_DAYS)
}

/// `find_rollout` over only the newest `days` day directories: a thread
/// created since an earlier look is written under the current day's.
pub fn find_recent_rollout(codex_dir: &Path, id: &str, days: usize) -> Option<PathBuf> {
    let suffix = format!("-{id}.jsonl");
    for day in day_dirs(codex_dir, days) {
        let Ok(entries) = fs::read_dir(&day) else { continue };
        for e in entries.flatten() {
            if e.file_name().to_str().is_some_and(|n| n.ends_with(&suffix)) {
                return Some(e.path());
            }
        }
    }
    None
}

/// Whether a live process holds Codex's writer lock on thread `id`
/// (`$CODEX_HOME/thread-writer-locks/<id>.lock`, an flock). The shared
/// background server keeps it for about 30 minutes after a client leaves
/// (Codex 0.164 `thread_unload_delay_secs`), and a Codex that cannot take it
/// cannot write the conversation.
pub fn thread_locked(codex_dir: &Path, id: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let path = codex_dir.join("thread-writer-locks").join(format!("{id}.lock"));
    let (Ok(meta), Ok(real)) = (fs::metadata(&path), fs::canonicalize(&path)) else { return false };
    let Ok(locks) = fs::read_to_string("/proc/locks") else { return false };
    lock_holders(&locks, meta.ino()).into_iter().any(|pid| match pid {
        // An open-file-description lock names no process.
        None => true,
        Some(pid) => holds_open(pid, &real),
    })
}

/// The processes holding an flock on inode `ino` in a `/proc/locks` table
/// (`None` for a lock without a process). Inode numbers repeat across
/// filesystems, so a caller confirms the process has the file open.
pub fn lock_holders(table: &str, ino: u64) -> Vec<Option<u32>> {
    table
        .lines()
        .filter(|line| !line.contains("->"))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.get(1) != Some(&"FLOCK") {
                return None;
            }
            let file_ino = fields.get(5)?.rsplit(':').next()?.parse::<u64>().ok()?;
            (file_ino == ino).then(|| fields.get(4).and_then(|p| p.parse::<u32>().ok()))
        })
        .collect()
}

/// Whether process `pid` has `path` open; a process whose descriptors cannot
/// be read is assumed to.
fn holds_open(pid: u32, path: &Path) -> bool {
    let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else { return true };
    fds.flatten().any(|fd| fs::read_link(fd.path()).is_ok_and(|target| target == path))
}

/// The rollout of a thread a person started. Codex also reports turns of
/// threads it runs for itself: sub-agents, and the temporary thread that names
/// a conversation, which writes no rollout at all.
pub fn user_rollout(codex_dir: &Path, id: &str) -> Option<PathBuf> {
    let path = find_rollout(codex_dir, id)?;
    read_meta(&path).filter(|m| m.user_thread && m.id == id).map(|_| path)
}

/// The conversations whose working directory is `start_dir` that were
/// written most recently, newest first. A rollout is filed under the day its
/// conversation began, and one resumed since is among the newest, so every
/// day folder is read, not only the newest ones.
pub fn conversations(codex_dir: &Path, start_dir: &Path, limit: usize) -> Vec<Conversation> {
    let names = thread_names(codex_dir);
    let start = start_dir.to_string_lossy();
    let mut out = Vec::new();
    for (path, modified_ms) in rollouts_in(&day_dirs(codex_dir, HISTORY_DAYS)) {
        if out.len() >= limit {
            break;
        }
        let Some(meta) = read_meta(&path) else { continue };
        if meta.cwd != start || !meta.user_thread {
            continue;
        }
        let title = names.get(&meta.id).map(|n| n.name.clone()).or_else(|| first_user_message(&path));
        let Some(title) = title else { continue };
        out.push(Conversation {
            search_words: Vec::new(),
            tool: Tool::Codex,
            id: meta.id,
            title,
            created_at: meta.created_ms / 1000,
            updated_at: modified_ms / 1000,
            open_in: None,
            host: None,
        });
    }
    out
}

#[cfg(test)]
#[path = "../../src/tests/codex.rs"]
mod tests;
