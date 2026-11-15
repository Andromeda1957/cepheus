//! Two-to-four-word session titles. Long prompts and provider-generated titles can exceed the desired display
//! width. A chat-derived title longer than four words is summarized once by Claude Haiku in the background; the
//! original remains visible until a summary is available.
//!
//! The summary runs `claude -p` from the state directory with no settings sources, tools, MCP servers or saved
//! session, receiving input on standard input. Results are cached in `titles.json` by the text's SHA-256 so
//! restarts do not repeat the request.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// A title of at most this many words is shown as it is.
pub const MAX_WORDS: usize = 4;
/// The longest summary kept, in characters.
const MAX_CHARS: usize = 40;
/// How much of a long prompt the summary reads.
const MAX_INPUT: usize = 2_000;
/// Summaries asked for at once; the rest wait their turn.
const RUNNING: usize = 2;
/// A summary that takes longer is given up.
const TIMEOUT: Duration = Duration::from_secs(60);
/// A text whose summary failed is not asked again before this.
const RETRY: Duration = Duration::from_secs(10 * 60);
/// Summaries kept in `titles.json`, the oldest dropped first.
const KEPT: usize = 1_000;

const INSTRUCTION: &str = "You name chat sessions in a terminal list. The user message is a request someone gave a \
coding agent, or a long title for it. Reply with only a 2 to 4 word title in sentence case that summarizes what is \
asked. No quotes and no punctuation at the end.";

/// Whether `text` is longer than a title should be.
pub fn too_long(text: &str) -> bool {
    text.split_whitespace().count() > MAX_WORDS
}

/// The title in a summarizer's reply: its first line without quotes, list
/// marks or closing punctuation, at most four words and `MAX_CHARS`
/// characters, with no control characters. None when nothing is left.
pub fn clean(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.trim_start_matches(['-', '*', '#', '>', ' ']);
    let line = line.strip_prefix("Title:").unwrap_or(line);
    let words: Vec<String> = line
        .split_whitespace()
        .take(MAX_WORDS)
        .map(|w| w.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|w| !w.is_empty())
        .collect();
    let mut title = words.join(" ");
    let quotes = ['"', '\'', '`', '“', '”', '‘', '’', '*'];
    title = title.trim_matches(|c: char| quotes.contains(&c) || c.is_whitespace()).to_string();
    title = title.trim_end_matches(['.', '!', '?', ',', ';', ':']).to_string();
    if title.chars().count() > MAX_CHARS {
        title = title.chars().take(MAX_CHARS).collect::<String>().trim_end().to_string();
    }
    (!title.is_empty()).then_some(title)
}

fn key(text: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, text.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    /// `[key, title]`, oldest first.
    titles: Vec<(String, String)>,
}

pub struct Titles {
    /// The summarizer's program and arguments; the text goes on its input.
    /// None turns summaries off.
    command: Option<Vec<String>>,
    /// Where summaries are kept, and the directory the summarizer runs in.
    dir: Option<PathBuf>,
    done: HashMap<String, String>,
    order: VecDeque<String>,
    asked: HashSet<String>,
    failed: HashMap<String, Instant>,
    queue: VecDeque<(String, String)>,
    running: usize,
    tx: Sender<(String, Option<String>)>,
    rx: Receiver<(String, Option<String>)>,
}

impl Titles {
    /// Summaries off: every title stays as it is.
    pub fn off() -> Titles {
        Titles::new(None, None)
    }

    /// Summaries by `claude` (the configured command and its arguments),
    /// kept in `state_dir`.
    pub fn claude(claude: &[String], state_dir: PathBuf) -> Titles {
        let mut command = claude.to_vec();
        command.extend(
            [
                "-p",
                "--model",
                "haiku",
                "--setting-sources",
                "",
                "--strict-mcp-config",
                "--tools",
                "",
                "--no-session-persistence",
                "--disable-slash-commands",
                "--system-prompt",
                INSTRUCTION,
            ]
            .map(String::from),
        );
        Titles::new(Some(command), Some(state_dir))
    }

