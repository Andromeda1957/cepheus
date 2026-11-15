//! What a session's agent is doing and the sub-agents it started, for the second line of its row. Claude Code
//! reports its sub-agents to the deck's hook (`claude_hook`). Codex writes a sub-agent's thread as a rollout of
//! its own whose `session_meta` names the parent thread.

use std::path::Path;

use serde_json::Value;

use crate::protocol::Subagent;
use crate::util;

/// How long a Codex sub-agent's thread counts as running after it last
/// wrote, when its turn state is unknown.
pub const CODEX_IDLE_LIMIT_SECS: u64 = 120;
/// Finished sub-agents older than this are left out.
pub const FINISHED_KEEP_SECS: u64 = 6 * 3600;
/// At most this many sub-agents per session, running ones first.
pub const MAX_LISTED: usize = 24;
/// The longest activity text.
const ACTIVITY_WIDTH: usize = 72;

/// A Claude sub-agent as Claude's hooks reported it.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClaudeAgent {
    /// Claude's id for it: `agent_id` in `SubagentStart`, `id` in
    /// `background_tasks`.
    pub id: String,
    /// `started_at` is when the deck first heard of it; `last_active`, when
    /// Claude last reported it running or the report that no longer did.
    pub agent: Subagent,
}

/// Take one of Claude's hook reports into a session's sub-agents; false when
/// it reports none.
///
/// Claude Code (2.1.319) gives its `Stop` and `SubagentStop` hooks the
/// session's in-flight background work, `background_tasks`, from the one task
/// registry its main thread and every sub-agent share. The `subagent` entries
/// running or pending in it are the agents Claude's own "Waiting for N
/// background agents to finish" counts, nested ones included; it leaves out a
/// backgrounded main session (`main-session`). An agent stopped, killed or
/// done is gone from the next report; one whose turn ended while its own
/// background work runs stays in it. `SubagentStart` names an agent as it
/// starts, before any report lists it.
pub fn claude_hook(agents: &mut Vec<ClaudeAgent>, payload: &Value, now: u64) -> bool {
    match text(payload, "hook_event_name") {
        "SubagentStart" if !text(payload, "agent_id").is_empty() => {
            mark_running(agents, text(payload, "agent_id"), text(payload, "agent_type"), "", now);
        }
        "Stop" | "SubagentStop" => {
            let Some(tasks) = payload.get("background_tasks").and_then(Value::as_array) else { return false };
            let listed: Vec<&Value> = tasks
                .iter()
                .filter(|t| {
                    text(t, "type") == "subagent"
                        && matches!(text(t, "status"), "running" | "pending")
                        && text(t, "agent_type") != "main-session"
                        && !text(t, "id").is_empty()
                })
                .collect();
            for a in agents.iter_mut().filter(|a| a.agent.active) {
                if !listed.iter().any(|t| text(t, "id") == a.id) {
                    a.agent.active = false;
                    a.agent.last_active = now;
                }
            }
            for t in listed {
                mark_running(agents, text(t, "id"), text(t, "agent_type"), text(t, "description"), now);
            }
        }
        _ => return false,
    }
    agents.retain(|a| a.agent.active || now.saturating_sub(a.agent.last_active) < FINISHED_KEEP_SECS);
    // Running ones newest first, then the most recently ended.
    agents.sort_by_key(|a| {
        std::cmp::Reverse((a.agent.active, if a.agent.active { a.agent.started_at } else { a.agent.last_active }))
    });
    // Every running one stays listed: the row counts them all.
    let running = agents.iter().filter(|a| a.agent.active).count();
    agents.truncate(running.max(MAX_LISTED));
    true
}

/// The sub-agents a Claude session shows: only what its hooks reported.
pub fn claude_listed(agents: &[ClaudeAgent]) -> Vec<Subagent> {
    agents.iter().map(|a| a.agent.clone()).collect()
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Mark the agent `id` running, adding it when new; it is named by what it
/// was asked to do, else its type.
fn mark_running(agents: &mut Vec<ClaudeAgent>, id: &str, kind: &str, description: &str, now: u64) {
    let kind = Some(util::one_line(kind)).filter(|k| !k.is_empty());
    let name = Some(util::one_line(description)).filter(|d| !d.is_empty());
    match agents.iter_mut().find(|a| a.id == id) {
        Some(known) => {
            known.agent.active = true;
            known.agent.last_active = now;
            if let Some(name) = name {
                known.agent.name = name;
            }
            if kind.is_some() {
                known.agent.kind = kind;
            }
        }
        None => agents.push(ClaudeAgent {
            id: id.to_string(),
            agent: Subagent {
                name: name.or_else(|| kind.clone()).unwrap_or_else(|| "sub-agent".to_string()),
                kind,
                active: true,
                started_at: now,
                last_active: now,
            },
        }),
    }
}

/// Running sub-agents first, newest first within each, finished ones only
/// while recent, at most `MAX_LISTED`.
pub fn keep_recent(mut agents: Vec<Subagent>, now: u64) -> Vec<Subagent> {
    agents.retain(|a| a.active || now.saturating_sub(a.last_active) < FINISHED_KEEP_SECS);
    agents.sort_by(|a, b| b.active.cmp(&a.active).then(b.last_active.cmp(&a.last_active)));
    agents.truncate(MAX_LISTED);
    agents
}

/// A tool call as the row shows it: the tool and what it works on.
pub fn describe_tool(name: &str, input: &Value) -> String {
    let text = |k: &str| input.get(k).and_then(Value::as_str).map(util::one_line).filter(|s| !s.is_empty());
    let file =
        |k: &str| text(k).map(|p| Path::new(&p).file_name().map_or(p.clone(), |n| n.to_string_lossy().into_owned()));
    let described = match name {
        "Bash" => text("description").or_else(|| text("command")).map(|d| format!("Bash: {d}")),
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            file("file_path").or_else(|| file("notebook_path")).map(|f| format!("{name} {f}"))
        }
        "Grep" | "Glob" => text("pattern").map(|p| format!("{name} \u{201c}{p}\u{201d}")),
        "Agent" | "Task" => text("description").map(|d| format!("Agent: {d}")),
        "WebFetch" => text("url").map(|u| format!("Fetch {}", host_of(&u))),
        "WebSearch" => text("query").map(|q| format!("Search \u{201c}{q}\u{201d}")),
        "TodoWrite" => Some("Updating its todo list".to_string()),
        "Skill" => text("skill").map(|s| format!("Skill {s}")),
        _ => None,
    };
    let described = described.unwrap_or_else(|| match name.strip_prefix("mcp__") {
        Some(rest) => {
            let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
            format!("{server}: {tool}")
        }
        None => name.to_string(),
    });
    util::truncate_width(&described, ACTIVITY_WIDTH)
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest).to_string()
}

