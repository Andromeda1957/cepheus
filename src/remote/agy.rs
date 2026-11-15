//! An Antigravity (`agy`) conversation as chat items, read from the SQLite database it keeps for each
//! conversation (`conversations/<id>.db`). The phone used to show agy's screen instead, which after a resume
//! holds only its banner and input box.
//!
//! Each row of `steps` is one step of the conversation, and `step_payload` is an undocumented protobuf message,
//! read here with a minimal wire reader. The field numbers were read from the owner's conversations, written by
//! Antigravity CLI 1.4.12 and earlier (2026-11-02):
//!
//! - type 14, a prompt: its text at 19.2;
//! - type 15, a reply: Markdown at 20.1, thinking at 20.3 and each tool call
//!   at 20.7 (id 1, name 2, JSON arguments 3);
//! - type 132, a tool's result: its call at 5.4.1 and its output at 140.2.1,
//!   failed with status 6 (cancelled) or 7, the error at 31.2;
//! - type 17, an error such as a spent quota: its message at 24.3.1;
//! - type 23, the summary that replaces older steps when the context fills;
//! - type 101, a message from another agent: title 114.2.1, text 114.2.10.1.
//!
//! Every step's metadata is field 5, its creation time 5.1 (seconds, nanos). Other steps are left out, and so
//! is a field this reader does not find.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use super::chat::{self, Item};

const PROMPT: i64 = 14;
const REPLY: i64 = 15;
const ERROR: i64 = 17;
const SUMMARY: i64 = 23;
const AGENT_MESSAGE: i64 = 101;
const RESULT: i64 = 132;

/// Step statuses of a tool that did not finish: cancelled, failed.
const FAILED: [i64; 2] = [6, 7];

/// One field of a protobuf message as the wire carries it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn varint(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

fn take<'a>(data: &'a [u8], at: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = at.checked_add(len).filter(|end| *end <= data.len())?;
    let bytes = &data[*at..end];
    *at = end;
    Some(bytes)
}

fn next_field<'a>(data: &'a [u8], at: &mut usize) -> Option<(u64, Wire<'a>)> {
    if *at >= data.len() {
        return None;
    }
    let key = varint(data, at)?;
    let wire = match key & 7 {
        0 => Wire::Varint(varint(data, at)?),
        1 => take(data, at, 8).map(|_| Wire::Fixed)?,
        5 => take(data, at, 4).map(|_| Wire::Fixed)?,
        2 => {
            let len = usize::try_from(varint(data, at)?).ok()?;
            Wire::Bytes(take(data, at, len)?)
        }
        // Groups (3, 4) are not used by these messages.
        _ => return None,
    };
    (key >> 3 != 0).then_some((key >> 3, wire))
}

/// The fields of message `data` in order, ending at the first malformed one.
fn fields(data: &[u8]) -> impl Iterator<Item = (u64, Wire<'_>)> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let field = next_field(data, &mut at);
        if field.is_none() {
            at = data.len();
        }
        field
    })
}

/// Every length-delimited field `number` of message `data`.
fn all(data: &[u8], number: u64) -> impl Iterator<Item = &[u8]> {
    fields(data).filter_map(move |(n, w)| match w {
        Wire::Bytes(b) if n == number => Some(b),
        _ => None,
    })
}

/// The first length-delimited field at `path`, a field number per level.
fn bytes<'a>(data: &'a [u8], path: &[u64]) -> Option<&'a [u8]> {
    path.iter().try_fold(data, |msg, n| all(msg, *n).next())
}

fn text(data: &[u8], path: &[u64]) -> Option<String> {
    bytes(data, path).and_then(|b| std::str::from_utf8(b).ok()).map(str::to_string)
}

fn number(data: &[u8], path: &[u64]) -> Option<u64> {
    let (last, outer) = path.split_last()?;
    let msg = bytes(data, outer)?;
    fields(msg).find_map(|(n, w)| match w {
        Wire::Varint(v) if n == *last => Some(v),
        _ => None,
    })
}

/// When step `payload` was created, in milliseconds.
fn created_ms(payload: &[u8]) -> Option<u64> {
    let secs = number(payload, &[5, 1, 1])?;
    Some(secs * 1000 + number(payload, &[5, 1, 2]).unwrap_or(0) / 1_000_000)
}

/// One line about a tool call: the command, file, query or address.
fn tool_summary(name: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str);
    let pick = match name {
        "run_command" => field("CommandLine"),
        "view_file" | "write_to_file" | "replace_file_content" => field("AbsolutePath").or(field("TargetFile")),
        "read_file" => field("Path"),
        "search_web" => field("query"),
        "read_url_content" => field("Url"),
        "send_message" => field("Recipient"),
        "invoke_subagent" => field("TypeName").or(field("Role")),
        _ => None,
    };
    match pick.or(field("toolSummary")) {
        Some(text) => chat::first_line(text, chat::SUMMARY_MAX),
        None => chat::tool_summary(name, input),
    }
}

