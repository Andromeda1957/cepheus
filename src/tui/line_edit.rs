//! A one-line text field with a cursor, edited as a shell's or Neovim's command line is. The text and its
//! cursor change together: replacing the text puts the cursor at its end, so no cursor is left pointing into a
//! line something else wrote.

use std::fmt;
use std::ops::Deref;

/// The text and the cursor, a byte offset on a character boundary.
#[derive(Clone, Default)]
pub struct LineEdit {
    text: String,
    cursor: usize,
}

impl LineEdit {
    /// Replace the text; the cursor goes to its end.
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// The text, leaving the line empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// The text before the cursor and the text after it.
    pub fn split(&self) -> (&str, &str) {
        self.text.split_at(self.cursor)
    }

    /// Type `text` at the cursor, which moves past it.
    pub fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// Replace the text from byte `start` to the cursor with `text`, leaving
    /// the cursor after it: Tab completes the word before the cursor.
    pub fn replace_before(&mut self, start: usize, text: &str) {
        self.text.replace_range(start..self.cursor, text);
        self.cursor = start + text.len();
    }

    /// Backspace: the character before the cursor goes.
    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    /// Delete: the character under the cursor goes.
    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.len();
    }

    /// To the start of this word or the one before, as a shell's
    /// Ctrl+Left moves.
    pub fn word_left(&mut self) {
        let before = self.text[..self.cursor].trim_end();
        self.cursor = before.rfind(char::is_whitespace).map_or(0, |at| at + 1);
    }

    /// To the end of this word or the next, as a shell's Ctrl+Right moves.
    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let word = after.len() - after.trim_start().len();
        let rest = &after[word..];
        self.cursor += word + rest.find(char::is_whitespace).unwrap_or(rest.len());
    }
}

impl Deref for LineEdit {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl From<&str> for LineEdit {
    fn from(text: &str) -> Self {
        let mut line = LineEdit::default();
        line.set(text);
        line
    }
}

impl PartialEq<&str> for LineEdit {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl fmt::Display for LineEdit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl fmt::Debug for LineEdit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (before, after) = self.split();
        write!(f, "{before:?}▏{after:?}")
    }
}

#[cfg(test)]
#[path = "../tests/line_edit.rs"]
mod tests;
