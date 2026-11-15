//! Conversation-scoped unresolved questions. Ordinary chat stays bounded;
//! only question metadata and complete answer receipts are read from the
//! older prefix, one transcript record at a time, then incrementally.
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use super::{codex_record, codex_text, question_replies, ChatQuestion, CodexMode, Item, ModelSeen};

pub(super) fn refresh(
    path: &Path,
    mode: CodexMode,
    offset: &mut u64,
    pending: &mut Vec<ChatQuestion>,
    replies: &mut Vec<String>,
) -> bool {
    refresh_until(path, mode, offset, pending, replies, &|| true)
}

pub(super) fn refresh_until(
    path: &Path,
    mode: CodexMode,
    offset: &mut u64,
    pending: &mut Vec<ChatQuestion>,
    replies: &mut Vec<String>,
    live: &impl Fn() -> bool,
) -> bool {
    if mode != CodexMode::Items {
        return false;
    }
    let Ok(mut file) = File::open(path) else { return false };
    let Ok(meta) = file.metadata() else { return false };
    if file.seek(SeekFrom::Start(*offset)).is_err() {
        return false;
    }
    let mut reader = BufReader::new(file.take(meta.len().saturating_sub(*offset)));
    let mut line = Vec::new();
    let mut changed = false;
    loop {
        if !live() {
            break;
        }
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line) else { break };
        if read == 0 || line.last() != Some(&b'\n') {
            // A partial envelope is revisited from its start next refresh.
            break;
        }
        *offset += read as u64;
        let Ok(record) = serde_json::from_slice::<Value>(&line) else { continue };
        if record["type"] != "event_msg" || record["payload"]["type"] != "item_completed" {
            continue;
        }
        let item = &record["payload"]["item"];
        match item["type"].as_str() {
            Some("AgentMessage") if item.get("questions").is_some() => {
                // Share the chat parser's stable identities, clipping and
                // validation without materializing ordinary history.
                let mut items = Vec::new();
                codex_record(&record, None, mode, &mut items, &mut ModelSeen::default());
                for item in items {
                    if let Item::Assistant { questions, .. } = item {
                        for question in questions {
                            if !pending.iter().any(|q| q.id == question.id) {
                                pending.push(question);
                                changed = true;
                            }
                        }
                    }
                }
            }
            Some("UserMessage") => {
                let resolved = question_replies(&codex_text(&item["content"]));
                let before = pending.len();
                for id in resolved.iter().filter(|id| pending.iter().any(|q| &q.id == *id)) {
                    replies.push(id.clone());
                }
                // Pending cards have no age limit. Confirmation only needs
                // recent receipts; a cold reopen rebuilds these too.
                if replies.len() > 256 {
                    replies.drain(..replies.len() - 256);
                }
                pending.retain(|q| !resolved.contains(&q.id));
                changed |= before != pending.len();
            }
            _ => {}
        }
    }
    changed
}
