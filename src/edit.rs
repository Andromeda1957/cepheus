//! Text being typed into a box: a cursor that moves by character, word and line, deleting the same ways,
//! pasting at the cursor, and stepping back through earlier entries as a shell's history does. First used by
//! other text composers.

/// The text, the cursor's byte offset in it (always between characters),
/// and the earlier entry being shown with the text typed before browsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    text: String,
    cursor: usize,
    recall: Option<(usize, String)>,
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Draft {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The text, leaving the draft empty.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        *self = Self::default();
        text
    }

    fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    fn edited(&mut self) {
        self.recall = None;
    }

    /// Type or paste `text` at the cursor. Line breaks of any kind become
    /// `\n` and a tab four spaces; other control characters are dropped.
    pub fn insert(&mut self, text: &str) {
        let clean: String = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
            .chars()
            .filter(|c| *c == '\n' || !c.is_control())
            .collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.edited();
    }

    fn before(&self) -> &str {
        &self.text[..self.cursor]
    }

    fn after(&self) -> &str {
        &self.text[self.cursor..]
    }

    fn prev(&self) -> usize {
        self.before().char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.after().chars().next().map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    /// Where the word before the cursor starts: past spaces and punctuation,
    /// then past the word.
    fn word_start(&self) -> usize {
        let mut at = self.cursor;
        let mut chars = self.before().char_indices().rev().peekable();
        while let Some((i, c)) = chars.peek().copied() {
            if word_char(c) {
                break;
            }
            at = i;
            chars.next();
        }
        for (i, c) in chars {
            if !word_char(c) {
                break;
            }
            at = i;
        }
        at
    }

    /// Where the word after the cursor ends.
    fn word_end(&self) -> usize {
        let mut at = self.cursor;
        let mut chars = self.after().chars().peekable();
        while let Some(c) = chars.peek().copied() {
            if word_char(c) {
                break;
            }
            at += c.len_utf8();
            chars.next();
        }
        for c in chars {
            if !word_char(c) {
                break;
            }
            at += c.len_utf8();
        }
        at
    }

    fn line_start(&self) -> usize {
        self.before().rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.after().find('\n').map_or(self.text.len(), |i| self.cursor + i)
    }

    pub fn left(&mut self) {
        self.cursor = self.prev();
    }

    pub fn right(&mut self) {
        self.cursor = self.next();
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_end();
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    fn remove(&mut self, from: usize, to: usize) {
        if from < to {
            self.text.replace_range(from..to, "");
            self.cursor = from;
            self.edited();
        }
    }

    pub fn backspace(&mut self) {
        self.remove(self.prev(), self.cursor);
    }

    pub fn delete(&mut self) {
        self.remove(self.cursor, self.next());
    }

    pub fn delete_word_back(&mut self) {
        self.remove(self.word_start(), self.cursor);
    }

    pub fn delete_word_forward(&mut self) {
        self.remove(self.cursor, self.word_end());
    }

    /// Delete to the end of the line the cursor is on.
    pub fn kill_line(&mut self) {
        self.remove(self.cursor, self.line_end());
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.edited();
    }

    /// Show the entry before the one shown, `entries` newest first; the
    /// text being typed comes back after the newest. False at the oldest.
    pub fn older(&mut self, entries: &[String]) -> bool {
        let at = self.recall.as_ref().map_or(0, |(at, _)| at + 1);
        let Some(entry) = entries.get(at) else { return false };
        let typed = match self.recall.take() {
            Some((_, typed)) => typed,
            None => self.text.clone(),
        };
        self.set(entry.clone());
        self.recall = Some((at, typed));
        true
    }

    /// Show the entry after the one shown, then the text that was being typed.
    pub fn newer(&mut self, entries: &[String]) -> bool {
        match self.recall.take() {
            None => false,
            Some((0, typed)) => {
                self.set(typed);
                true
            }
            Some((at, typed)) => {
                self.set(entries.get(at - 1).cloned().unwrap_or_default());
                self.recall = Some((at - 1, typed));
                true
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/edit.rs"]
mod tests;
