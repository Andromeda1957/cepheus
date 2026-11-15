//! A hosted session's conversation as chat items for the phone, read from
//! the program's own log: Claude Code's transcript, Codex's rollout, or the
//! database of opencode or Antigravity ([`super::agy`]); Grok shows its screen
//! instead.
//!
//! Only records the program injected are left out (hook output, command
//! tags, `isMeta`, sidechains, developer and environment context). The deck's
//! title rules do not apply here: short replies, questions and multiline text
//! stay whole (review AR-20261027-339e742b7b).

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[path = "questions.rs"]
mod questions;

use super::{agy, streaming};
use crate::protocol::SessionInfo;
#[cfg(test)]
use crate::protocol::Tool;
use crate::{codex, other, util};

/// Text kept of each item, in characters.
pub(crate) const TEXT_MAX: usize = 24_000;
const TOOL_INPUT_MAX: usize = 3_000;
pub(super) const RESULT_MAX: usize = 6_000;
pub(super) const SUMMARY_MAX: usize = 160;
/// Items kept per conversation; older ones are dropped from the front.
const ITEMS_MAX: usize = 3_000;
/// A log is read at most this often, and a screen-only session's screen.
const REFRESH: Duration = Duration::from_millis(700);
const FILE_REFRESH: Duration = Duration::from_millis(80);
/// Conversations kept parsed.
const CACHE_MAX: usize = 8;
/// Bytes of a log read at once.
const READ_MAX: u64 = 24 << 20;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChatQuestion {
    pub id: String,
    pub title: String,
    pub options: Vec<String>,
    #[serde(default)]
    pub answered: bool,
}

/// Pending questions in a legacy peer's retained history.
pub fn pending_questions(items: &[Item]) -> Vec<&ChatQuestion> {
    items
        .iter()
        .filter_map(|item| match item {
            Item::Assistant { questions, .. } => Some(questions),
            _ => None,
        })
        .flatten()
        .filter(|q| !q.answered)
        .collect()
}

/// Read canonical unresolved metadata without opening an observer or retaining
/// ordinary history. Native controls use the same identities as phone cards.
pub fn unresolved_question(path: &Path, id: &str, live: &impl Fn() -> bool) -> Option<ChatQuestion> {
    let head = util::read_head(path, 4096).ok()?;
    let mut pending = Vec::new();
    questions::refresh_until(path, codex_mode(&head), &mut 0, &mut pending, &mut Vec::new(), live);
    pending.into_iter().find(|question| question.id == id && !question.answered)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Item {
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        /// The images sent with it, each the key [`image`] serves it by, so the phone shows them as the Claude
        /// app does rather than "[image]".
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        at: Option<u64>,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        questions: Vec<ChatQuestion>,
        #[serde(skip_serializing_if = "Option::is_none")]
        at: Option<u64>,
    },
    Thinking {
        text: String,
    },
    /// A tool call: `call` ties it to its result.
    Tool {
        call: String,
        name: String,
        summary: String,
        input: String,
        /// What the call does in words, where the program says: a Claude
        /// shell command's description ("Ran Build the APK", as the Claude
        /// app shows a lone command).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// The lines a file edit adds and removes, counted before `input`
        /// is clipped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<[u32; 2]>,
    },
    Result {
        call: String,
        text: String,
        error: bool,
    },
    Notice {
        text: String,
    },
    /// The session's screen, for a program whose log is not read.
    Screen {
        text: String,
    },
}

pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("\n…");
    out
}

