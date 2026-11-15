//! The Dot session's small composer, with bracketed paste for desktop and phone.
use std::io::{self, Write};

use super::{is_space, Error};

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";
/// The longest message ChatGPT accepts; longer input is never sent truncated.
pub const MESSAGE_MAX: usize = 32000;
const BUFFER_MAX: usize = 128_000;
#[cfg(test)]
#[path = "../../src/tests/dot_terminal.rs"]
mod tests;
const OVERFLOW: &str = "Message exceeds 32000 characters; Ctrl-U to clear. Nothing sent.";

/// What the owner submitted.
#[derive(Debug, PartialEq)]
pub enum Input {
    Line(String),
    Quit,
}

pub struct Terminal {
    buffer: String,
    /// Characters in `buffer`.
    count: usize,
    escape: String,
    pasting: bool,
    overflow: bool,
    /// `(role, text)`: `user` or `assistant`.
    pub messages: Vec<(String, String)>,
    /// The failure shown below the conversation, if any.
    pub failure: Option<String>,
}

/// Text without control characters other than newline and tab.
pub fn plain(text: &str) -> String {
    text.chars().filter(|&c| c == '\n' || c == '\t' || !c.is_control()).collect()
}

/// Python's `str.expandtabs(size)`.
fn expand_tabs(text: &str, size: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut column = 0;
    for c in text.chars() {
        match c {
            '\t' => {
                let spaces = size - column % size;
                out.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            '\n' | '\r' => {
                out.push(c);
                column = 0;
            }
            _ => {
                out.push(c);
                column += 1;
            }
        }
    }
    out
}

/// Python's `str.splitlines()` for text `plain` already cleaned.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (at, c) in text.char_indices() {
        if matches!(c, '\n' | '\u{2028}' | '\u{2029}') {
            lines.push(&text[start..at]);
            start = at + c.len_utf8();
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

impl Default for Terminal {
    fn default() -> Self {
        Terminal {
            buffer: String::new(),
            count: 0,
            escape: String::new(),
            pasting: false,
            overflow: false,
            messages: Vec::new(),
            failure: None,
        }
    }
}

impl Terminal {
    /// Whether the owner has begun a message: text, a paste or a key sequence.
    pub fn composing(&self) -> bool {
        !self.buffer.is_empty() || !self.escape.is_empty() || self.pasting
    }

    fn append(&mut self, c: char) {
        if self.count >= MESSAGE_MAX {
            self.overflow = true;
        }
        if self.count < BUFFER_MAX {
            self.buffer.push(c);
            self.count += 1;
        }
    }

    /// Take typed or pasted text. A paste keeps its newlines and waits for
    /// Enter; an overlong message is refused whole.
    pub fn feed(&mut self, text: &str) -> Vec<Input> {
        let mut inputs = Vec::new();
        for c in text.chars() {
            if !self.escape.is_empty() {
                self.escape.push(c);
                if self.escape == PASTE_START {
                    self.pasting = true;
                    self.escape.clear();
                } else if self.escape == PASTE_END {
                    self.pasting = false;
                    self.escape.clear();
                } else if !(PASTE_START.starts_with(&self.escape) || PASTE_END.starts_with(&self.escape))
                    && (c.is_alphabetic() || c == '~' || self.escape.chars().count() > 16)
                {
                    self.escape.clear();
                }
                continue;
            }
            match c {
                '\x1b' => self.escape.push(c),
                _ if self.pasting => self.append(c),
                '\r' | '\n' => {
                    if self.overflow {
                        self.failure = Some(OVERFLOW.into());
                    } else if !self.buffer.chars().all(is_space) {
                        inputs.push(Input::Line(std::mem::take(&mut self.buffer)));
                        self.count = 0;
                    }
                }
                '\x03' | '\x04' => inputs.push(Input::Quit),
                '\x7f' | '\x08' => {
                    self.count -= usize::from(self.buffer.pop().is_some());
                }
                '\x15' => {
                    self.buffer.clear();
                    self.count = 0;
                    self.overflow = false;
                }
                _ if (c as u32) >= 32 => self.append(c),
                _ => {}
            }
        }
        inputs
    }

    /// The whole screen: the latest conversation rows, wrapped between
    /// words, then any failure and the composer.
    pub fn render(&self, columns: usize, rows: usize) -> String {
        let columns = columns.max(1);
        let mut wrapped = crate::wrap::text("Dot | /reconnect /resolve /quit", "", "", columns);
        for (role, text) in &self.messages {
            let label = if role == "user" { "You: " } else { "Dot: " };
            let hang = " ".repeat(label.len());
            wrapped.push(String::new());
            let text = expand_tabs(&plain(text), 4);
            let lines = split_lines(&text);
            for (at, line) in lines.iter().enumerate() {
                wrapped.extend(crate::wrap::text(line, if at == 0 { label } else { &hang }, &hang, columns));
            }
            if lines.is_empty() {
                wrapped.push(label.trim_end().into());
            }
        }
        let failure =
            self.failure.as_deref().map(|f| crate::wrap::text(&plain(f), "", "", columns)).unwrap_or_default();
        let keep = rows.saturating_sub(2 + failure.len()).max(1);
        let mut view: Vec<String> = wrapped.split_off(wrapped.len().saturating_sub(keep));
        view.push(String::new());
        view.extend(failure);
        view.push(format!("› {}", plain(&self.buffer).replace('\n', " ↵ ")));
        format!("\x1b[2J\x1b[H{}", view.join("\r\n"))
    }
}

/// The terminal size: `COLUMNS`/`LINES`, the terminal's own, else 100x30.
pub fn size() -> (usize, usize) {
    let env = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<usize>().ok()).filter(|&v| v > 0);
    // SAFETY: TIOCGWINSZ fills a winsize the call owns.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    let columns = env("COLUMNS").or((ok && ws.ws_col > 0).then_some(ws.ws_col as usize)).unwrap_or(100);
    let rows = env("LINES").or((ok && ws.ws_row > 0).then_some(ws.ws_row as usize)).unwrap_or(30);
    (columns, rows)
}

/// Raw input with bracketed paste on while it lives. The service sends a
/// message only to a session that has turned bracketed paste on.
pub struct Raw {
    previous: libc::termios,
}

/// The owner's terminal settings, which a newer build taking over the chat
/// restores on quit, passed to it by the build it replaces.
const INHERITED: &str = "CEPHEUS_DOT_TERMINAL";

impl Raw {
    pub fn enter() -> Result<Raw, Error> {
        // SAFETY: isatty, tcgetattr and tcsetattr on standard input with a
        // termios this function owns.
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) != 1 {
                return Err(Error::Invalid("Dot chat needs a terminal; use probe for noninteractive checks.".into()));
            }
            // Taking over from an earlier build: the terminal is already raw,
            // and keys typed during the switch stay for this build to read.
            let inherited = std::env::var(INHERITED).ok();
            if inherited.is_some() {
                std::env::remove_var(INHERITED);
            }
            if let Some(previous) = inherited.as_deref().and_then(decode_settings) {
                let mut out = io::stdout().lock();
                let _ = out.write_all(b"\x1b[?2004h");
                let _ = out.flush();
                return Ok(Raw { previous });
            }
            let mut previous: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut previous) != 0 {
                return Err(io::Error::last_os_error().into());
            }
            let mut raw = previous;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) != 0 {
                return Err(io::Error::last_os_error().into());
            }
            let mut out = io::stdout().lock();
            let _ = out.write_all(b"\x1b[?2004h");
            let _ = out.flush();
            Ok(Raw { previous })
        }
    }

    /// Let `command`, a newer build of this chat run in this process, keep
    /// the terminal as it is and restore the owner's settings on quit.
    pub fn pass_to(&self, command: &mut std::process::Command) {
        command.env(INHERITED, encode_settings(&self.previous));
    }
}