    pub fn new(command: Option<Vec<String>>, dir: Option<PathBuf>) -> Titles {
        let (tx, rx) = channel();
        let mut titles = Titles {
            command: command.filter(|c| !c.is_empty()),
            dir,
            done: HashMap::new(),
            order: VecDeque::new(),
            asked: HashSet::new(),
            failed: HashMap::new(),
            queue: VecDeque::new(),
            running: 0,
            tx,
            rx,
        };
        titles.load();
        titles
    }

    fn file(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join("titles.json"))
    }

    fn load(&mut self) {
        let Some(saved) =
            self.file().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice::<Saved>(&b).ok())
        else {
            return;
        };
        for (key, title) in saved.titles {
            if let Some(title) = clean(&title) {
                self.remember(key, title);
            }
        }
    }

    fn save(&self) {
        let Some(file) = self.file() else { return };
        let titles = self.order.iter().filter_map(|k| Some((k.clone(), self.done.get(k)?.clone()))).collect();
        if let Ok(bytes) = serde_json::to_vec(&Saved { titles }) {
            let _ = crate::util::write_atomic(&file, &bytes);
        }
    }

    fn remember(&mut self, key: String, title: String) {
        if self.done.insert(key.clone(), title).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > KEPT {
            if let Some(old) = self.order.pop_front() {
                self.done.remove(&old);
            }
        }
    }

    /// The title to show for `text`: as it is when it is short enough or
    /// summaries are off, else its summary once there is one. A long text
    /// without one is queued to be summarized and shown as it is meanwhile.
    pub fn short(&mut self, text: &str) -> String {
        if !too_long(text) || self.command.is_none() {
            return text.to_string();
        }
        let key = key(text);
        if let Some(title) = self.done.get(&key) {
            return title.clone();
        }
        let failed_recently = self.failed.get(&key).is_some_and(|at| at.elapsed() < RETRY);
        if !failed_recently && self.asked.insert(key.clone()) {
            let input: String = text.chars().take(MAX_INPUT).collect();
            self.queue.push_back((key, input));
            self.start_queued();
        }
        text.to_string()
    }

    fn start_queued(&mut self) {
        let Some(command) = self.command.clone() else { return };
        while self.running < RUNNING {
            let Some((key, text)) = self.queue.pop_front() else { break };
            self.running += 1;
            let (tx, dir, command) = (self.tx.clone(), self.dir.clone(), command.clone());
            std::thread::spawn(move || {
                let title = summarize(&command, dir.as_deref(), &text);
                let _ = tx.send((key, title));
            });
        }
    }

    /// Take the summaries that have come; true when any did.
    pub fn collect(&mut self) -> bool {
        let mut any = false;
        while let Ok((key, title)) = self.rx.try_recv() {
            self.running = self.running.saturating_sub(1);
            self.asked.remove(&key);
            match title {
                Some(title) => {
                    self.failed.remove(&key);
                    self.remember(key, title);
                    any = true;
                }
                None => {
                    self.failed.insert(key, Instant::now());
                }
            }
        }
        if any {
            self.save();
        }
        self.start_queued();
        any
    }
}

/// Run the summarizer on `text` and read its title, or None when it fails,
/// says nothing usable or takes longer than `TIMEOUT`.
fn summarize(command: &[String], dir: Option<&std::path::Path>, text: &str) -> Option<String> {
    let (program, args) = command.split_first()?;
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    // A hosted session's own hooks talk to the service through this; the
    // summarizer is not a session.
    for name in crate::paths::env_names("SOCKET") {
        cmd.env_remove(name);
    }
    crate::client_temp::contain(&mut cmd, "title").ok()?;
    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.join().ok()?;
    status.filter(|s| s.success()).and_then(|_| clean(&out))
}

#[cfg(test)]
#[path = "../../src/tests/titles.rs"]
mod tests;
