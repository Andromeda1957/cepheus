//! The `:` line's history, as a shell keeps its own. `↑` and `↓` (`Ctrl-p`, `Ctrl-n`) step through the lines
//! entered at `:`, newest first; `↓` past the newest gives back the line being typed. Only lines typed and
//! entered there are kept, never a command a key, a script or a dialog runs. The deck keeps the newest
//! `command_history` of them (0: none) in the state folder for the next deck.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::App;
use crate::util;

/// The lines entered at `:`, oldest first, the file that keeps them (none in
/// tests that do not ask), and where browsing them stands.
#[derive(Default)]
pub struct CommandHistory {
    entries: Vec<String>,
    file: Option<PathBuf>,
    browse: Option<Browse>,
}

/// The entry on the line, the line typed before browsing began, and the line
/// as the command line's own keys last left it: a line something else
/// replaced (a prompt that fills the line in) ends browsing.
struct Browse {
    at: usize,
    draft: String,
    line: String,
}

/// The history in `path`; a missing or unreadable file is an empty one.
fn read(path: &Path) -> Vec<String> {
    fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

impl CommandHistory {
    /// The history kept in `path`, as saved there.
    pub fn load(path: PathBuf) -> Self {
        Self { entries: read(&path), file: Some(path), browse: None }
    }

    /// Keep `line`, entered at `:`, unless it is empty or repeats the newest
    /// entry, keeping at most `limit` (0: none). The file is read again first
    /// so another deck's lines stay.
    fn record(&mut self, line: &str, limit: usize) -> io::Result<()> {
        self.browse = None;
        let line = line.trim();
        if limit == 0 || line.is_empty() {
            return Ok(());
        }
        if let Some(path) = &self.file {
            self.entries = read(path);
        }
        if self.entries.last().is_some_and(|last| last == line) {
            return Ok(());
        }
        self.entries.push(line.to_string());
        let excess = self.entries.len().saturating_sub(limit);
        self.entries.drain(..excess);
        match &self.file {
            Some(path) => util::write_atomic(path, &serde_json::to_vec(&self.entries)?),
            None => Ok(()),
        }
    }

    /// The entry before the one on the line, or the newest as browsing
    /// begins with `line` kept for `↓`; none when there is nothing older.
    fn older(&mut self, line: &str, limit: usize) -> Option<String> {
        let first = self.entries.len().saturating_sub(limit);
        let at = self.browse.as_ref().map_or(self.entries.len(), |b| b.at);
        if at <= first {
            return None;
        }
        let draft = self.browse.take().map_or_else(|| line.to_string(), |b| b.draft);
        Some(self.show(at - 1, draft))
    }

    /// The entry after the one on the line, and past the newest the line
    /// typed before browsing; none when not browsing.
    fn newer(&mut self) -> Option<String> {
        let browse = self.browse.take()?;
        if browse.at + 1 < self.entries.len() {
            return Some(self.show(browse.at + 1, browse.draft));
        }
        Some(browse.draft)
    }

    fn show(&mut self, at: usize, draft: String) -> String {
        let line = self.entries[at].clone();
        self.browse = Some(Browse { at, draft, line: line.clone() });
        line
    }

    /// Before a key on the command line: a line something else replaced
    /// since ends browsing.
    fn follow(&mut self, line: &str) {
        if self.browse.as_ref().is_some_and(|b| b.line != line) {
            self.browse = None;
        }
    }

    /// After a key or a paste edited the line: browsing goes on from the
    /// same entry.
    fn edited(&mut self, line: &str) {
        if let Some(browse) = &mut self.browse {
            browse.line = line.to_string();
        }
    }
}

impl App {
    /// The command line opens: browsing starts again from the newest.
    pub(super) fn command_history_stop(&mut self) {
        self.command_history.browse = None;
    }

    /// Before a key on the command line.
    pub(super) fn command_history_follow(&mut self) {
        self.command_history.follow(&self.command);
    }

    /// After a key or a paste on the command line.
    pub(super) fn command_history_edited(&mut self) {
        self.command_history.edited(&self.command);
    }

    /// `↑` (`Ctrl-p`) on the command line.
    pub(super) fn command_history_older(&mut self) {
        if let Some(line) = self.command_history.older(&self.command, self.cfg.command_history) {
            self.command.set(line);
        }
    }

    /// `↓` (`Ctrl-n`) on the command line.
    pub(super) fn command_history_newer(&mut self) {
        if let Some(line) = self.command_history.newer() {
            self.command.set(line);
        }
    }

    /// Enter on the command line: keep the line for `↑`.
    pub(super) fn command_history_record(&mut self, line: &str) {
        if let Err(err) = self.command_history.record(line, self.cfg.command_history) {
            let path = self.command_history.file.as_deref().unwrap_or(Path::new(""));
            self.local_error(format!("Could not save the command history to {}: {err}", path.display()));
        }
    }
}

#[cfg(test)]
#[path = "../tests/command_history.rs"]
mod tests;
