//! Text wrapped at word boundaries, as people read it: a line breaks between words, never inside one that fits
//! on a row.

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn push(row: &mut Vec<Span<'static>>, text: &str, style: Style) {
    match row.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push_str(text),
        _ => row.push(Span::styled(text.to_string(), style)),
    }
}

/// `spans` in rows of at most `width` cells. The first row starts with
/// `lead` and the others with `hang`, so list items and quotes keep their
/// indent. Spaces where a row breaks are dropped; a word wider than a whole
/// row breaks between characters.
pub fn spans(
    spans: &[Span<'static>],
    lead: &[Span<'static>],
    hang: &[Span<'static>],
    width: usize,
) -> Vec<Vec<Span<'static>>> {
    let hang_width: usize = hang.iter().map(Span::width).sum();
    let mut rows = Vec::new();
    let mut row = lead.to_vec();
    let mut used: usize = lead.iter().map(Span::width).sum();
    // Whether the row holds any text yet, past its indent.
    let mut filled = false;
    let mut pending: Option<(String, Style)> = None;
    let mut new_row = |row: &mut Vec<Span<'static>>, used: &mut usize, filled: &mut bool| {
        rows.push(std::mem::replace(row, hang.to_vec()));
        *used = hang_width;
        *filled = false;
    };
    for span in spans {
        let mut chars = span.content.char_indices().peekable();
        while let Some((start, first)) = chars.next() {
            let space = first.is_whitespace();
            let mut end = start + first.len_utf8();
            while let Some((at, c)) = chars.peek().copied() {
                if c.is_whitespace() != space {
                    break;
                }
                end = at + c.len_utf8();
                chars.next();
            }
            let text = &span.content[start..end];
            if space {
                match &mut pending {
                    Some((spaces, _)) => spaces.push_str(text),
                    None => pending = Some((text.to_string(), span.style)),
                }
                continue;
            }
            let w = text.width();
            let gap = pending.as_ref().map_or(0, |(spaces, _)| spaces.width());
            if used + gap + w <= width {
                if let Some((spaces, style)) = pending.take() {
                    push(&mut row, &spaces, style);
                }
                push(&mut row, text, span.style);
                used += gap + w;
                filled = true;
                continue;
            }
            let spaces = pending.take();
            if filled && hang_width + w <= width {
                new_row(&mut row, &mut used, &mut filled);
                push(&mut row, text, span.style);
                used += w;
                filled = true;
                continue;
            }
            // Wider than any row: it starts after the spaces where there is room.
            if let Some((spaces, style)) = spaces.filter(|_| filled && used + gap < width) {
                push(&mut row, &spaces, style);
                used += gap;
            }
            for c in text.chars() {
                let cw = c.width().unwrap_or(0);
                if filled && used + cw > width {
                    new_row(&mut row, &mut used, &mut filled);
                }
                push(&mut row, c.encode_utf8(&mut [0; 4]), span.style);
                used += cw;
                filled = true;
            }
        }
    }
    // Trailing spaces stay where they fit, so a composer's cursor follows them.
    if let Some((spaces, style)) = pending.filter(|(spaces, _)| used + spaces.width() <= width) {
        push(&mut row, &spaces, style);
    }
    rows.push(row);
    rows
}

/// Plain `text` wrapped the same way, each row as a string.
pub fn text(text: &str, lead: &str, hang: &str, width: usize) -> Vec<String> {
    let plain = |s: &str| [Span::raw(s.to_string())];
    spans(&plain(text), &plain(lead), &plain(hang), width)
        .into_iter()
        .map(|row| row.iter().map(|span| span.content.as_ref()).collect())
        .collect()
}

#[cfg(test)]
#[path = "tests/wrap.rs"]
mod tests;
