//! Search session conversations from the phone.
//!
//! Chat pages read only a log's last 24 MB, while conversation logs can be much larger. The index reads each
//! complete log once, retaining user messages and agent replies, then reads only appended content. Sessions
//! whose chat comes from the screen are searched within their retained screen history.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::SessionInfo;
use crate::remote::chat::{self, CodexMode, Dirs, Item, ModelSeen, Source};
use crate::util;

/// The longest search the phone may send, in bytes, and its words.
pub const QUERY_MAX: usize = 200;
const WORDS_MAX: usize = 8;
/// Matching messages shown per session.
const SNIPPETS: usize = 3;
/// A snippet's length in characters, and how much of it leads the match.
const SNIPPET_CHARS: usize = 160;
const SNIPPET_LEAD: usize = 50;
/// Messages kept per log; older ones are dropped first.
const MESSAGES_MAX: usize = 20_000;
/// Bytes of a log read at once.
const READ_CHUNK: u64 = 16 << 20;

/// Whose words a message holds.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The owner's own message.
    User,
    Assistant,
    /// A program whose screen is its chat.
    Screen,
}

/// One matching message, cut around its first match.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Snippet {
    pub role: Role,
    pub text: String,
    /// When it was written, in milliseconds, when the log says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
}

/// A session whose conversation holds every word of the search.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Hit {
    pub id: u64,
    /// Matching messages, and how many of them are the owner's.
    pub count: usize,
    pub mine: usize,
    /// The owner's matching messages first, then the agent's, newest first.
    pub snippets: Vec<Snippet>,
}

/// What a search of one machine's sessions found: the hits, and the
/// sessions to search on their screen instead.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Found {
    #[serde(default)]
    pub hits: Vec<Hit>,
    #[serde(default)]
    pub screen: Vec<u64>,
}

/// The words of a search, lowercased: every one must be in a message.
pub fn words(query: &str) -> Result<Vec<String>> {
    if query.len() > QUERY_MAX {
        bail!("a search is at most {QUERY_MAX} characters");
    }
    let mut out: Vec<String> = Vec::new();
    for w in query.split_whitespace().map(fold) {
        if !out.contains(&w) {
            out.push(w);
        }
    }
    if out.is_empty() {
        bail!("type something to search for");
    }
    if out.len() > WORDS_MAX {
        bail!("a search has at most {WORDS_MAX} words");
    }
    Ok(out)
}

/// Lowercase, one character for one, so a position in the folded text is
/// the same character's position in the original. A character whose
/// lowercase is longer (`İ`) stays as it is.
fn fold(text: &str) -> String {
    text.chars()
        .map(|c| {
            let mut lower = c.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(l), None) => l,
                _ => c,
            }
        })
        .collect()
}

/// `text` as one line around the first word's first place, with "…"
/// where it was cut.
fn snippet(text: &str, words: &[String]) -> String {
    let line = util::one_line(text);
    let folded = fold(&line);
    let first = words.iter().filter_map(|w| folded.find(w.as_str())).min().unwrap_or(0);
    let at = folded[..first].chars().count();
    let chars: Vec<char> = line.chars().collect();
    let start = if chars.len() <= SNIPPET_CHARS { 0 } else { at.saturating_sub(SNIPPET_LEAD) };
    let end = (start + SNIPPET_CHARS).min(chars.len());
    let start = end.saturating_sub(SNIPPET_CHARS).min(start);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// Session `id`'s hit among `messages` (role, text, time), oldest first;
/// none when no message holds every word.
pub fn hit<'a>(id: u64, messages: impl Iterator<Item = (Role, &'a str, Option<u64>)>, words: &[String]) -> Option<Hit> {
    let mut mine = Vec::new();
    let mut theirs = Vec::new();
    for (role, text, at) in messages {
        let folded = fold(text);
        if words.iter().all(|w| folded.contains(w.as_str())) {
            if role == Role::User {
                mine.push((role, text, at));
            } else {
                theirs.push((role, text, at));
            }
        }
    }
    let count = mine.len() + theirs.len();
    if count == 0 {
        return None;
    }
    let snippets = mine
        .iter()
        .rev()
        .chain(theirs.iter().rev())
        .take(SNIPPETS)
        .map(|(role, text, at)| Snippet { role: *role, text: snippet(text, words), at: *at })
        .collect();
    Some(Hit { id, count, mine: mine.len(), snippets })
}

/// The messages among chat items: the owner's, the agent's and a screen.
pub fn item_messages(items: &[Item]) -> impl Iterator<Item = (Role, &str, Option<u64>)> {
    items.iter().filter_map(|item| match item {
        Item::User { text, at, .. } => Some((Role::User, text.as_str(), *at)),
        Item::Assistant { text, at, .. } => Some((Role::Assistant, text.as_str(), *at)),
        Item::Screen { text } => Some((Role::Screen, text.as_str(), None)),
        _ => None,
    })
}

