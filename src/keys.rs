//! Encode outer-terminal input events as the bytes a hosted program expects
//! from an xterm-compatible terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

use crate::screen::{M_APP_CURSOR, M_BRACKETED_PASTE, M_MOUSE_ANY, M_MOUSE_DRAG, M_MOUSE_MOTION, M_SGR_MOUSE};

fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '-' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
fn modifier_param(m: KeyModifiers) -> u8 {
    1 + u8::from(m.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(m.contains(KeyModifiers::ALT))
        + 4 * u8::from(m.contains(KeyModifiers::CONTROL))
}

fn cursor_key(letter: char, m: KeyModifiers, modes: u32) -> String {
    let p = modifier_param(m);
    if p > 1 {
        format!("\x1b[1;{p}{letter}")
    } else if modes & M_APP_CURSOR != 0 {
        format!("\x1bO{letter}")
    } else {
        format!("\x1b[{letter}")
    }
}

/// The Up or Down arrow, as the program's cursor-key mode expects it.
pub fn arrow(up: bool, modes: u32) -> String {
    cursor_key(if up { 'A' } else { 'B' }, KeyModifiers::NONE, modes)
}

fn tilde_key(code: u8, m: KeyModifiers) -> String {
    let p = modifier_param(m);
    if p > 1 {
        format!("\x1b[{code};{p}~")
    } else {
        format!("\x1b[{code}~")
    }
}

/// Bytes for a key press, or `None` for keys with no terminal encoding.
pub fn encode_key(key: &KeyEvent, modes: u32) -> Option<String> {
    let m = key.modifiers;
    let shift = m.contains(KeyModifiers::SHIFT);
    let alt = m.contains(KeyModifiers::ALT);
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let esc = if alt { "\x1b" } else { "" };
    let out = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                if let Some(b) = ctrl_byte(c) {
                    return Some(format!("{esc}{}", b as char));
                }
            }
            format!("{esc}{c}")
        }
        KeyCode::Enter => match (shift, alt, ctrl) {
            (false, false, false) => "\r".to_string(),
            (false, true, false) => "\x1b\r".to_string(),
            _ => format!("\x1b[13;{}u", modifier_param(m)),
        },
        KeyCode::Tab if shift => "\x1b[Z".to_string(),
        KeyCode::Tab => format!("{esc}\t"),
        KeyCode::BackTab => "\x1b[Z".to_string(),
        KeyCode::Backspace if ctrl => format!("{esc}\x08"),
        KeyCode::Backspace => format!("{esc}\x7f"),
        KeyCode::Esc => format!("{esc}\x1b"),
        KeyCode::Up => cursor_key('A', m, modes),
        KeyCode::Down => cursor_key('B', m, modes),
        KeyCode::Right => cursor_key('C', m, modes),
        KeyCode::Left => cursor_key('D', m, modes),
        KeyCode::Home => cursor_key('H', m, modes),
        KeyCode::End => cursor_key('F', m, modes),
        KeyCode::Insert => tilde_key(2, m),
        KeyCode::Delete => tilde_key(3, m),
        KeyCode::PageUp => tilde_key(5, m),
        KeyCode::PageDown => tilde_key(6, m),
        KeyCode::F(n @ 1..=4) => {
            let letter = (b'P' + (n - 1)) as char;
            let p = modifier_param(m);
            if p > 1 {
                format!("\x1b[1;{p}{letter}")
            } else {
                format!("\x1bO{letter}")
            }
        }
        KeyCode::F(n @ 5..=12) => {
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            tilde_key(CODES[(n - 5) as usize], m)
        }
        KeyCode::Null => "\0".to_string(),
        _ => return None,
    };
    Some(out)
}

/// Text pasted into the outer terminal, wrapped when the program asked for
/// bracketed paste. Control characters other than line breaks and tab are
/// dropped: without ESC and its one-character form U+009B the text can neither
/// end the paste early nor, unbracketed, type escape-sequence keys. Removing
/// the paste markers instead was bypassable, because removing one nested
/// inside another rebuilt it.
pub fn encode_paste(text: &str, modes: u32) -> String {
    let clean: String = text.chars().filter(|&c| matches!(c, '\n' | '\r' | '\t') || !c.is_control()).collect();
    if modes & M_BRACKETED_PASTE != 0 {
        format!("\x1b[200~{clean}\x1b[201~")
    } else {
        clean.replace("\r\n", "\r").replace('\n', "\r")
    }
}

/// A message composed in the deck, as one bracketed paste. Control characters
/// other than newline and tab are dropped, so the text can never end the paste
/// early or carry its own key presses; the caller submits it separately.
pub fn encode_message(text: &str) -> String {
    let clean: String = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect();
    format!("\x1b[200~{}\x1b[201~", clean.trim_end_matches('\n'))
}

/// Mark an automatic delivery as an agent message rather than owner input.
/// Escape all markup in the body so a supplied closing tag cannot escape its
/// provenance. Save the original text separately; only delivery is encoded.
pub fn non_owner_message(text: &str) -> String {
    let body = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<cross-session-message source=\"cepheus\" encoding=\"html\">\n{body}\n</cross-session-message>")
}

/// Mouse report for a program that enabled mouse tracking. `col` and `row`
/// are zero-based within the hosted screen.
pub fn encode_mouse(kind: MouseEventKind, col: u16, row: u16, m: KeyModifiers, modes: u32) -> Option<String> {
    if modes & M_MOUSE_ANY == 0 {
        return None;
    }
    let button_code = |b: MouseButton| match b {
        MouseButton::Left => 0u16,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (mut cb, release) = match kind {
        MouseEventKind::Down(b) => (button_code(b), false),
        MouseEventKind::Up(b) => (button_code(b), true),
        MouseEventKind::Drag(b) => {
            if modes & (M_MOUSE_DRAG | M_MOUSE_MOTION) == 0 {
                return None;
            }
            (button_code(b) + 32, false)
        }
        MouseEventKind::Moved => {
            if modes & M_MOUSE_MOTION == 0 {
                return None;
            }
            (3 + 32, false)
        }
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    if m.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if m.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if m.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    if modes & M_SGR_MOUSE != 0 {
        let end = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{cb};{};{}{end}", col + 1, row + 1));
    }
    let cb = if release { (cb & !3) | 3 } else { cb };
    // Legacy X10 encoding only reaches column/row 94 while staying ASCII.
    if col > 93 || row > 93 || cb > 94 {
        return None;
    }
    let b = |v: u16| char::from(32 + v as u8);
    Some(format!("\x1b[M{}{}{}", b(cb), b(col + 1), b(row + 1)))
}

/// Arrow presses that stand in for wheel scrolling on an alternate screen.
pub fn wheel_as_arrows(up: bool, lines: usize, modes: u32) -> String {
    let one = cursor_key(if up { 'A' } else { 'B' }, KeyModifiers::NONE, modes);
    one.repeat(lines)
}

#[cfg(test)]
#[path = "tests/keys.rs"]
mod tests;