pub(super) fn first_line(text: &str, max: usize) -> String {
    util::truncate_width(&util::one_line(text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")), max)
}

/// The tags Claude Code and Codex open a user record with when they wrote
/// it themselves: hook and command output, reminders, notifications and
/// context. Only these mark a record as theirs. The owner's own prompt may
/// open with any tag, such as `<div>`, `<svg>` or `<config>`, and was lost
/// when every leading tag counted (CEP-001).
const INJECTED_TAGS: &[&str] = &[
    "bash-input",
    "bash-stderr",
    "bash-stdout",
    "channel",
    "command-args",
    "command-message",
    "command-name",
    "environment_context",
    "fork-boilerplate",
    "ide_diagnostics",
    "ide_opened_file",
    "ide_selection",
    "local-command-caveat",
    "local-command-stderr",
    "local-command-stdout",
    "new-diagnostics",
    "skill",
    "subagent_notification",
    "system-reminder",
    "task-notification",
    "teammate-message",
    "turn_aborted",
    "user-memory-input",
    "user-prompt-submit-hook",
    "user_instructions",
    "user_shell_command",
];

const QUESTION_REPLY: &str = "send_user_message_question_reply";

/// The name of the bare tag a text opens with: `div` for `<div>…`.
fn leading_tag(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix('<')?;
    let len = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or(rest.len());
    (len > 0 && rest[len..].starts_with('>')).then(|| &rest[..len])
}

/// Text a program put in the conversation itself rather than the owner:
/// hook and command tags, system reminders, Claude's caveats and Codex's
/// instructions and environment context.
pub fn injected(text: &str) -> bool {
    let t = text.trim_start();
    if t.starts_with("Caveat: The messages below were generated") || t.starts_with("# AGENTS.md instructions") {
        return true;
    }
    leading_tag(t).is_some_and(|tag| INJECTED_TAGS.contains(&tag))
}

fn tag_body<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

/// The body of a text that is one `<tag>…</tag>` envelope and nothing else.
fn envelope<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    text.trim().strip_prefix(&format!("<{tag}>"))?.strip_suffix(&format!("</{tag}>"))
}

/// Only a complete native receipt resolves a question; examples and partial
/// envelopes in ordinary messages never change its state.
pub(super) fn question_replies(text: &str) -> Vec<String> {
    let Some(body) = envelope(text, QUESTION_REPLY) else {
        return Vec::new();
    };
    let Ok(Value::Array(answers)) = serde_json::from_str::<Value>(body) else { return Vec::new() };
    answers
        .iter()
        .filter_map(|answer| {
            answer.get("answer")?.as_str().filter(|s| !s.trim().is_empty())?;
            answer.get("question")?.as_str().filter(|s| !s.trim().is_empty())?;
            let id = answer.get("questionItemId")?.as_str()?;
            let (tool, message, index) = serde_json::from_str::<(String, String, usize)>(id).ok()?;
            (tool == "request_user_input_async").then(|| format!("{message}:{index}"))
        })
        .collect()
}

/// What the owner typed, from a user record's text: a slash command reads
/// as typed, its output as a notice, injected text as nothing. Only a record
/// the program wrote is read for its tags: the same tags quoted in a prompt
/// are the owner's words and stay as he typed them (CEP-001).
pub(super) fn user_text(text: &str, at: Option<u64>, out: &mut Vec<Item>) {
    if let Some(body) = envelope(text, QUESTION_REPLY) {
        let resolved = question_replies(text);
        for item in out.iter_mut() {
            if let Item::Assistant { questions, .. } = item {
                for question in questions.iter_mut().filter(|q| resolved.contains(&q.id)) {
                    question.answered = true;
                }
            }
        }
        if let Ok(Value::Array(answers)) = serde_json::from_str::<Value>(body) {
            for answer in answers {
                if let Some(text) = answer.get("answer").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                    out.push(Item::User { client_id: None, text: clip(text, TEXT_MAX), images: Vec::new(), at });
                }
            }
        }
        return;
    }
    let tag = leading_tag(text);
    let command = matches!(tag, Some("command-name" | "command-message" | "command-args"));
    let command_output = matches!(tag, Some("local-command-stdout" | "local-command-stderr"));
    if let Some(name) = tag_body(text, "command-name").filter(|_| command) {
        let args = tag_body(text, "command-args").unwrap_or("").trim();
        let typed = if args.is_empty() { name.trim().to_string() } else { format!("{} {args}", name.trim()) };
        out.push(Item::User { client_id: None, text: typed, images: Vec::new(), at });
        return;
    }
    if let Some(output) = tag_body(text, "local-command-stdout").filter(|_| command_output) {
        let output = output.trim();
        if !output.is_empty() {
            out.push(Item::Notice { text: clip(output, RESULT_MAX) });
        }
        return;
    }
    let t = text.trim();
    if t.starts_with("[Request interrupted by user") {
        out.push(Item::Notice { text: "Interrupted".into() });
        return;
    }
    if t.is_empty() || injected(t) {
        return;
    }
    out.push(Item::User { client_id: None, text: clip(t, TEXT_MAX), images: Vec::new(), at });
}

/// One line about a tool call: the command, file, pattern or address.
pub(super) fn tool_summary(name: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str);
    let pick = match name {
        "Bash" => field("command"),
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => field("file_path").or(field("notebook_path")),
        "Grep" | "Glob" => field("pattern"),
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        "Task" | "Agent" => field("description"),
        _ => None,
    };
    let text = pick
        .map(str::to_string)
        .or_else(|| input.as_object().and_then(|o| o.values().find_map(|v| v.as_str().map(str::to_string))));
    first_line(&text.unwrap_or_default(), SUMMARY_MAX)
}

pub(super) fn pretty(input: &Value) -> String {
    let text = match input {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    clip(&text, TOOL_INPUT_MAX)
}

/// A tool result's text: a string, or the text parts of a list.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") | Some("input_text") | Some("output_text") => {
                    p.get("text").and_then(Value::as_str).map(str::to_string)
                }
                Some("image") | Some("input_image") => Some("[image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn timestamp(v: &Value) -> Option<u64> {
    v.get("timestamp").and_then(Value::as_str).and_then(codex::parse_rfc3339_ms)
}

/// The model and effort a log last named.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelSeen {
    pub model: Option<String>,
    pub effort: Option<String>,
}

fn is_image(b: &Value) -> bool {
    matches!(b.get("type").and_then(Value::as_str), Some("image" | "input_image" | "local_image"))
}