struct Message {
    role: Role,
    text: String,
    at: Option<u64>,
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Claude,
    Codex,
}

/// One log's messages, read up to `offset`.
struct LogIndex {
    format: Format,
    offset: u64,
    mode: Option<CodexMode>,
    messages: Vec<Message>,
}

impl LogIndex {
    fn new(format: Format) -> LogIndex {
        LogIndex { format, offset: 0, mode: None, messages: Vec::new() }
    }

    /// Read what the log gained, a chunk at a time, through its last whole line.
    fn refresh(&mut self, path: &Path) {
        let Ok(meta) = std::fs::metadata(path) else { return };
        if meta.len() < self.offset {
            // Rewritten: start again.
            *self = LogIndex::new(self.format);
        }
        if self.format == Format::Codex && self.mode.is_none() {
            self.mode = Some(chat::codex_mode(&util::read_head(path, 256 << 10).unwrap_or_default()));
        }
        let mut seen = ModelSeen::default();
        while self.offset < meta.len() {
            let to = meta.len().min(self.offset + READ_CHUNK);
            let Ok(bytes) = util::read_between(path, self.offset, to) else { return };
            let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else {
                if to == meta.len() {
                    return; // A line still being written.
                }
                // A line longer than a chunk: skip past it.
                self.offset = to;
                continue;
            };
            let mut items = Vec::new();
            for line in bytes[..end].split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
                match self.format {
                    Format::Claude => chat::claude_record(&v, None, &mut items, &mut seen),
                    Format::Codex => {
                        chat::codex_record(&v, None, self.mode.unwrap_or(CodexMode::Items), &mut items, &mut seen)
                    }
                }
            }
            self.messages.extend(items.into_iter().filter_map(|item| match item {
                Item::User { text, at, .. } => Some(Message { role: Role::User, text, at }),
                Item::Assistant { text, at, .. } => Some(Message { role: Role::Assistant, text, at }),
                _ => None,
            }));
            if self.messages.len() > MESSAGES_MAX {
                let extra = self.messages.len() - MESSAGES_MAX;
                self.messages.drain(..extra);
            }
            self.offset += end as u64 + 1;
        }
    }

    fn messages(&self) -> impl Iterator<Item = (Role, &str, Option<u64>)> {
        self.messages.iter().map(|m| (m.role, m.text.as_str(), m.at))
    }
}

/// The logs searched so far, by path.
#[derive(Default)]
pub struct Index {
    logs: HashMap<PathBuf, LogIndex>,
}

impl Index {
    /// Search the conversations of `sessions`, all of this machine, for
    /// every word of `words`. Logs of sessions no longer searched are let go.
    pub fn search(&mut self, sessions: &[SessionInfo], dirs: &Dirs, words: &[String]) -> Found {
        let mut found = Found::default();
        let mut used = HashSet::new();
        for info in sessions {
            let hit = match chat::source(info, dirs) {
                Some(Source::Claude(path)) => self.log_hit(info.id, &path, Format::Claude, words, &mut used),
                Some(Source::Codex(path)) => self.log_hit(info.id, &path, Format::Codex, words, &mut used),
                Some(Source::OpenCode(conversation)) => {
                    let sqlite3: Vec<&str> = dirs.sqlite3.iter().map(String::as_str).collect();
                    chat::opencode_items(&sqlite3, &dirs.opencode_db, &conversation)
                        .and_then(|(items, _)| hit(info.id, item_messages(&items), words))
                }
                Some(Source::Antigravity(path)) => {
                    let sqlite3: Vec<&str> = dirs.sqlite3.iter().map(String::as_str).collect();
                    let mut steps = super::agy::Steps::default();
                    steps.refresh(&sqlite3, &path).and_then(|_| hit(info.id, item_messages(&steps.items()), words))
                }
                Some(Source::Snapshot(path)) => {
                    crate::providers::snapshot::read(&path).and_then(|items| hit(info.id, item_messages(&items), words))
                }
                Some(Source::Screen) => {
                    found.screen.push(info.id);
                    None
                }
                Some(Source::Missing | Source::Remote) | None => None,
            };
            found.hits.extend(hit);
        }
        self.logs.retain(|path, _| used.contains(path));
        found
    }

    fn log_hit(
        &mut self,
        id: u64,
        path: &Path,
        format: Format,
        words: &[String],
        used: &mut HashSet<PathBuf>,
    ) -> Option<Hit> {
        used.insert(path.to_path_buf());
        let log = self.logs.entry(path.to_path_buf()).or_insert_with(|| LogIndex::new(format));
        log.refresh(path);
        hit(id, log.messages(), words)
    }
}

#[cfg(test)]
#[path = "../tests/remote_search.rs"]
mod tests;
