//! Client-side selection of the visible hosted screen, including unsent drafts.

use crossterm::event::MouseEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthStr;

use crate::screen::{self, ScreenFrame};
use crate::theme::Theme;

pub struct TextSelection {
    pub frame: ScreenFrame,
    pub press: MouseEvent,
    pub pressed: bool,
    pub dragged: bool,
    cells: Buffer,
    anchor: (u16, u16),
    end: (u16, u16),
}

impl TextSelection {
    pub fn new(frame: ScreenFrame, pane: Rect, press: MouseEvent) -> Self {
        let area = Rect::new(0, 0, pane.width, pane.height);
        let mut cells = Buffer::empty(area);
        screen::render_frame(&frame, area, &mut cells);
        let anchor = (press.row - pane.y, press.column - pane.x);
        Self { frame, press, pressed: true, dragged: false, cells, anchor, end: anchor }
    }

    pub fn update(&mut self, pane: Rect, event: MouseEvent) {
        let row = event.row.saturating_sub(pane.y).min(self.cells.area.height.saturating_sub(1));
        let col = event.column.saturating_sub(pane.x).min(self.cells.area.width.saturating_sub(1));
        self.end = (row, col);
        self.dragged |= self.end != self.anchor;
    }

    fn bounds(&self) -> ((u16, u16), (u16, u16)) {
        (self.anchor.min(self.end), self.anchor.max(self.end))
    }

    /// Visit whole displayed glyphs so a wide character or combining mark is
    /// never split and its continuation cell never turns into an extra space.
    fn visit(&self, mut glyph: impl FnMut(u16, u16, u16, &str)) {
        let ((y0, x0), (y1, x1)) = self.bounds();
        for y in y0..=y1 {
            let left = if y == y0 { x0 } else { 0 };
            let right = if y == y1 { x1 } else { self.cells.area.width.saturating_sub(1) };
            let mut x = 0;
            while x < self.cells.area.width {
                let text = self.cells[(x, y)].symbol();
                let width = (text.width().max(1) as u16).min(self.cells.area.width - x);
                if x <= right && x + width > left {
                    glyph(x, y, width, text);
                }
                x += width;
            }
        }
    }

    pub fn text(&self) -> String {
        let ((y0, _), (y1, _)) = self.bounds();
        let mut rows = vec![String::new(); usize::from(y1 - y0) + 1];
        self.visit(|_, y, _, text| rows[usize::from(y - y0)].push_str(text));
        rows.iter().map(|row| row.trim_end()).collect::<Vec<_>>().join("\n")
    }

    pub fn highlight(&self, pane: Rect, buffer: &mut Buffer, theme: &Theme) {
        if !self.dragged {
            return;
        }
        self.visit(|x, y, width, _| {
            for col in x..x + width {
                if let Some(cell) = buffer.cell_mut((pane.x + col, pane.y + y)) {
                    cell.set_fg(theme.on_color).set_bg(theme.accent);
                    cell.modifier.remove(Modifier::REVERSED);
                }
            }
        });
    }

    /// Keep the highlight through cursor blinks and changes elsewhere, but
    /// never leave it pointing at different text after the screen scrolls.
    pub fn matches(&self, frame: &ScreenFrame) -> bool {
        let ((y0, _), (y1, _)) = self.bounds();
        frame.cols == self.frame.cols
            && frame.rows == self.frame.rows
            && frame.offset == self.frame.offset
            && (y0..=y1).all(|row| frame.lines.get(row as usize) == self.frame.lines.get(row as usize))
    }
}

#[cfg(test)]
#[path = "tests/selection.rs"]
mod tests;
