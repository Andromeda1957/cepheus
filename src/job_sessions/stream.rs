//! Private job-output adapters. Input is a bounded accumulated log window,
//! not an independently paginated byte chunk. The caller caps it at 1 MiB,
//! discards a cut leading record and exposes truncation; a delta whose earlier
//! state fell outside that window is ignored. Clients replace the returned
//! chat snapshot. Item updates and partial assistant messages replace their
//! earlier representation. Syntactically incomplete final JSON waits for more
//! bytes. No path, image reference, command or URL in a record is opened.
use crate::{
    protocol::Tool,
    remote::chat::{self, Item, ModelSeen},
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

const TEXT_MAX: usize = 24_000;
const RESULT_MAX: usize = 6_000;
const INPUT_MAX: usize = 3_000;
const ITEMS_MAX: usize = 3_000;

fn clip(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let kept: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{kept}\n…")
    } else {
        kept
    }
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
fn pretty(value: &Value, max: usize) -> String {
    match value {
        Value::String(s) => clip(s, max),
        Value::Null => String::new(),
        value => clip(&serde_json::to_string_pretty(value).unwrap_or_default(), max),
    }
}
fn notice(text: impl AsRef<str>) -> Item {
    Item::Notice { text: clip(text.as_ref(), TEXT_MAX) }
}
fn assistant(text: &str) -> Item {
    Item::Assistant { text: clip(text, TEXT_MAX), questions: Vec::new(), at: None }
}

#[derive(Default)]
struct Output {
    parts: Vec<Vec<Item>>,
    keys: HashMap<String, usize>,
}
impl Output {
    fn put(&mut self, key: String, items: Vec<Item>) {
        if let Some(&index) = self.keys.get(&key) {
            self.parts[index] = items;
        } else {
            self.keys.insert(key, self.parts.len());
            self.parts.push(items);
        }
    }
    fn last_answer(&self) -> Option<&str> {
        self.parts.iter().rev().flat_map(|part| part.iter().rev()).find_map(|item| match item {
            Item::Assistant { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }
}

#[derive(Default)]
struct Block {
    value: Value,
    input: String,
}
#[derive(Default)]
struct ClaudeMessage {
    blocks: BTreeMap<u64, Block>,
    complete: Vec<(String, Value)>,
}
impl ClaudeMessage {
    fn render(&self) -> Vec<Item> {
        let mut out = Vec::new();
        let mut seen = ModelSeen::default();
        if self.complete.is_empty() {
            let blocks: Vec<&Value> = self.blocks.values().map(|block| &block.value).collect();
            chat::claude_record(&json!({"type":"assistant","message":{"content":blocks}}), None, &mut out, &mut seen);
        } else {
            for (_, record) in &self.complete {
                chat::claude_record(record, None, &mut out, &mut seen);
            }
        }
        out
    }
}

fn codex_item(item: &Value, complete: bool) -> Vec<Item> {
    let kind = text(item, "type");
    let id = text(item, "id");
    let mut out = Vec::new();
    // These payloads match the shared rollout parser after their exec-specific
    // names and field shapes are normalized. Exec file changes have no diff.
    let normalized = match kind {
        "agent_message" => {
            Some(json!({"id":id,"type":"AgentMessage","content":[{"type":"output_text","text":text(item,"text")}]}))
        }
        "reasoning" => Some(json!({"id":id,"type":"Reasoning","summary_text":[text(item,"text")]})),
        "command_execution" => Some(json!({
            "id":id,"type":"CommandExecution","command":[text(item,"command")],
            "aggregated_output":text(item,"aggregated_output"),"exit_code":item.get("exit_code")
        })),
        _ => None,
    };
    if let Some(normalized_item) = normalized {
        chat::codex_record(
            &json!({"type":"event_msg","payload":{"type":"item_completed","item":normalized_item}}),
            None,
            chat::CodexMode::Items,
            &mut out,
            &mut ModelSeen::default(),
        );
        if kind == "command_execution" {
            if !complete && text(item, "aggregated_output").trim().is_empty() {
                out.retain(|item| !matches!(item, Item::Result { .. }));
            } else if matches!(text(item, "status"), "failed" | "declined") {
                for result in &mut out {
                    if let Item::Result { error, .. } = result {
                        *error = true;
                    }
                }
            }
        }
        return out;
    }
    let call = |name: &str, input: String, summary: String| Item::Tool {
        call: id.into(),
        name: name.into(),
        summary: clip(&summary, 160),
        input: clip(&input, INPUT_MAX),
        label: None,
        diff: None,
    };
    match kind {
        "file_change" => {
            let paths: Vec<&str> = item
                .get("changes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|change| change.get("path").and_then(Value::as_str))
                .collect();
            out.push(call("edit", pretty(&item["changes"], INPUT_MAX), paths.join(", ")));
            if complete && text(item, "status") == "failed" {
                out.push(Item::Result { call: id.into(), text: "File change failed".into(), error: true });
            }
        }
        "mcp_tool_call" => {
            let name = format!("mcp:{}.{}", text(item, "server"), text(item, "tool"));
            out.push(call(&name, pretty(&item["arguments"], INPUT_MAX), String::new()));
            if complete {
                let error = text(item, "status") == "failed" || item.get("error").is_some_and(|v| !v.is_null());
                let message = text(&item["error"], "message");
                let result = if !message.is_empty() {
                    message.to_string()
                } else {
                    let content = item.pointer("/result/content").and_then(Value::as_array);
                    let parts: Vec<&str> = content
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect();
                    if parts.is_empty() {
                        pretty(item.pointer("/result/structured_content").unwrap_or(&Value::Null), RESULT_MAX)
                    } else {
                        clip(&parts.join("\n"), RESULT_MAX)
                    }
                };
                out.push(Item::Result { call: id.into(), text: clip(&result, RESULT_MAX), error });
            }
        }
        "web_search" => {
            let query = text(item, "query");
            out.push(call("web_search", query.into(), query.into()));
            if complete && item.get("results").is_some() {
                out.push(Item::Result { call: id.into(), text: pretty(&item["results"], RESULT_MAX), error: false });
            }
        }
        "collab_tool_call" => {
            out.push(call("agents", pretty(item, INPUT_MAX), text(item, "tool").into()));
            if complete {
                out.push(Item::Result {
                    call: id.into(),
                    text: pretty(&item["agents_states"], RESULT_MAX),
                    error: text(item, "status") == "failed",
                });
            }
        }
        "todo_list" => {
            let tasks: Vec<String> = item
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|task| {
                    format!(
                        "{} {}",
                        if task["completed"].as_bool() == Some(true) { "✓" } else { "○" },
                        text(task, "text")
                    )
                })
                .collect();
            if !tasks.is_empty() {
                out.push(notice(tasks.join("\n")));
            }
        }
        "error" if !text(item, "message").is_empty() => out.push(notice(format!("Error: {}", text(item, "message")))),
        _ => {}
    }
    out
}

fn codex_type(kind: &str) -> bool {
    matches!(
        kind,
        "thread.started"
            | "turn.started"
            | "turn.completed"
            | "turn.failed"
            | "item.started"
            | "item.updated"
            | "item.completed"
    )
}
fn claude_type(kind: &str) -> bool {
    matches!(kind, "assistant" | "user" | "stream_event" | "result" | "system")
}

fn claude_envelope(value: &Value) -> bool {
    match text(value, "type") {
        "assistant" | "user" => {
            value.pointer("/message/content").is_some_and(|content| content.is_array() || content.is_string())
        }
        "stream_event" => !text(&value["event"], "type").is_empty(),
        "result" | "system" => !text(value, "subtype").is_empty(),
        _ => false,
    }
}

fn error_message(value: &Value) -> Option<&str> {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/error/message").and_then(Value::as_str))
        .filter(|message| !message.is_empty())
}

fn incomplete_json(raw: &str) -> bool {
    let start = raw.trim_start();
    (start.starts_with('{') || start.starts_with('['))
        && serde_json::from_str::<Value>(raw).is_err_and(|error| error.is_eof())
}

fn append(value: &mut Value, field: &str, delta: &str, max: usize) {
    if !value.is_object() {
        return;
    }
    let mut current = text(value, field).to_string();
    current.push_str(delta);
    value[field] = Value::String(clip(&current, max));
}

/// Normalize an accumulated private job log into the existing chat schema.
/// Known providers ignore unknown JSON event kinds; unrelated commands keep
/// their plain output. Optional provider detection recognizes event envelopes,
/// never executable names, paths, tool arguments or arbitrary JSON contents.
pub fn items(provider: Option<Tool>, snapshot: &str) -> Vec<Item> {
    let plain = || {
        let mut value = snapshot;
        if provider.is_none() && !value.ends_with('\n') {
            let start = value.rfind('\n').map_or(0, |index| index + 1);
            if incomplete_json(&value[start..]) {
                value = &value[..start];
            }
        }
        let value = value.trim_end_matches(['\r', '\n']);
        if value.is_empty() {
            Vec::new()
        } else {
            vec![notice(value)]
        }
    };
    if provider.is_some_and(|tool| !matches!(tool, Tool::Claude | Tool::Codex)) {
        return plain();
    }
    if provider.is_none()
        && !snapshot.lines().any(|line| {
            serde_json::from_str::<Value>(line).ok().is_some_and(|v| {
                let kind = text(&v, "type");
                codex_type(kind) || claude_envelope(&v) || (kind == "error" && error_message(&v).is_some())
            })
        })
    {
        return plain();
    }
    let mut out = Output::default();
    let mut claude: HashMap<String, ClaudeMessage> = HashMap::new();
    let mut active: Option<String> = None;
    let mut turn = 0_u64;
    for (line_index, line) in snapshot.split_inclusive('\n').enumerate() {
        let raw = line.trim_end_matches(['\r', '\n']);
        if raw.trim().is_empty() {
            continue;
        }
        let key = format!("line:{line_index}");
        let value: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) if error.is_eof() && !line.ends_with('\n') && incomplete_json(raw) => continue,
            Err(_) => {
                out.put(key, vec![notice(raw)]);
                continue;
            }
        };
        let kind = text(&value, "type");
        if kind == "error" {
            if let Some(message) = error_message(&value) {
                out.put(key, vec![notice(format!("Error: {message}"))]);
            } else if provider.is_none() {
                out.put(key, vec![notice(raw)]);
            }
            continue;
        }
        if provider != Some(Tool::Claude) && codex_type(kind) {
            match kind {
                "turn.started" => turn += 1,
                "turn.completed" => out.put(format!("codex:turn:{turn}"), vec![notice("Completed")]),
                "turn.failed" => out.put(
                    format!("codex:turn:{turn}"),
                    vec![notice(format!("Failed: {}", text(&value["error"], "message")))],
                ),
                "item.started" | "item.updated" | "item.completed" => {
                    let item = &value["item"];
                    let id = text(item, "id");
                    let key = if id.is_empty() { key } else { format!("codex:{id}") };
                    out.put(key, codex_item(item, kind == "item.completed"));
                }
                _ => {}
            }
        } else if provider != Some(Tool::Codex) && claude_type(kind) && (provider.is_some() || claude_envelope(&value))
        {
            match kind {
                "stream_event" => {
                    let event = &value["event"];
                    match text(event, "type") {
                        "message_start" => {
                            let id = text(&event["message"], "id");
                            active = Some(if id.is_empty() { key.clone() } else { id.into() });
                        }
                        "message_stop" => active = None,
                        "content_block_start" | "content_block_delta" | "content_block_stop" => {
                            let (Some(id), Some(index)) = (active.as_ref(), event["index"].as_u64()) else {
                                continue;
                            };
                            let message = claude.entry(id.clone()).or_default();
                            match text(event, "type") {
                                "content_block_start" => {
                                    message.blocks.insert(
                                        index,
                                        Block { value: event["content_block"].clone(), input: String::new() },
                                    );
                                }
                                "content_block_delta" => {
                                    let Some(block) = message.blocks.get_mut(&index) else {
                                        continue;
                                    };
                                    let delta = &event["delta"];
                                    match text(delta, "type") {
                                        "text_delta" => append(&mut block.value, "text", text(delta, "text"), TEXT_MAX),
                                        "thinking_delta" => {
                                            append(&mut block.value, "thinking", text(delta, "thinking"), RESULT_MAX)
                                        }
                                        "input_json_delta" => {
                                            block.input.push_str(text(delta, "partial_json"));
                                            block.input = clip(&block.input, 128_000);
                                        }
                                        _ => {}
                                    }
                                }
                                "content_block_stop" => {
                                    if let Some(block) = message
                                        .blocks
                                        .get_mut(&index)
                                        .filter(|b| b.value.is_object() && !b.input.is_empty())
                                    {
                                        if let Ok(input) = serde_json::from_str(&block.input) {
                                            block.value["input"] = input;
                                        }
                                    }
                                }
                                _ => {}
                            }
                            out.put(format!("claude:{id}"), message.render());
                        }
                        _ => {}
                    }
                }
                "assistant" => {
                    let id = value
                        .pointer("/message/id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .or_else(|| value.get("uuid").and_then(Value::as_str))
                        .unwrap_or(&key);
                    let message = claude.entry(id.into()).or_default();
                    let record_id = value.get("uuid").and_then(Value::as_str).unwrap_or("message").to_string();
                    if let Some((_, record)) = message.complete.iter_mut().find(|(id, _)| *id == record_id) {
                        *record = value.clone();
                    } else {
                        message.complete.push((record_id, value.clone()));
                    }
                    out.put(format!("claude:{id}"), message.render());
                }
                "result" => {
                    let failed =
                        value["is_error"].as_bool() == Some(true) || text(&value, "subtype").starts_with("error");
                    if failed {
                        let errors: Vec<&str> =
                            value["errors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                        let reason = if !errors.is_empty() {
                            errors.join("\n")
                        } else if !text(&value, "result").is_empty() {
                            text(&value, "result").to_string()
                        } else {
                            text(&value, "subtype").to_string()
                        };
                        out.put(key, vec![notice(format!("Failed: {reason}"))]);
                    } else {
                        let answer = text(&value, "result").trim();
                        let shown = clip(answer, TEXT_MAX);
                        let mut completed = Vec::new();
                        if !answer.is_empty() && out.last_answer() != Some(shown.as_str()) {
                            completed.push(assistant(answer));
                        }
                        completed.push(notice("Completed"));
                        out.put(key, completed);
                    }
                }
                _ => {
                    let mut parsed = Vec::new();
                    chat::claude_record(&value, None, &mut parsed, &mut ModelSeen::default());
                    let record_id = value
                        .get("uuid")
                        .and_then(Value::as_str)
                        .map(|id| format!("claude:record:{id}"))
                        .unwrap_or(key);
                    out.put(record_id, parsed);
                }
            }
        } else if provider.is_none() {
            out.put(key, vec![notice(raw)]);
        }
    }
    let mut result: Vec<Item> = out.parts.into_iter().flatten().collect();
    if result.len() > ITEMS_MAX {
        result.drain(..result.len() - ITEMS_MAX);
    }
    result
}

#[cfg(test)]
#[path = "../tests/job_sessions/stream.rs"]
mod tests;