/// Every image block of a record, in the order a walk of it meets them:
/// an image's place in this list is the second half of its key.
fn image_blocks(v: &Value) -> Vec<&Value> {
    fn walk<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
        match v {
            Value::Object(map) => {
                if is_image(v) {
                    out.push(v);
                }
                map.values().for_each(|child| walk(child, out));
            }
            Value::Array(list) => list.iter().for_each(|child| walk(child, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out);
    out
}

/// Name the images among `blocks`, a record's own content rather than a
/// tool's result, for the owner's message they came with: the last one the
/// record added after `from`, or one of their own. A record is read from its
/// `offset` in the log again when the phone asks for an image ([`image`]);
/// without one (search) the images are left out.
fn attach_images(v: &Value, blocks: &[Value], offset: Option<u64>, at: Option<u64>, from: usize, out: &mut Vec<Item>) {
    let Some(offset) = offset else { return };
    let all = image_blocks(v);
    let keys: Vec<String> = blocks
        .iter()
        .filter(|b| is_image(b))
        .filter_map(|b| all.iter().position(|x| std::ptr::eq(*x, b)))
        .map(|n| format!("{offset}.{n}"))
        .collect();
    if keys.is_empty() {
        return;
    }
    let mine = out[from..].iter_mut().rev().find_map(|item| match item {
        Item::User { images, .. } => Some(images),
        _ => None,
    });
    match mine {
        Some(images) => images.extend(keys),
        None => out.push(Item::User { client_id: None, text: String::new(), images: keys, at }),
    }
}

/// A record holding many screenshots is several megabytes; one past this is
/// not read for an image.
const IMAGE_RECORD_MAX: u64 = 64 << 20;
/// The most base64 an image may be to be sent: a host's answer carries it,
/// and a host message is at most [`crate::protocol::MAX_MESSAGE`].
pub const IMAGE_SEND_MAX: usize = 16 << 20;

/// The image `key` ([`attach_images`]) names in the log at `path`: its media
/// type and its bytes in base64.
pub fn image(path: &Path, key: &str) -> anyhow::Result<(String, String)> {
    use anyhow::Context;
    let (offset, n) = key
        .split_once('.')
        .and_then(|(o, n)| Some((o.parse::<u64>().ok()?, n.parse::<usize>().ok()?)))
        .context("not an image key")?;
    let bytes = util::read_between(path, offset, offset + IMAGE_RECORD_MAX)?;
    let line = bytes.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let v: Value = serde_json::from_slice(line).context("the image's message is no longer in the log")?;
    let block = image_blocks(&v).get(n).copied().context("the image is no longer in the log")?;
    let (media, data) = image_data(block)?;
    anyhow::ensure!(media.starts_with("image/"), "not an image");
    anyhow::ensure!(data.len() <= IMAGE_SEND_MAX, "the image is too large to send");
    Ok((media, data))
}

fn image_data(block: &Value) -> anyhow::Result<(String, String)> {
    use anyhow::Context;
    if let Some(source) = block.get("source").filter(|s| s.get("type").and_then(Value::as_str) == Some("base64")) {
        let media = source.get("media_type").and_then(Value::as_str).unwrap_or("image/png");
        let data = source.get("data").and_then(Value::as_str).context("the image has no data")?;
        return Ok((media.to_string(), data.to_string()));
    }
    // Codex sends a data URL, or names a file it was pasted from.
    let url = block.get("image_url").and_then(|u| u.as_str().or_else(|| u.get("url").and_then(Value::as_str)));
    if let Some((meta, data)) = url.and_then(|u| u.strip_prefix("data:")).and_then(|u| u.split_once(',')) {
        return Ok((meta.trim_end_matches(";base64").to_string(), data.to_string()));
    }
    let file = block.get("path").and_then(Value::as_str).context("the image is not kept in the log")?;
    // Only a picture's file: the path is the log's word, not the phone's.
    let media = match Path::new(file).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => anyhow::bail!("not an image"),
    };
    let size = fs::metadata(file).context("the image's file is gone")?.len();
    anyhow::ensure!(size as usize <= IMAGE_SEND_MAX / 4 * 3, "the image is too large to send");
    Ok((media.to_string(), util::base64(&fs::read(file)?)))
}

/// What a Claude tool call does in words ([`Item::Tool`]'s `label`).
fn tool_label(name: &str, input: &Value) -> Option<String> {
    let text = match name {
        "Bash" => input.get("description").and_then(Value::as_str)?,
        _ => return None,
    };
    Some(first_line(text, SUMMARY_MAX)).filter(|t| !t.is_empty())
}

fn line_count(text: Option<&str>) -> u32 {
    text.filter(|t| !t.is_empty()).map_or(0, |t| t.trim_end_matches('\n').split('\n').count() as u32)
}

/// The lines a Claude file tool adds and removes ([`Item::Tool`]'s `diff`).
fn tool_diff(name: &str, input: &Value) -> Option<[u32; 2]> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    match name {
        "Write" => Some([line_count(s(input, "content").as_deref()), 0]),
        "Edit" => Some([line_count(s(input, "new_string").as_deref()), line_count(s(input, "old_string").as_deref())]),
        "MultiEdit" => {
            let edits = input.get("edits")?.as_array()?;
            Some(edits.iter().fold([0, 0], |[a, r], e| {
                [a + line_count(s(e, "new_string").as_deref()), r + line_count(s(e, "old_string").as_deref())]
            }))
        }
        _ => None,
    }
}

/// The lines a unified diff adds and removes.
pub(super) fn unified_diff_lines(diff: &str) -> [u32; 2] {
    diff.lines().fold([0, 0], |[a, r], line| {
        if line.starts_with("+++") || line.starts_with("---") {
            [a, r]
        } else if line.starts_with('+') {
            [a + 1, r]
        } else if line.starts_with('-') {
            [a, r + 1]
        } else {
            [a, r]
        }
    })
}

/// One record of a Claude Code transcript, which starts `offset` bytes into
/// its log (none when only its text matters).
pub fn claude_record(v: &Value, offset: Option<u64>, out: &mut Vec<Item>, seen: &mut ModelSeen) {
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || v.get("isMeta").and_then(Value::as_bool) == Some(true)
        // The summary Claude Code writes after compacting is the model's context, not the owner's message: the
        // phone showed it as his, `<system-reminder>` tags and all, where the Claude app shows nothing.
        || v.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
    {
        return;
    }
    let at = timestamp(v);
    match v.get("type").and_then(Value::as_str) {
        Some("user") => {
            let Some(content) = v.pointer("/message/content") else { return };
            match content {
                Value::String(s) => user_text(s, at, out),
                Value::Array(blocks) => {
                    let from = out.len();
                    for b in blocks {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => user_text(b.get("text").and_then(Value::as_str).unwrap_or(""), at, out),
                            Some("tool_result") => out.push(Item::Result {
                                call: b.get("tool_use_id").and_then(Value::as_str).unwrap_or("").to_string(),
                                text: clip(&result_text(b.get("content").unwrap_or(&Value::Null)), RESULT_MAX),
                                error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                            }),
                            _ => {}
                        }
                    }
                    attach_images(v, blocks, offset, at, from, out);
                }
                _ => {}
            }
        }
        Some("assistant") => {
            if let Some(model) = v.pointer("/message/model").and_then(Value::as_str).filter(|m| !m.starts_with('<')) {
                seen.model = Some(model.to_string());
            }
            let Some(Value::Array(blocks)) = v.pointer("/message/content") else { return };
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = b.get("text").and_then(Value::as_str).unwrap_or("").trim();
                        if !text.is_empty() {
                            out.push(Item::Assistant { text: clip(text, TEXT_MAX), at, questions: Vec::new() });
                        }
                    }
                    Some("thinking") => {
                        let text = b.get("thinking").and_then(Value::as_str).unwrap_or("").trim();
                        if !text.is_empty() {
                            out.push(Item::Thinking { text: clip(text, RESULT_MAX) });
                        }
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
                        let input = b.get("input").cloned().unwrap_or(Value::Null);
                        out.push(Item::Tool {
                            call: b.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                            summary: tool_summary(&name, &input),
                            input: pretty(&input),
                            label: tool_label(&name, &input),
                            diff: tool_diff(&name, &input),
                            name,
                        });
                    }
                    _ => {}
                }
            }
        }
        // A message typed while a turn ran reaches the model as an attachment.
        Some("attachment") => {
            let Some(a) = v.get("attachment") else { return };
            if a.get("type").and_then(Value::as_str) != Some("queued_command")
                || a.get("commandMode").and_then(Value::as_str) != Some("prompt")
            {
                return;
            }
            match a.get("prompt") {
                Some(Value::String(s)) => user_text(s, at, out),
                Some(Value::Array(parts)) => {
                    let from = out.len();
                    let text: Vec<&str> = parts
                        .iter()
                        .filter(|p| matches!(p.get("type").and_then(Value::as_str), Some("text" | "input_text")))
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect();
                    user_text(&text.join("\n"), at, out);
                    attach_images(v, parts, offset, at, from, out);
                }
                _ => {}
            }
        }
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => {
            out.push(Item::Notice { text: "Conversation compacted".into() });
        }
        _ => {}
    }
}

