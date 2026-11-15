//! Read-only views of Claude Code's own session records and transcripts.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::protocol::{Conversation, Tool};
use crate::util;

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeProc {
    pub pid: u32,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub name: Option<String>,
    pub busy: Option<bool>,
    pub interactive: bool,
    pub proc_start: Option<u64>,
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

pub fn parse_session_file(v: &Value) -> Option<ClaudeProc> {
    let pid = as_u64(v.get("pid")?)? as u32;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Some(ClaudeProc {
        pid,
        session_id: s("sessionId"),
        cwd: s("cwd"),
        // Claude names a session after its folder ("my-project", source
        // "derived") until the conversation has a name of its own; that is no
        // title.
        name: s("name")
            .filter(|_| s("nameSource").as_deref() != Some("derived"))
            .map(|n| util::one_line(&n))
            .filter(|n| !n.is_empty()),
        busy: s("status").map(|st| st == "busy"),
        interactive: s("kind").is_none_or(|k| k == "interactive"),
        proc_start: v.get("procStart").and_then(as_u64),
    })
}

/// Live Claude processes as recorded in `<claude>/sessions/<pid>.json`.
pub fn session_files(claude_dir: &Path) -> Vec<ClaudeProc> {
    let Ok(dir) = fs::read_dir(claude_dir.join("sessions")) else { return Vec::new() };
    dir.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter_map(|v| parse_session_file(&v))
        .collect()
}

/// Claude's project directory name for a working directory.
pub fn project_slug(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

pub fn transcript_path(claude_dir: &Path, cwd: &str, session_id: &str) -> PathBuf {
    claude_dir.join("projects").join(project_slug(cwd)).join(format!("{session_id}.jsonl"))
}

/// A message's text: the whole content, or its first text part.
fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => items
            .iter()
            .find(|i| i.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|i| i.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// What the owner typed, from a record of it: a prompt Claude took at once
/// (`user`), or one typed while it worked (a `queued_command` attachment).
/// Meta records, the summary that continues a compacted session, tool
/// results, task notifications and command output are not typed.
pub(crate) fn typed_text(v: &Value) -> Option<String> {
    let flag = |k: &str| v.get(k).and_then(Value::as_bool) == Some(true);
    let text = match v.get("type")?.as_str()? {
        "user" if !flag("isMeta") && !flag("isCompactSummary") => {
            let content = v.get("message")?.get("content")?;
            // Hook output can ride beside a tool result as text of its own.
            let tool_result = content.as_array().is_some_and(|items| {
                items.iter().any(|i| i.get("type").and_then(Value::as_str) == Some("tool_result"))
            });
            if tool_result {
                return None;
            }
            content_text(content)?
        }
        "attachment" => {
            let a = v.get("attachment")?;
            let field = |k: &str| a.get(k).and_then(Value::as_str);
            let human =
                a.get("origin").and_then(|o| o.get("kind")).and_then(Value::as_str).is_none_or(|k| k == "human");
            if field("type") != Some("queued_command") || field("commandMode") != Some("prompt") || !human {
                return None;
            }
            content_text(a.get("prompt")?)?
        }
        _ => return None,
    };
    let text = text.trim();
    // Claude writes "[Request interrupted by user]" (or "… for tool use") as a user record of its own when the
    // owner interrupts; it became a title.
    let generated =
        text.starts_with('<') || text.starts_with("Caveat:") || text.starts_with("[Request interrupted by user");
    (!text.is_empty() && !generated).then(|| text.to_string())
}

/// The names of the Claude agents defined in `dirs` (`<dir>/agents/<name>.md`).
/// Claude names an agent session's terminal after its agent, which names no
/// conversation.
pub fn agent_names(dirs: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = dirs
        .iter()
        .filter_map(|dir| fs::read_dir(dir.join("agents")).ok())
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().is_some_and(|e| e == "md")).then(|| path.file_stem()?.to_str().map(str::to_string))?
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

fn user_text(v: &Value) -> Option<String> {
    if v.get("type")?.as_str()? != "user" {
        return None;
    }
    Some(util::truncate_width(&util::one_line(&typed_text(v)?), 100))
}

/// The last task the owner gave in whole transcript lines.
pub fn last_task(lines: &[u8]) -> Option<util::TaskPrompt> {
    // Tool results are `user` records too, and the largest lines; text the
    // owner typed carries no unescaped `"tool_use_id"`.
    let keep = |line: &[u8]| {
        (util::contains(line, br#""type":"user""#) && !util::contains(line, br#""tool_use_id""#))
            || util::contains(line, br#""queued_command""#)
    };
    util::last_task_prompt(lines, keep, typed_text)
}

/// What a transcript record says the owner typed or Claude said and ran, or None for context attached to the
/// conversation: hook output such as a SessionStart summary naming every worktree, API request copies,
/// file-history snapshots and meta records. Tool output is dropped too: it is what Claude read, not where it
/// works, and a listing names every worktree.
pub fn conversation_part(mut record: Value) -> Option<Value> {
    match record.get("type").and_then(Value::as_str) {
        Some("assistant" | "queue-operation") => Some(record),
        Some("user") if record.get("isMeta").and_then(Value::as_bool) != Some(true) => {
            if let Some(fields) = record.as_object_mut() {
                fields.remove("toolUseResult");
            }
            if let Some(items) = record.pointer_mut("/message/content").and_then(Value::as_array_mut) {
                items.retain(|item| item.get("type").and_then(Value::as_str) != Some("tool_result"));
            }
            Some(record)
        }
        _ => None,
    }
}

/// Claude's generated title and the agent it runs as, from whole transcript
/// lines: the latest `ai-title` and `agent-setting` records update them.
/// Claude names an agent session's terminal after the agent, so that title
/// names no conversation; only some conversations get an `ai-title`.
pub fn title_records(lines: &[u8], ai_title: &mut Option<String>, agent: &mut Option<String>) {
    const TITLE: &[u8] = br#""type":"ai-title""#;
    const AGENT: &[u8] = br#""type":"agent-setting""#;
    for line in lines.split(|&b| b == b'\n') {
        if !util::contains(line, TITLE) && !util::contains(line, AGENT) {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(line) else { continue };
        let text = |key: &str| record.get(key).and_then(Value::as_str).map(util::one_line).filter(|t| !t.is_empty());
        match record.get("type").and_then(Value::as_str) {
            Some("ai-title") => *ai_title = text("aiTitle").or(ai_title.take()),
            Some("agent-setting") => *agent = text("agentSetting").or(agent.take()),
            _ => {}
        }
    }
}

/// The first prompt the owner typed in a transcript.
pub fn first_prompt(path: &Path) -> Option<String> {
    let head = util::read_head(path, 512 * 1024).ok()?;
    head.split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find_map(|v| user_text(&v))
}

/// Recent conversations recorded for `start_dir`, newest first.
pub fn conversations(claude_dir: &Path, start_dir: &Path, limit: usize) -> Vec<Conversation> {
    let dir = claude_dir.join("projects").join(project_slug(&start_dir.to_string_lossy()));
    let Ok(entries) = fs::read_dir(&dir) else { return Vec::new() };
    let mut files: Vec<(PathBuf, u64, u64)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            let created = meta
                .created()
                .ok()
                .and_then(|c| c.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(modified);
            Some((e.path(), modified, created))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    files
        .into_iter()
        .filter_map(|(path, updated_at, created_at)| {
            let id = path.file_stem()?.to_str()?.to_string();
            let title = first_prompt(&path)?;
            Some(Conversation {
                search_words: Vec::new(),
                tool: Tool::Claude,
                id,
                title,
                created_at,
                updated_at,
                open_in: None,
                host: None,
            })
        })
        .take(limit)
        .collect()
}

#[cfg(test)]
#[path = "../../src/tests/claude.rs"]
mod tests;