/// The chat items of one step.
pub fn step_items(index: i64, kind: i64, status: i64, payload: &[u8]) -> Vec<Item> {
    let mut out = Vec::new();
    let at = created_ms(payload);
    match kind {
        PROMPT => {
            if let Some(t) = text(payload, &[19, 2]).or_else(|| text(payload, &[19, 3, 1])) {
                chat::user_text(&t, at, &mut out);
            }
        }
        REPLY => {
            let Some(reply) = bytes(payload, &[20]) else { return out };
            let thinking = text(reply, &[3]).unwrap_or_default();
            if !thinking.trim().is_empty() {
                out.push(Item::Thinking { text: chat::clip(thinking.trim(), chat::RESULT_MAX) });
            }
            let said = text(reply, &[1]).unwrap_or_default();
            if !said.trim().is_empty() {
                out.push(Item::Assistant { text: chat::clip(said.trim(), chat::TEXT_MAX), questions: Vec::new(), at });
            }
            for call in all(reply, 7) {
                let name = text(call, &[2]).unwrap_or_else(|| "tool".into());
                let raw = text(call, &[3]).unwrap_or_default();
                let input = serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw));
                out.push(Item::Tool {
                    call: text(call, &[1]).unwrap_or_default(),
                    summary: tool_summary(&name, &input),
                    input: chat::pretty(&input),
                    label: None,
                    diff: None,
                    name,
                });
            }
        }
        RESULT => {
            let error = FAILED.contains(&status);
            let output = text(payload, &[140, 2, 1]).unwrap_or_default();
            let output = match output.trim() {
                "" if error => text(payload, &[31, 2]).unwrap_or_default(),
                trimmed => trimmed.to_string(),
            };
            out.push(Item::Result {
                call: text(payload, &[5, 4, 1]).unwrap_or_default(),
                text: chat::clip(output.trim(), chat::RESULT_MAX),
                error,
            });
        }
        ERROR => {
            if let Some(t) = text(payload, &[24, 3, 1]).filter(|t| !t.trim().is_empty()) {
                out.push(Item::Notice { text: chat::clip(t.trim(), chat::RESULT_MAX) });
            }
        }
        SUMMARY => out.push(Item::Notice { text: "Conversation compacted".into() }),
        AGENT_MESSAGE => {
            let Some(message) = bytes(payload, &[114, 2]) else { return out };
            let call = format!("agy-step-{index}");
            out.push(Item::Tool {
                call: call.clone(),
                name: "agent".into(),
                summary: chat::first_line(&text(message, &[1]).unwrap_or_default(), chat::SUMMARY_MAX),
                input: String::new(),
                label: None,
                diff: None,
            });
            let body = text(message, &[10, 1]).unwrap_or_default();
            out.push(Item::Result { call, text: chat::clip(body.trim(), chat::RESULT_MAX), error: false });
        }
        _ => {}
    }
    out
}

/// The bytes of SQLite's `hex()` of a payload. `auth::decode_hex` stops at
/// 256 digits, and a step's payload runs to megabytes.
fn hex_bytes(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect()
}

/// A step as the database lists it, and its items once read.
#[derive(Debug)]
struct Step {
    kind: i64,
    status: i64,
    len: i64,
    items: Vec<Item>,
}

/// The steps of one conversation read so far. A step's payload is read again
/// only when its type, status or size changes, as a reply's does while it
/// streams, so a long conversation is read whole once.
#[derive(Default, Debug)]
pub struct Steps {
    steps: BTreeMap<i64, Step>,
}

fn int(row: &Value, key: &str) -> i64 {
    row.get(key).and_then(Value::as_i64).unwrap_or(-1)
}

impl Steps {
    /// Bring the steps up to date with database `db`, read through the
    /// `sqlite3` program (and any leading arguments) opened read-only;
    /// whether they changed, or none when the database could not be read.
    pub fn refresh(&mut self, sqlite3: &[&str], db: &Path) -> Option<bool> {
        let rows = chat::sqlite_rows(
            sqlite3,
            db,
            "select idx, step_type, status, length(step_payload) as len from steps order by idx",
        )?;
        let listed: BTreeMap<i64, (i64, i64, i64)> =
            rows.iter().map(|r| (int(r, "idx"), (int(r, "step_type"), int(r, "status"), int(r, "len")))).collect();
        let mut changed = self.steps.len() != listed.len() || self.steps.keys().any(|i| !listed.contains_key(i));
        self.steps.retain(|i, _| listed.contains_key(i));
        let stale: Vec<i64> = listed
            .iter()
            .filter(|(i, (kind, status, len))| {
                self.steps.get(i).is_none_or(|s| (s.kind, s.status, s.len) != (*kind, *status, *len))
            })
            .map(|(i, _)| *i)
            .collect();
        if stale.is_empty() {
            return Some(changed);
        }
        let only = if stale.len() == listed.len() {
            String::new()
        } else {
            let list: Vec<String> = stale.iter().map(i64::to_string).collect();
            format!(" where idx in ({})", list.join(","))
        };
        let rows = chat::sqlite_rows(
            sqlite3,
            db,
            &format!("select idx, step_type, status, length(step_payload) as len, hex(step_payload) as payload from steps{only}"),
        )?;
        for row in rows {
            let index = int(&row, "idx");
            let (kind, status, len) = (int(&row, "step_type"), int(&row, "status"), int(&row, "len"));
            let payload = row.get("payload").and_then(Value::as_str).and_then(hex_bytes).unwrap_or_default();
            let items = step_items(index, kind, status, &payload);
            if self.steps.get(&index).is_none_or(|s| s.items != items) {
                changed = true;
            }
            self.steps.insert(index, Step { kind, status, len, items });
        }
        Some(changed)
    }

    /// The conversation's items, in step order.
    pub fn items(&self) -> Vec<Item> {
        self.steps.values().flat_map(|s| s.items.iter().cloned()).collect()
    }
}

#[cfg(test)]
#[path = "../tests/remote_agy.rs"]
mod tests;