/// Which records of a Codex rollout carry the conversation: 0.163 and later
/// write each finished item as `event_msg`/`item_completed`; earlier ones
/// `event_msg` messages and `response_item` calls. A rollout uses one, so
/// nothing is shown twice.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CodexMode {
    Items,
    Events,
}

pub fn codex_mode(head: &[u8]) -> CodexMode {
    let newer =
        head.split(|b| *b == b'\n').next().and_then(|line| serde_json::from_slice::<Value>(line).ok()).and_then(|v| {
            let version = v.pointer("/payload/cli_version")?.as_str()?.to_string();
            let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
            Some((parts.next()?, parts.next().unwrap_or(0)) >= (0, 163))
        });
    if newer == Some(true) || util::contains(head, b"\"item_completed\"") {
        CodexMode::Items
    } else {
        CodexMode::Events
    }
}

fn codex_text(content: &Value) -> String {
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                    Some("text") | Some("Text") | Some("input_text") | Some("output_text") => {
                        p.get("text").and_then(Value::as_str).map(str::to_string)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// One record of a Codex rollout, which starts `offset` bytes into it (none
/// when only its text matters).
pub fn codex_record(v: &Value, offset: Option<u64>, mode: CodexMode, out: &mut Vec<Item>, seen: &mut ModelSeen) {
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    let Some(p) = v.get("payload") else { return };
    let ptype = p.get("type").and_then(Value::as_str).unwrap_or("");
    let at = timestamp(v);
    if kind == "turn_context" {
        if let Some(m) = p.get("model").and_then(Value::as_str) {
            seen.model = Some(m.to_string());
        }
        seen.effort = p.get("effort").and_then(Value::as_str).map(str::to_string);
        return;
    }
    match mode {
        CodexMode::Items if kind == "event_msg" && ptype == "item_completed" => {
            let Some(item) = p.get("item") else { return };
            let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                "UserMessage" => {
                    let content = item.get("content").unwrap_or(&Value::Null);
                    let from = out.len();
                    user_text(&codex_text(content), at, out);
                    if let Some(parts) = content.as_array() {
                        attach_images(v, parts, offset, at, from, out);
                    }
                    let client = item
                        .get("client_id")
                        .or_else(|| item.get("clientId"))
                        .and_then(Value::as_str)
                        .filter(|id| id.len() <= 128);
                    for row in &mut out[from..] {
                        if let Item::User { client_id, .. } = row {
                            *client_id = client.map(str::to_owned);
                        }
                    }
                }
                "AgentMessage" => {
                    let mut text = codex_text(item.get("content").unwrap_or(&Value::Null)).trim().to_string();
                    let mut questions = Vec::new();
                    for (index, q) in item.get("questions").and_then(Value::as_array).into_iter().flatten().enumerate()
                    {
                        let title = q.get("title").and_then(Value::as_str).unwrap_or("");
                        let options: Vec<&str> = q
                            .get("options")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect();
                        if !id.is_empty() && !title.trim().is_empty() {
                            questions.push(ChatQuestion {
                                id: format!("{id}:{index}"),
                                answered: false,
                                title: clip(title, TEXT_MAX),
                                options: options.iter().map(|o| clip(o, TEXT_MAX)).collect(),
                            });
                        }
                        text.push_str(&format!("\n\n**{title}**"));
                        for o in options {
                            text.push_str(&format!("\n- {o}"));
                        }
                    }
                    if !text.trim().is_empty() {
                        out.push(Item::Assistant { text: clip(text.trim(), TEXT_MAX), at, questions });
                    }
                }
                "Reasoning" => {
                    let text: Vec<&str> = item
                        .get("summary_text")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    let text = text.join("\n");
                    if !text.trim().is_empty() {
                        out.push(Item::Thinking { text: clip(text.trim(), RESULT_MAX) });
                    }
                }
                "CommandExecution" => {
                    let command: Vec<&str> = item
                        .get("command")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    // `["bash", "-lc", "<script>"]`: the script is what was run.
                    let shown = command.last().copied().unwrap_or("").to_string();
                    out.push(Item::Tool {
                        call: id.clone(),
                        name: "shell".into(),
                        summary: first_line(&shown, SUMMARY_MAX),
                        input: clip(&shown, TOOL_INPUT_MAX),
                        label: None,
                        diff: None,
                    });
                    let code = item.get("exit_code").and_then(Value::as_i64);
                    let mut text = s("aggregated_output");
                    if let Some(c) = code.filter(|c| *c != 0) {
                        text.push_str(&format!("\n(exit {c})"));
                    }
                    out.push(Item::Result {
                        call: id,
                        text: clip(text.trim(), RESULT_MAX),
                        error: code.is_some_and(|c| c != 0),
                    });
                }
                "FileChange" => {
                    let changes = item.get("changes").and_then(Value::as_object);
                    let files: Vec<String> = changes
                        .map(|c| {
                            c.keys()
                                .map(|k| {
                                    Path::new(k).file_name().map_or(k.clone(), |n| n.to_string_lossy().into_owned())
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let lines = changes.map(|c| {
                        c.values().fold([0, 0], |[a, r], ch| {
                            let [ca, cr] =
                                unified_diff_lines(ch.get("unified_diff").and_then(Value::as_str).unwrap_or(""));
                            [a + ca, r + cr]
                        })
                    });
                    let diff: String = changes
                        .map(|c| {
                            c.iter()
                                .map(|(path, ch)| {
                                    format!("{path}\n{}", ch.get("unified_diff").and_then(Value::as_str).unwrap_or(""))
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    out.push(Item::Tool {
                        call: id,
                        name: "edit".into(),
                        summary: first_line(&files.join(", "), SUMMARY_MAX),
                        input: clip(&diff, TOOL_INPUT_MAX),
                        label: None,
                        diff: lines,
                    });
                }
                "ContextCompaction" => out.push(Item::Notice { text: "Conversation compacted".into() }),
                "Extension" => out.push(Item::Tool {
                    call: id,
                    name: s("kind"),
                    summary: String::new(),
                    input: String::new(),
                    label: None,
                    diff: None,
                }),
                "ImageView" => out.push(Item::Tool {
                    call: id,
                    name: "image".into(),
                    summary: s("path"),
                    input: String::new(),
                    label: None,
                    diff: None,
                }),
                "CollabAgentToolCall" => out.push(Item::Tool {
                    call: id,
                    name: "agents".into(),
                    summary: first_line(&s("tool"), SUMMARY_MAX),
                    input: String::new(),
                    label: None,
                    diff: None,
                }),
                // Sub-agent progress is not part of this conversation.
                _ => {}
            }
        }
        CodexMode::Items => {}
        CodexMode::Events => match (kind, ptype) {
            ("event_msg", "user_message") => user_text(p.get("message").and_then(Value::as_str).unwrap_or(""), at, out),
            ("event_msg", "agent_message") => {
                let text = p.get("message").and_then(Value::as_str).unwrap_or("").trim();
                if !text.is_empty() {
                    out.push(Item::Assistant { text: clip(text, TEXT_MAX), at, questions: Vec::new() });
                }
            }
            ("event_msg", "agent_reasoning") => {
                let text = p.get("text").and_then(Value::as_str).unwrap_or("").trim();
                if !text.is_empty() {
                    out.push(Item::Thinking { text: clip(text, RESULT_MAX) });
                }
            }
            ("response_item", "function_call") | ("response_item", "custom_tool_call") => {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
                let raw = p.get("arguments").or_else(|| p.get("input")).and_then(Value::as_str).unwrap_or("");
                let input = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
                let summary = match input.get("command") {
                    Some(Value::Array(parts)) => {
                        first_line(parts.last().and_then(Value::as_str).unwrap_or(""), SUMMARY_MAX)
                    }
                    _ => tool_summary(&name, &input),
                };
                // A patch is text: count the lines it adds and removes.
                let diff = (name == "apply_patch").then(|| unified_diff_lines(raw));
                out.push(Item::Tool {
                    call: p.get("call_id").and_then(Value::as_str).unwrap_or("").to_string(),
                    name,
                    summary,
                    input: pretty(&input),
                    label: None,
                    diff,
                });
            }
            ("response_item", "function_call_output") | ("response_item", "custom_tool_call_output") => {
                let output = p.get("output").cloned().unwrap_or(Value::Null);
                let text = match &output {
                    Value::String(s) => serde_json::from_str::<Value>(s)
                        .ok()
                        .and_then(|v| v.get("output").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_else(|| s.clone()),
                    other => result_text(other),
                };
                out.push(Item::Result {
                    call: p.get("call_id").and_then(Value::as_str).unwrap_or("").to_string(),
                    text: clip(text.trim(), RESULT_MAX),
                    error: false,
                });
            }
            _ => {}
        },
    }
}

/// The rows `sql` selects from database `db` as JSON objects, read through
/// the `sqlite3` program (and any leading arguments) opened read-only; none
/// when it could not be read. A program's own database is read this way, never
/// opened for writing.
pub(super) fn sqlite_rows(sqlite3: &[&str], db: &Path, sql: &str) -> Option<Vec<Value>> {
    if !db.is_file() {
        return None;
    }
    let (program, lead) = sqlite3.split_first()?;
    let out = Command::new(program)
        .args(lead)
        .args(["-readonly", "-bail", "-json", "-cmd", ".timeout 500"])
        .arg(db)
        .arg(sql)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    if out.stdout.iter().all(u8::is_ascii_whitespace) {
        return Some(Vec::new());
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// An opencode session's messages, read from its database read-only.
pub fn opencode_items(sqlite3: &[&str], db: &Path, id: &str) -> Option<(Vec<Item>, ModelSeen)> {
    if !other::is_opencode_id(id) {
        return None;
    }
    let rows = sqlite_rows(
        sqlite3,
        db,
        &format!(
            "select type, time_created, data from session_message where session_id = '{id}' and type in ('user', 'assistant') order by seq"
        ),
    )?;
    let mut items = Vec::new();
    let mut seen = ModelSeen::default();
    for row in rows {
        let at = row.get("time_created").and_then(Value::as_u64);
        let Some(data) = row.get("data").and_then(Value::as_str).and_then(|d| serde_json::from_str::<Value>(d).ok())
        else {
            continue;
        };
        match row.get("type").and_then(Value::as_str) {
            Some("user") => user_text(data.get("text").and_then(Value::as_str).unwrap_or(""), at, &mut items),
            Some("assistant") => {
                if let Some(m) = data.pointer("/model/id").and_then(Value::as_str) {
                    seen.model = Some(m.to_string());
                    seen.effort = data.pointer("/model/variant").and_then(Value::as_str).map(str::to_string);
                }
                for part in data.get("content").and_then(Value::as_array).into_iter().flatten() {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or("").trim();
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") if !text.is_empty() => {
                            items.push(Item::Assistant { text: clip(text, TEXT_MAX), at, questions: Vec::new() })
                        }
                        Some("reasoning") if !text.is_empty() => {
                            items.push(Item::Thinking { text: clip(text, RESULT_MAX) })
                        }
                        Some("tool") => {
                            let call = part.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                            let name = part.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
                            let input = part.pointer("/state/input").cloned().unwrap_or(Value::Null);
                            items.push(Item::Tool {
                                call: call.clone(),
                                summary: tool_summary(&name, &input),
                                input: pretty(&input),
                                label: None,
                                diff: None,
                                name,
                            });
                            let status = part.pointer("/state/status").and_then(Value::as_str).unwrap_or("");
                            let result = result_text(part.pointer("/state/content").unwrap_or(&Value::Null));
                            if !result.is_empty() || status == "error" {
                                items.push(Item::Result {
                                    call,
                                    text: clip(result.trim(), RESULT_MAX),
                                    error: status == "error",
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some((items, seen))
}

impl Source {
    /// An image the conversation's items name ([`image`]): only a Claude or
    /// Codex log keeps them.
    pub fn image(&self, key: &str) -> anyhow::Result<(String, String)> {
        match self {
            Source::Claude(path) | Source::Codex(path) => image(path, key),
            _ => anyhow::bail!("this conversation keeps no images"),
        }
    }
}

/// Where a session's conversation is read from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Claude(PathBuf),
    Codex(PathBuf),
    OpenCode(String),
    /// An Antigravity conversation's database.
    Antigravity(PathBuf),
    /// A custom provider's atomically replaced plain-message snapshot.
    Snapshot(PathBuf),
    /// Live output, including Codex until its exact conversation is known
    /// and its rollout is available. Shared-server clients need not expose
    /// a thread name, and creation-time matching can be ambiguous.
    Screen,
    /// A new Claude chat whose log is not written until the first message
    /// after `/clear`.
    Missing,
    /// A session on another host, whose host reads the log and sends what
    /// it gained ([`Chunk`]).
    Remote,
}

/// What a host sends of a session's chat (`HostReq::Chat`): the items its
/// asker lacks when the asker's copy is of the same generation, else all
/// it keeps, starting at index `start`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Chunk {
    pub kind: ChunkKind,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub dropped: usize,
    #[serde(default)]
    pub start: usize,
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub seen: ModelSeen,
    /// Full unresolved-question snapshot, independent of item offsets.
    /// None is an older host, whose retained items remain the fallback.
    #[serde(default)]
    pub questions: Option<Vec<ChatQuestion>>,
    /// Recent positive receipt evidence for native answer confirmation.
    #[serde(default)]
    pub question_replies: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// No log found yet.
    None,
    /// A program whose screen is its chat: ask the deck for the screen.
    Screen,
    Log,
}

impl Chunk {
    fn of(kind: ChunkKind) -> Chunk {
        Chunk {
            kind,
            generation: 0,
            dropped: 0,
            start: 0,
            items: Vec::new(),
            seen: ModelSeen::default(),
            questions: Some(Vec::new()),
            question_replies: Vec::new(),
        }
    }
}

pub struct Dirs {
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub opencode_db: PathBuf,
    /// Antigravity's state directory, holding `conversations/<id>.db`.
    pub antigravity: PathBuf,
    pub start_dir: PathBuf,
    pub sqlite3: Vec<String>,
}

/// Find the log of a session's current conversation.
pub fn source(info: &SessionInfo, dirs: &Dirs) -> Option<Source> {
    crate::providers::transcripts::source(info, dirs)
}

/// A generation no earlier copy of any chat had. A phone joins a page onto the copy it holds only when their
/// generations are equal, so a copy read afresh (after the gateway restarts, a cache eviction, `/clear` or a
/// rewritten log) must not reuse one: its indexes can start elsewhere in the log (`READ_MAX`), and joining them
/// spliced one read's messages into another's. Counting from the wall clock in microseconds keeps a restarted
/// gateway clear of every generation the previous one handed out.
fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.as_micros() as u64);
    let mut current = NEXT.load(Ordering::Relaxed);
    loop {
        let next = current.max(now).saturating_add(1);
        match NEXT.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => current = seen,
        }
    }
}

/// A conversation parsed so far.
pub struct Log {
    pub source: Source,
    /// Items, after `dropped` older ones.
    pub items: Vec<Item>,
    pub dropped: usize,
    pub seen: ModelSeen,
    /// All unresolved questions in this conversation, including ones whose
    /// ordinary history has aged out. Rebuilt from the full transcript on open.
    pub questions: Vec<ChatQuestion>,
    question_replies: Vec<String>,
    question_offset: u64,
    file_id: Option<(u64, u64)>,
    offset: u64,
    mode: Option<CodexMode>,
    checked: Option<Instant>,
    /// Replaced by a [`next_generation`] when the items were replaced
    /// rather than added to.
    pub generation: u64,
    used: Instant,
    /// An Antigravity conversation's steps as read so far.
    steps: agy::Steps,
    observer: Option<Weak<streaming::Observer>>,
    completed: VecDeque<String>,
}

impl Log {
    fn new(source: Source) -> Log {
        Log {
            source,
            items: Vec::new(),
            dropped: 0,
            seen: ModelSeen::default(),
            questions: Vec::new(),
            question_replies: Vec::new(),
            question_offset: 0,
            file_id: None,
            offset: 0,
            mode: None,
            checked: None,
            generation: next_generation(),
            used: Instant::now(),
            steps: agy::Steps::default(),
            observer: None,
            completed: VecDeque::new(),
        }
    }

    pub fn total(&self) -> usize {
        self.dropped + self.items.len()
    }

    /// Native deltas share the phone's ephemeral stream. They never occupy
    /// canonical indexes, replace loaded history, or enter an offline chunk.
    pub(super) fn stream(&self) -> Option<String> {
        self.observer.as_ref().and_then(Weak::upgrade)?.latest(&self.completed)
    }

    /// Positive receipt evidence also survives a native card aging out of rows.
    pub fn question_answered(&self, id: &str) -> bool {
        self.question_replies.iter().any(|reply| reply == id)
            || self.items.iter().any(|item| {
                matches!(item, Item::Assistant { questions, .. }
                    if questions.iter().any(|q| q.id == id && q.answered))
            })
    }

    fn push_all(&mut self, new: Vec<Item>) {
        self.items.extend(new);
        if self.items.len() > ITEMS_MAX {
            let extra = self.items.len() - ITEMS_MAX;
            self.items.drain(..extra);
            self.dropped += extra;
        }
    }

    /// Read what the log gained since last time.
    fn refresh(&mut self, dirs: &Dirs, now: Instant) {
        let refresh = if matches!(self.source, Source::Claude(_) | Source::Codex(_)) { FILE_REFRESH } else { REFRESH };
        if self.checked.is_some_and(|at| now.duration_since(at) < refresh) {
            return;
        }
        self.checked = Some(now);
        let sqlite3: Vec<&str> = dirs.sqlite3.iter().map(String::as_str).collect();
        match self.source.clone() {
            Source::Claude(path) | Source::Codex(path) => self.read_file(&path),
            Source::OpenCode(id) => {
                if let Some((items, seen)) = opencode_items(&sqlite3, &dirs.opencode_db, &id) {
                    self.seen = seen;
                    self.replace(items);
                }
            }
            Source::Antigravity(db) => {
                if self.steps.refresh(&sqlite3, &db) == Some(true) {
                    let items = self.steps.items();
                    self.replace(items);
                }
            }
            Source::Snapshot(path) => {
                if let Some(items) = crate::providers::snapshot::read(&path) {
                    self.replace(items);
                }
            }
            Source::Screen | Source::Missing | Source::Remote => {}
        }
        if let Some(observer) = self.observer.as_ref().and_then(Weak::upgrade) {
            observer.acknowledge(&self.completed);
        }
    }

    /// Take a database's whole conversation, read again: a message's parts
    /// fill in as it streams. The phone is sent only what was added unless an
    /// item it has changed.
    fn replace(&mut self, items: Vec<Item>) {
        if self.items != items {
            let grew = items.len() >= self.items.len() && items[..self.items.len()] == self.items[..];
            if !grew {
                self.generation = next_generation();
            }
            self.items = items;
            self.dropped = 0;
        }
    }

    fn read_file(&mut self, path: &Path) {
        let Ok(meta) = fs::metadata(path) else { return };
        let file_id = (meta.dev(), meta.ino());
        if self.file_id.is_some_and(|old| old != file_id)
            || meta.len() < self.offset
            || meta.len() < self.question_offset
        {
            // Rewritten: start again.
            *self = Log::new(self.source.clone());
        }
        self.file_id = Some(file_id);
        let codex = matches!(self.source, Source::Codex(_));
        if meta.len() == self.offset && (!codex || meta.len() == self.question_offset) {
            return;
        }
        if codex && self.mode.is_none() {
            self.mode = Some(codex_mode(&util::read_head(path, 256 << 10).unwrap_or_default()));
        }
        if codex
            && questions::refresh(
                path,
                self.mode.unwrap_or(CodexMode::Items),
                &mut self.question_offset,
                &mut self.questions,
                &mut self.question_replies,
            )
        {
            self.generation = next_generation();
        }
        // Rollouts reach hundreds of megabytes: a long conversation starts
        // from its last `READ_MAX` bytes, at a line's start.
        if self.offset == 0 && meta.len() > READ_MAX {
            let from = meta.len() - READ_MAX;
            let Ok((head, _)) = util::read_range(path, from, 1 << 20) else { return };
            let Some(nl) = head.iter().position(|b| *b == b'\n') else { return };
            self.offset = from + nl as u64 + 1;
            self.items.push(Item::Notice { text: "Earlier messages are only on the desk".into() });
        }
        let to = meta.len().min(self.offset + READ_MAX);
        let Ok(bytes) = util::read_between(path, self.offset, to) else { return };
        let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else { return };
        let complete = &bytes[..=end];
        // Replies can resolve cards read in an earlier refresh, so parse
        // against retained items as well as the current file increment.
        let mut new = std::mem::take(&mut self.items);
        let answered_count = |items: &[Item]| -> usize {
            items
                .iter()
                .map(|item| match item {
                    Item::Assistant { questions, .. } => questions.iter().filter(|q| q.answered).count(),
                    _ => 0,
                })
                .sum()
        };
        let before = answered_count(&new);
        let mut at = self.offset;
        for line in complete.split(|b| *b == b'\n') {
            // Where this record starts, which names the images sent in it.
            let offset = Some(at);
            at += line.len() as u64 + 1;
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
            if codex {
                if v.pointer("/payload/type").and_then(Value::as_str) == Some("item_completed")
                    && v.pointer("/payload/item/type").and_then(Value::as_str) == Some("AgentMessage")
                {
                    if let Some(id) = v.pointer("/payload/item/id").and_then(Value::as_str) {
                        if self.completed.len() == 512 {
                            self.completed.pop_front();
                        }
                        self.completed.push_back(id.to_string());
                    }
                }
                codex_record(&v, offset, self.mode.unwrap_or(CodexMode::Items), &mut new, &mut self.seen);
            } else {
                claude_record(&v, offset, &mut new, &mut self.seen);
            }
        }
        self.offset += complete.len() as u64;
        if answered_count(&new) != before {
            self.generation = next_generation();
        }
        self.push_all(new);
    }
}

/// The conversations the phone has looked at, by session.
#[derive(Default)]
pub struct Cache {
    logs: HashMap<u64, Log>,
    observers: HashMap<u64, Arc<streaming::Observer>>,
}

impl Cache {
    /// Subscribe before a phone opens a chat, so opening midway through a reply
    /// already has its prefix. Parsed history remains limited to CACHE_MAX.
    pub fn watch_sessions(&mut self, sessions: &[SessionInfo]) {
        self.observers.retain(|id, observer| sessions.iter().any(|s| s.id == *id && observer.matches(s)));
        for info in sessions {
            if let std::collections::hash_map::Entry::Vacant(entry) = self.observers.entry(info.id) {
                if let Some(observer) = streaming::Observer::start(info) {
                    entry.insert(Arc::new(observer));
                }
            }
        }
    }

    /// The session's conversation or live-output source, brought up to date;
    /// none until a new Claude conversation's first log is available.
    pub fn get(&mut self, info: &SessionInfo, dirs: &Dirs) -> Option<&Log> {
        let known = self.logs.get(&info.id).and_then(|l| {
            let path = match &l.source {
                Source::Claude(p) | Source::Codex(p) => p,
                _ => return None,
            };
            let id = info.conversation.as_deref()?;
            let name = path.file_name()?.to_str()?;
            (name.ends_with(&format!("{id}.jsonl")) && path.is_file()).then(|| l.source.clone())
        });
        let src = match known.or_else(|| source(info, dirs)) {
            Some(src) => src,
            // The session left the chat held for a new one with no log yet: an empty chat replaces it, or the
            // phone keeps showing the old one.
            None if self.logs.contains_key(&info.id) => Source::Missing,
            None => return None,
        };
        let now = Instant::now();
        if self.logs.get(&info.id).is_some_and(|l| l.source != src) {
            // A new chat (`/clear`) in the same session.
            self.logs.insert(info.id, Log::new(src.clone()));
        }
        if !self.logs.contains_key(&info.id) && self.logs.len() >= CACHE_MAX {
            if let Some(oldest) = self.logs.iter().min_by_key(|(_, l)| l.used).map(|(id, _)| *id) {
                self.logs.remove(&oldest);
            }
        }
        if self.observers.get(&info.id).is_some_and(|o| !o.matches(info)) {
            self.observers.remove(&info.id);
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = self.observers.entry(info.id) {
            if let Some(observer) = streaming::Observer::start(info) {
                entry.insert(Arc::new(observer));
            }
        }
        let log = self.logs.entry(info.id).or_insert_with(|| Log::new(src));
        log.used = now;
        log.observer = self.observers.get(&info.id).map(Arc::downgrade);
        log.refresh(dirs, now);
        Some(log)
    }

    pub fn forget_missing(&mut self, live: &[u64]) {
        self.logs.retain(|id, _| live.contains(id));
        self.observers.retain(|id, _| live.contains(id));
    }

    /// The copy of session `id`'s chat as it stands, without reading.
    pub fn peek(&self, id: u64) -> Option<&Log> {
        self.logs.get(&id)
    }

    /// On a host: what a copy of the session's chat holding `from` items
    /// of `generation` lacks, or all of it when that copy is not current.
    pub fn chunk(&mut self, info: &SessionInfo, dirs: &Dirs, from: usize, generation: u64) -> Chunk {
        let Some(log) = self.get(info, dirs) else { return Chunk::of(ChunkKind::None) };
        if log.source == Source::Screen {
            return Chunk::of(ChunkKind::Screen);
        }
        let current = generation == log.generation && (log.dropped..=log.total()).contains(&from);
        let start = if current { from } else { log.dropped };
        Chunk {
            kind: ChunkKind::Log,
            generation: log.generation,
            dropped: log.dropped,
            start,
            items: log.items[start - log.dropped..].to_vec(),
            seen: log.seen.clone(),
            questions: Some(log.questions.clone()),
            question_replies: log.question_replies.clone(),
        }
    }

    /// On the hub: bring the copy of session `id`'s chat up to what its
    /// host sent; none while the host has no log for it.
    pub fn apply(&mut self, id: u64, chunk: Chunk) -> Option<&Log> {
        let source = match chunk.kind {
            ChunkKind::None => {
                self.logs.remove(&id);
                return None;
            }
            ChunkKind::Screen => Source::Screen,
            ChunkKind::Log => Source::Remote,
        };
        if self.logs.get(&id).is_some_and(|l| l.source != source) {
            self.logs.remove(&id);
        }
        if !self.logs.contains_key(&id) && self.logs.len() >= CACHE_MAX {
            if let Some(oldest) = self.logs.iter().min_by_key(|(_, l)| l.used).map(|(id, _)| *id) {
                self.logs.remove(&oldest);
            }
        }
        let log = self.logs.entry(id).or_insert_with(|| Log::new(source));
        log.used = Instant::now();
        if chunk.kind == ChunkKind::Log {
            if chunk.generation == log.generation && chunk.start == log.total() {
                log.items.extend(chunk.items);
            } else {
                log.items = chunk.items;
                log.dropped = chunk.start;
                log.generation = chunk.generation;
            }
            // Drop what the host dropped, so indexes stay the host's.
            let gone = chunk.dropped.saturating_sub(log.dropped).min(log.items.len());
            log.items.drain(..gone);
            log.dropped += gone;
            log.seen = chunk.seen;
            log.question_replies = chunk.question_replies;
            log.questions =
                chunk.questions.unwrap_or_else(|| pending_questions(&log.items).into_iter().cloned().collect());
        }
        Some(log)
    }
}

#[cfg(test)]
#[path = "../tests/remote_chat.rs"]
mod tests;
