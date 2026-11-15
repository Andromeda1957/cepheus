//! Antigravity CLI (`agy`) conversations. Antigravity keeps its state in `~/.gemini/antigravity-cli`:
//! `history.jsonl` records each submitted prompt with its conversation, workspace and time, and each
//! conversation is a SQLite database under `conversations/`; `conversation_summaries.db` says whether each
//! conversation's run is idle. The deck reads only those; it never opens Antigravity's sign-in files.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::monitor::TurnEnd;
use crate::protocol::{Conversation, Tool};
use crate::util;

/// How much of `history.jsonl`'s end is read.
const HISTORY_TAIL: u64 = 2 << 20;

/// One submitted prompt from `history.jsonl`.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub conversation: String,
    pub text: String,
    pub at_ms: u64,
    pub workspace: String,
    /// A slash command such as `/model`, not a message.
    pub command: bool,
}

/// Parse one `history.jsonl` line. Older lines name no conversation.
pub fn parse_prompt(line: &[u8]) -> Option<Prompt> {
    let v: Value = serde_json::from_slice(line).ok()?;
    let conversation = v.get("conversationId").and_then(Value::as_str).filter(|c| !c.is_empty())?.to_string();
    Some(Prompt {
        conversation,
        text: v.get("display").and_then(Value::as_str).unwrap_or("").to_string(),
        at_ms: v.get("timestamp").and_then(Value::as_u64).unwrap_or(0),
        workspace: v.get("workspace").and_then(Value::as_str).unwrap_or("").to_string(),
        command: v.get("type").and_then(Value::as_str) == Some("slash_command"),
    })
}

/// The recent prompts recorded in `dir`'s history, oldest first.
pub fn history(dir: &Path) -> Vec<Prompt> {
    let Ok(tail) = util::read_tail(&dir.join("history.jsonl"), HISTORY_TAIL) else { return Vec::new() };
    tail.split(|&b| b == b'\n').filter_map(parse_prompt).collect()
}

/// The database file of conversation `id`.
pub fn conversation_db(dir: &Path, id: &str) -> PathBuf {
    dir.join("conversations").join(format!("{id}.db"))
}

/// Antigravity's summary of every conversation, `conversation_summaries.db`.
pub fn summaries_db(dir: &Path) -> PathBuf {
    dir.join("conversation_summaries.db")
}

/// The latest completed turn of conversation `id` in the summaries database
/// `db`: Antigravity marks the conversation idle
/// (`CASCADE_RUN_STATUS_IDLE`, neither killed nor partly idle) when its turn
/// is over, named by the conversation's step count then. Its record does not
/// tell a turn interrupted from one completed.
pub fn turn_end(db: &Path, id: &str) -> Option<TurnEnd> {
    let valid = id.len() == 36 && id.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit());
    if !valid || !db.is_file() {
        return None;
    }
    let query = format!(
        "select step_count, status, killed, not_fully_idle, last_modified_time \
         from conversation_summaries where conversation_id = '{id}'"
    );
    let args = ["-init", "/dev/null", "-readonly", "-bail", "-noheader", "-cmd", ".timeout 100", db.to_str()?, &query];
    let out = crate::external::run_with_timeout("sqlite3", &args, std::time::Duration::from_millis(750))?;
    summary_turn_end(std::str::from_utf8(&out).ok()?)
}

/// [`turn_end`]'s row as `sqlite3` prints it.
fn summary_turn_end(row: &str) -> Option<TurnEnd> {
    let fields: Vec<&str> = row.trim().split('|').collect();
    let [steps, status, killed, partly_idle, modified] = fields.as_slice() else { return None };
    let steps: u64 = steps.parse().ok()?;
    if *status != "CASCADE_RUN_STATUS_IDLE" || *killed != "0" || *partly_idle != "0" || steps == 0 {
        return None;
    }
    let at_ms = crate::codex::parse_rfc3339_ms(&modified.replacen(' ', "T", 1)).unwrap_or(0);
    Some(TurnEnd { id: steps.to_string(), at_ms, superseded: false })
}

/// Conversations started in `workspace`, newest first: each titled with its
/// first message and dated by its prompts and its database.
pub fn conversations(dir: &Path, workspace: &Path, limit: usize) -> Vec<Conversation> {
    let workspace = workspace.to_string_lossy();
    let mut found: BTreeMap<String, Conversation> = BTreeMap::new();
    for p in history(dir).into_iter().filter(|p| p.workspace == workspace) {
        let at = p.at_ms / 1000;
        let entry = found.entry(p.conversation.clone()).or_insert_with(|| Conversation {
            search_words: Vec::new(),
            tool: Tool::Antigravity,
            id: p.conversation.clone(),
            title: String::new(),
            created_at: at,
            updated_at: at,
            open_in: None,
            host: None,
        });
        entry.created_at = entry.created_at.min(at);
        entry.updated_at = entry.updated_at.max(at);
        if entry.title.is_empty() && !p.command {
            entry.title = util::one_line(&p.text);
        }
    }
    let mut out: Vec<Conversation> = found
        .into_values()
        .filter(|c| crate::service::valid_conversation_id(&c.id))
        .map(|mut c| {
            if let Some(ms) = util::modified_ms(&conversation_db(dir, &c.id)) {
                c.updated_at = c.updated_at.max(ms / 1000);
            }
            if c.title.is_empty() {
                c.title = "Antigravity".to_string();
            }
            c
        })
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    out.truncate(limit);
    out
}

/// The first message of conversation `id`.
pub fn first_prompt(prompts: &[Prompt], id: &str) -> Option<String> {
    prompts.iter().find(|p| p.conversation == id && !p.command).map(|p| util::one_line(&p.text))
}

/// The conversation id of a path under `dir/conversations/`, if it is one.
pub fn conversation_of(dir: &Path, path: &Path) -> Option<String> {
    if path.parent()? != dir.join("conversations") {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    // SQLite opens `<id>.db-wal` and `<id>.db-shm` beside `<id>.db`.
    let id = name.split_once(".db").map(|(id, _)| id)?;
    crate::service::valid_conversation_id(id).then(|| id.to_string())
}

/// The conversations whose database a process has open.
pub fn open_conversations(dir: &Path, pid: u32) -> Vec<String> {
    let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
    // Descriptor targets are resolved paths; compare them with the resolved
    // directory too, as `monitor::rollouts_written_by` does for Codex.
    let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut out = Vec::new();
    for fd in fds.flatten() {
        let Ok(path) = fs::read_link(fd.path()) else { continue };
        if let Some(id) = conversation_of(&real, &path).or_else(|| conversation_of(dir, &path)) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// The conversation a new session started at `spawned_ms` in `workspace` is
/// in, from its first prompt in the history: the only conversation with a
/// prompt since then that no other session holds. Two such conversations
/// leave it unknown.
pub fn started_conversation(
    prompts: &[Prompt],
    workspace: &str,
    spawned_ms: u64,
    claimed: &HashSet<String>,
) -> Option<String> {
    let mut since: Vec<&str> = prompts
        .iter()
        .filter(|p| p.workspace == workspace && p.at_ms >= spawned_ms && !claimed.contains(&p.conversation))
        .map(|p| p.conversation.as_str())
        .collect();
    since.sort_unstable();
    since.dedup();
    match since.as_slice() {
        [only] => Some(only.to_string()),
        _ => None,
    }
}

#[cfg(test)]
#[path = "../../src/tests/antigravity.rs"]
mod tests;