fn encode_settings(s: &libc::termios) -> String {
    let cc: String = s.c_cc.iter().map(|b| format!("{b:02x}")).collect();
    format!("{} {} {} {} {} {cc} {} {}", s.c_iflag, s.c_oflag, s.c_cflag, s.c_lflag, s.c_line, s.c_ispeed, s.c_ospeed)
}

fn decode_settings(text: &str) -> Option<libc::termios> {
    let mut words = text.split(' ');
    // SAFETY: an all-zero termios is a valid value; each field is then set.
    let mut s: libc::termios = unsafe { std::mem::zeroed() };
    s.c_iflag = words.next()?.parse().ok()?;
    s.c_oflag = words.next()?.parse().ok()?;
    s.c_cflag = words.next()?.parse().ok()?;
    s.c_lflag = words.next()?.parse().ok()?;
    s.c_line = words.next()?.parse().ok()?;
    let cc = words.next()?;
    if cc.len() != s.c_cc.len() * 2 {
        return None;
    }
    for (i, slot) in s.c_cc.iter_mut().enumerate() {
        *slot = u8::from_str_radix(cc.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    s.c_ispeed = words.next()?.parse().ok()?;
    s.c_ospeed = words.next()?.parse().ok()?;
    words.next().is_none().then_some(s)
}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: restores the attributes saved by `enter`.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &self.previous) };
        let mut out = io::stdout().lock();
        let _ = out.write_all(b"\x1b[?2004l\r\n");
        let _ = out.flush();
    }
}

/// UTF-8 decoding across reads, replacing invalid bytes.
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
}

impl Decoder {
    pub fn decode(&mut self, data: &[u8]) -> String {
        self.pending.extend_from_slice(data);
        let mut out = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    rest = &[];
                    break;
                }
                Err(err) => {
                    let (valid, after) = rest.split_at(err.valid_up_to());
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match err.error_len() {
                        Some(bad) => {
                            out.push('\u{fffd}');
                            rest = &after[bad..];
                        }
                        None => {
                            rest = after;
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        out
    }
}