/// The last tool call in the end of a Claude transcript.
pub fn claude_activity(tail: &[u8]) -> Option<String> {
    for line in tail.split(|&b| b == b'\n').rev() {
        if !util::contains(line, b"\"tool_use\"") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(parts) = v.get("message").and_then(|m| m.get("content")).and_then(Value::as_array) else {
            continue;
        };
        if let Some(call) = parts.iter().rev().find(|p| p.get("type").and_then(Value::as_str) == Some("tool_use")) {
            let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
            return Some(describe_tool(name, call.get("input").unwrap_or(&Value::Null)));
        }
    }
    None
}

/// The last command, edit or tool call in the end of a Codex rollout.
pub fn codex_activity(tail: &[u8]) -> Option<String> {
    for line in tail.split(|&b| b == b'\n').rev() {
        let interesting = util::contains(line, b"\"item_completed\"")
            || util::contains(line, b"\"item_started\"")
            || util::contains(line, b"\"function_call\"");
        if !interesting {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        let Some(p) = v.get("payload") else { continue };
        if let Some(found) = codex_item(p.get("item")) {
            return Some(util::truncate_width(&found, ACTIVITY_WIDTH));
        }
        if p.get("type").and_then(Value::as_str) == Some("function_call") {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = p
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                .unwrap_or(Value::Null);
            let cmd = args.get("cmd").or_else(|| args.get("command")).map(command_text);
            return Some(util::truncate_width(
                &match cmd {
                    Some(c) if !c.is_empty() => format!("Ran: {c}"),
                    _ => name.to_string(),
                },
                ACTIVITY_WIDTH,
            ));
        }
    }
    None
}

fn command_text(v: &Value) -> String {
    match v {
        Value::String(s) => util::one_line(s),
        // `["zsh", "-lc", "cargo test"]`: the script, not the shell.
        Value::Array(parts) => {
            let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            match words.as_slice() {
                [_, flag, script] if flag.starts_with('-') && flag.contains('c') => util::one_line(script),
                _ => util::one_line(&words.join(" ")),
            }
        }
        _ => String::new(),
    }
}

fn codex_item(item: Option<&Value>) -> Option<String> {
    let item = item?;
    let text = |k: &str| item.get(k).and_then(Value::as_str).map(util::one_line).filter(|s| !s.is_empty());
    match item.get("type").and_then(Value::as_str)? {
        "CommandExecution" => {
            let parsed = item
                .get("parsed_cmd")
                .and_then(Value::as_array)
                .and_then(|p| p.first())
                .and_then(|c| c.get("cmd"))
                .and_then(Value::as_str)
                .map(util::one_line);
            let cmd = parsed.or_else(|| item.get("command").map(command_text)).unwrap_or_default();
            Some(format!("Ran: {cmd}"))
        }
        "FileChange" => {
            let changes = item.get("changes").and_then(Value::as_object)?;
            let first = changes.keys().next()?;
            let name = Path::new(first).file_name().map_or(first.clone(), |n| n.to_string_lossy().into_owned());
            Some(match changes.len() {
                1 => format!("Edit {name}"),
                n => format!("Edit {name} and {} more", n - 1),
            })
        }
        "McpToolCall" => {
            let server = text("server").unwrap_or_default();
            let tool = text("tool").unwrap_or_default();
            Some(format!("{server}: {tool}"))
        }
        "WebSearch" => Some(format!("Search \u{201c}{}\u{201d}", text("query").unwrap_or_default())),
        _ => None,
    }
}

/// A Codex sub-agent thread as a row's sub-agent: `nickname` is the name
/// Codex gave it, `path` its task path (`/root/review_web_ci_collection`).
pub fn codex_subagent(
    nickname: Option<&str>,
    path: Option<&str>,
    created_ms: u64,
    last_active: u64,
    running: Option<bool>,
    now: u64,
) -> Subagent {
    let task = path.and_then(|p| p.rsplit('/').next()).filter(|t| !t.is_empty()).map(|t| t.replace('_', " "));
    let name = match (nickname, task) {
        (Some(n), Some(t)) => format!("{n}: {t}"),
        (Some(n), None) => n.to_string(),
        (None, Some(t)) => t,
        (None, None) => "sub-agent".to_string(),
    };
    let active = running.unwrap_or_else(|| now.saturating_sub(last_active) < CODEX_IDLE_LIMIT_SECS);
    Subagent { name, kind: None, active, started_at: created_ms / 1000, last_active }
}

#[cfg(test)]
#[path = "../../src/tests/activity.rs"]
mod tests;
