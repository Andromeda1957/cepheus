//! Kitty's graphics protocol with Unicode placeholders. An image is sent to
//! the terminal once and then drawn by ordinary text cells, so the sidebar's
//! grid, redraws and scrolling keep working, and kitty plays an animation
//! itself with no redraws (and no CPU) from the deck.

use crate::pet::Frame;
use ratatui::style::Color;

/// Base64 bytes per escape sequence, the protocol's limit.
const CHUNK: usize = 4096;
const PLACEHOLDER: char = '\u{10EEEE}';
/// Row and column numbers 0 to 7, from kitty's placeholder diacritic table.
const DIACRITICS: [char; 8] = ['\u{305}', '\u{30D}', '\u{30E}', '\u{310}', '\u{312}', '\u{33D}', '\u{33E}', '\u{33F}'];

/// Kitty draws images; elsewhere (other terminals, tmux) the deck keeps text.
pub fn supported() -> bool {
    std::env::var_os("TMUX").is_none()
        && (std::env::var_os("KITTY_WINDOW_ID").is_some() || std::env::var("TERM").is_ok_and(|t| t == "xterm-kitty"))
}

/// One command, its payload split across as many escapes as it needs. Every
/// part asks for no reply (`q=2`), so nothing arrives as keyboard input.
fn command(out: &mut Vec<u8>, keys: &str, payload: &[u8]) {
    let data = crate::util::base64(payload);
    let mut parts = data.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(part) = parts.next() {
        let more = u8::from(parts.peek().is_some());
        let head = if first { format!("\x1b_G{keys},q=2,m={more};") } else { format!("\x1b_Gm={more},q=2;") };
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(part);
        out.extend_from_slice(b"\x1b\\");
        first = false;
    }
}

fn control(out: &mut Vec<u8>, keys: &str) {
    out.extend_from_slice(format!("\x1b_G{keys},q=2\x1b\\").as_bytes());
}

/// Send frames as image `id`, looping forever when there is more than one.
pub fn animation(id: u32, frames: &[Frame]) -> Vec<u8> {
    let mut out = Vec::new();
    let Some((first, rest)) = frames.split_first() else { return out };
    command(&mut out, &format!("a=t,f=32,i={id},s={},v={}", first.width, first.height), &first.rgba);
    if rest.is_empty() {
        return out;
    }
    control(&mut out, &format!("a=a,i={id},r=1,z={}", first.gap.max(1)));
    for frame in rest {
        let keys = format!("a=f,f=32,i={id},s={},v={},z={}", frame.width, frame.height, frame.gap.max(1));
        command(&mut out, &keys, &frame.rgba);
    }
    control(&mut out, &format!("a=a,i={id},s=3,v=1"));
    out
}

/// Send a PNG as image `id`; kitty decodes it.
pub fn png(id: u32, png: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    command(&mut out, &format!("a=t,f=100,i={id}"), png);
    out
}

/// A virtual placement: image `id` fills any `columns` by `rows` block of
/// placeholder cells drawn in its colour.
pub fn place(id: u32, columns: u16, rows: u16) -> Vec<u8> {
    format!("\x1b_Ga=p,U=1,i={id},c={columns},r={rows},q=2\x1b\\").into_bytes()
}

/// The placeholder text for one row of a placement `columns` wide.
pub fn row(row: usize, columns: usize) -> String {
    (0..columns.min(DIACRITICS.len())).flat_map(|c| [PLACEHOLDER, DIACRITICS[row.min(7)], DIACRITICS[c]]).collect()
}

/// Placeholder cells name their image by foreground colour.
pub fn color(id: u32) -> Color {
    Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
}

#[cfg(test)]
#[path = "tests/kitty.rs"]
mod tests;
