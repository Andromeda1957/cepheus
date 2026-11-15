//! The window every overlay is drawn in.
//!
//! A raised surface inside a faint rounded border; the title on the left of a header row and a short summary on
//! its right; tabs when the window has pages; rows inset behind the selection bar; the window as wide and tall
//! as what it shows, up to the screen, and centred above the footer. Keys are listed by `:help`, never along
//! the window's edge.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::overlays::{row, scrollbar, search_line, width_of, INSET};
use super::{fit, plain, wrap_words};
use crate::theme::Theme;
use crate::tui::DialogViewport;

/// Cells kept clear between a window and the screen's left and right edges.
const EDGE: u16 = 2;
/// The narrowest window's rows.
const NARROWEST: usize = 48;
/// Cells between table columns.
pub const GAP: usize = 3;
/// The cells a table's first column keeps until every other column has
/// given way.
const FIRST_COLUMN: usize = 16;

/// One of a window's pages.
pub struct Tab {
    pub label: String,
    /// Drawn dim after the label, as "Ready 8".
    pub count: String,
}

impl Tab {
    pub fn new(label: impl Into<String>, count: impl Into<String>) -> Tab {
        Tab { label: label.into(), count: count.into() }
    }
}

/// What a window shows. `top` stays put under the header (a table's column
/// titles, the search field), `body` scrolls, and `bottom` stays at the
/// foot (the question being typed).
#[derive(Default)]
pub struct Panel {
    pub title: String,
    pub summary: Vec<Span<'static>>,
    pub tabs: Vec<Tab>,
    pub tab: usize,
    pub top: Vec<Line<'static>>,
    pub body: Vec<Line<'static>>,
    pub bottom: Vec<Line<'static>>,
    /// The body row to keep in view, as a selection moves.
    pub follow: Option<usize>,
    /// The body's first row, when the window keeps its own place.
    pub scroll: Option<usize>,
    /// Body rows kept even when it has fewer lines.
    pub min_body: usize,
    /// The body fills the window from border to border and may reach the top
    /// of the screen and the footer, for a body that is no use cut, such as
    /// a pairing code: the window draws no header or padding rows, so the
    /// body carries its title.
    pub tight: bool,
}

impl Panel {
    pub fn new(title: impl Into<String>) -> Panel {
        Panel { title: title.into(), ..Panel::default() }
    }

    /// The cells the header and tabs need.
    pub fn chrome_width(&self) -> usize {
        let summary = width_of(&self.summary);
        let header = self.title.width() + if summary > 0 { summary + 4 } else { 0 };
        header.max(tabs_width(&self.tabs))
    }
}

/// Where a window was drawn: its whole box and the rows at its foot.
pub struct Placed {
    pub rect: Rect,
    pub bottom: Rect,
}

/// The widest a window's rows may be on this screen.
pub fn widest(area: Rect) -> usize {
    area.width.saturating_sub(2 * EDGE + 2) as usize
}

/// The width a window lays its rows out in for `natural` cells of text:
/// the text and the insets on either side, within the screen.
pub fn line_width(area: Rect, natural: usize) -> usize {
    let most = widest(area);
    (natural + 2 * INSET).max(NARROWEST).min(most)
}

/// The cells of text a row `width` wide holds.
pub fn text_width(width: usize) -> usize {
    width.saturating_sub(2 * INSET)
}

/// The space above the footer, where windows are centred.
fn screen(area: Rect) -> Rect {
    Rect { height: area.height.saturating_sub(1), ..area }
}

fn centre(screen: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(screen.width);
    let height = height.min(screen.height);
    Rect { x: screen.x + (screen.width - width) / 2, y: screen.y + (screen.height - height) / 2, width, height }
}

/// The body rows a window may have on this screen with `fixed` rows of
/// header and foot.
pub fn body_room(area: Rect, fixed: usize) -> usize {
    rows_within(area, fixed, false)
}

/// The body rows inside the window's border, and a clear row above and
/// below it unless the window is `tight`.
fn rows_within(area: Rect, fixed: usize, tight: bool) -> usize {
    let chrome = if tight { 2 } else { 4 };
    (screen(area).height.saturating_sub(chrome) as usize).saturating_sub(fixed).max(1)
}

/// The rows above a window's body: padding, the header, a gap, and the tabs
/// with their gap.
pub fn head_rows(panel: &Panel) -> usize {
    let header = if panel.tight { 0 } else { 3 };
    header + if panel.tabs.is_empty() { 0 } else { 2 } + panel.top.len()
}

/// Draw `panel` with rows `width` wide.
pub fn show(f: &mut Frame, area: Rect, width: usize, panel: Panel, viewport: &mut DialogViewport, t: &Theme) -> Placed {
    let screen = screen(area);
    let mut head = Vec::new();
    if !panel.tight {
        head = vec![
            Line::raw(""),
            spread(vec![title_span(&panel.title, t)], quiet(panel.summary, t), width),
            Line::raw(""),
        ];
    }
    if !panel.tabs.is_empty() {
        head.push(tabs_line(&panel.tabs, panel.tab, width, t));
        head.push(Line::raw(""));
    }
    head.extend(panel.top);
    let foot = panel.bottom.len() + usize::from(!panel.tight);
    let room = rows_within(area, head.len() + foot, panel.tight);
    let body_rows = panel.body.len().max(panel.min_body).clamp(1, room);
    let height = (head.len() + body_rows + foot + 2) as u16;
    let rect = centre(screen, width as u16 + 2, height);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.faint).bg(t.surface))
        .style(Style::default().bg(t.surface));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let head_height = (head.len() as u16).min(inner.height);
    f.render_widget(Paragraph::new(head), Rect { height: head_height, ..inner });
    let body = Rect { y: inner.y + head_height, height: (body_rows as u16).min(inner.height - head_height), ..inner };
    viewport.measure(panel.body.len(), body.height as usize);
    if let Some(scroll) = panel.scroll {
        viewport.offset = scroll.min(viewport.limit);
    } else if let Some(line) = panel.follow {
        viewport.follow(line);
    }
    let (offset, total) = (viewport.offset, panel.body.len());
    let shown: Vec<Line<'static>> = panel.body.into_iter().skip(offset).take(body.height as usize).collect();
    f.render_widget(Paragraph::new(shown), body);
    scrollbar(f, rect, body.y, body.height, total, offset, t);
    let bottom_height = (panel.bottom.len() as u16).min(inner.bottom().saturating_sub(body.bottom()));
    let bottom = Rect { y: body.bottom(), height: bottom_height, ..inner };
    f.render_widget(Paragraph::new(panel.bottom), bottom);
    Placed { rect, bottom }
}

fn title_span(title: &str, t: &Theme) -> Span<'static> {
    Span::styled(title.to_string(), t.bold().fg(t.accent))
}

/// Unstyled spans of a summary or count drawn dim.
pub fn quiet(spans: Vec<Span<'static>>, t: &Theme) -> Vec<Span<'static>> {
    spans.into_iter().map(|s| if s.style == Style::default() { Span::styled(s.content, t.dim()) } else { s }).collect()
}

/// `left` from the inset and `right` ending at the right inset.
pub fn spread(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let room = text_width(width);
    let right = fit(right, room);
    let right_width = width_of(&right);
    let left = fit(left, room.saturating_sub(if right_width > 0 { right_width + 2 } else { 0 }));
    let gap = room.saturating_sub(width_of(&left) + right_width);
    let mut spans = vec![Span::raw(" ".repeat(INSET))];
    spans.extend(left);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Line::from(spans)
}

fn tabs_width(tabs: &[Tab]) -> usize {
    tabs.iter().map(|tab| tab.label.width() + if tab.count.is_empty() { 0 } else { tab.count.width() + 1 } + 3).sum()
}

/// The pages as pills, the open one lit.
fn tabs_line(tabs: &[Tab], open: usize, width: usize, t: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(INSET - 1))];
    for (i, tab) in tabs.iter().enumerate() {
        let lit = i == open;
        let (label, count, pad) = if lit {
            (t.bold().fg(t.accent).bg(t.selected_bg), t.dim().bg(t.selected_bg), plain().bg(t.selected_bg))
        } else {
            (t.dim(), t.faint(), plain())
        };
        spans.push(Span::styled(format!(" {}", tab.label), label));
        if !tab.count.is_empty() {
            spans.push(Span::styled(format!(" {}", tab.count), count));
        }
        spans.push(Span::styled(" ", pad));
        spans.push(Span::raw(" "));
    }
    Line::from(fit(spans, width))
}

/// A group's heading: its name in capitals, `right` (a count) dim beside
/// it, then a faint rule to the right inset.
pub fn section(name: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize, t: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(INSET))];
    spans.extend(name.into_iter().map(|s| Span::styled(s.content.to_uppercase(), s.style.patch(t.bold()))));
    let right = quiet(right, t);
    if !right.is_empty() {
        spans.push(Span::raw("  "));
        spans.extend(right);
    }
    let used = width_of(&spans);
    let end = width.saturating_sub(INSET);
    if used + 3 <= end {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("─".repeat(end - used - 1), t.faint()));
    }
    Line::from(fit(spans, width))
}

/// A dim sentence at the inset, wrapped to the window.
pub fn note(text: &str, width: usize, t: &Theme) -> Vec<Line<'static>> {
    wrap_words(text, text_width(width).max(1))
        .into_iter()
        .map(|line| Line::from(vec![Span::raw(" ".repeat(INSET)), Span::styled(line, t.dim())]))
        .collect()
}

/// A bar `cells` wide, `fill` (0 to 100) of it in `color` over a faint
/// track.
pub fn meter(fill: f64, cells: usize, color: Color, t: &Theme) -> Vec<Span<'static>> {
    let lit = ((fill.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize;
    let mut spans = Vec::new();
    if lit > 0 {
        spans.push(Span::styled("━".repeat(lit), plain().fg(color)));
    }
    if lit < cells {
        spans.push(Span::styled("━".repeat(cells - lit), t.faint()));
    }
    spans
}

/// A faint box from the inset, `width` cells wide with its edges, round
/// the text being typed.
pub fn input_box(rows: Vec<Vec<Span<'static>>>, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2);
    let edge = t.faint();
    let lead = || Span::raw(" ".repeat(INSET));
    let mut lines = vec![Line::from(vec![lead(), Span::styled(format!("╭{}╮", "─".repeat(inner)), edge)])];
    let room = inner.saturating_sub(2);
    for row in rows {
        let row = fit(row, room);
        let pad = room.saturating_sub(width_of(&row));
        let mut spans = vec![lead(), Span::styled("│ ", edge)];
        spans.extend(row);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(" │", edge));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(vec![lead(), Span::styled(format!("╰{}╯", "─".repeat(inner)), edge)]));
    lines
}

/// The search field while `/` search is open, then a gap.
pub fn search_rows(query: &str, count: &str, width: usize, t: &Theme) -> Vec<Line<'static>> {
    vec![search_line(query, count, width, t), Line::raw("")]
}

/// Keep the body lines that hold every searched word; the field to show
/// above them, or nothing outside search.
pub fn search_lines(
    body: &mut Vec<Line<'static>>,
    viewport: &DialogViewport,
    width: usize,
    t: &Theme,
) -> Vec<Line<'static>> {
    let Some(query) = &viewport.search else { return Vec::new() };
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let total = body.len();
    body.retain(|line| {
        let text = line.spans.iter().map(|span| span.content.as_ref()).collect::<String>().to_lowercase();
        !text.trim().is_empty() && words.iter().all(|word| text.contains(word))
    });
    let count = format!("{} of {total}", body.len());
    if body.is_empty() {
        body.extend(note("Nothing matches. Backspace widens the search.", width, t));
    }
    search_rows(query, &count, width, t)
}

/// A figure over its label, as a dashboard's tiles show them.
pub struct Tile {
    pub value: Vec<Span<'static>>,
    pub label: String,
}

fn tile_width(tile: &Tile) -> usize {
    width_of(&tile.value).max(tile.label.width()) + 2
}

/// The cells a row of tiles needs.
pub fn tiles_width(tiles: &[Tile]) -> usize {
    tiles.iter().map(tile_width).sum::<usize>() + 2 * tiles.len().saturating_sub(1)
}

/// Tiles spread evenly across the window, each a faint bar beside its
/// figure in bold over its label; as many rows of them as the width needs.
pub fn tiles(tiles: &[Tile], width: usize, t: &Theme) -> Vec<Line<'static>> {
    let room = text_width(width);
    let mut lines = Vec::new();
    let mut start = 0;
    while start < tiles.len() {
        // As many as fit in this row.
        let mut end = start;
        let mut used = 0;
        while end < tiles.len() {
            let w = tile_width(&tiles[end]) + if end > start { 2 } else { 0 };
            if end > start && used + w > room {
                break;
            }
            used += w;
            end += 1;
        }
        let row_tiles = &tiles[start..end];
        // Even columns when every tile fits one; otherwise each keeps its
        // own width and the spare room is shared, so no label is cut short
        // while the row still has space.
        let even = (room / row_tiles.len()).max(1);
        let needs: Vec<usize> = row_tiles
            .iter()
            .enumerate()
            .map(|(k, tile)| tile_width(tile) + if k + 1 < row_tiles.len() { 2 } else { 0 })
            .collect();
        let columns: Vec<usize> = if needs.iter().all(|&w| w <= even) {
            vec![even; row_tiles.len()]
        } else {
            let spare = room.saturating_sub(needs.iter().sum());
            let n = row_tiles.len();
            needs.iter().enumerate().map(|(k, &w)| (w + spare / n + usize::from(k < spare % n)).min(room)).collect()
        };
        let mut values = vec![Span::raw(" ".repeat(INSET))];
        let mut labels = vec![Span::raw(" ".repeat(INSET))];
        for (tile, &column) in row_tiles.iter().zip(&columns) {
            let value: Vec<Span<'static>> =
                tile.value.iter().map(|s| Span::styled(s.content.clone(), s.style.patch(t.bold()))).collect();
            let value = fit(value, column.saturating_sub(2));
            let value_pad = column.saturating_sub(2 + width_of(&value));
            values.push(Span::styled("▎ ", t.faint()));
            values.extend(value);
            values.push(Span::raw(" ".repeat(value_pad)));
            let label = crate::util::truncate_width(&tile.label, column.saturating_sub(2));
            let label_pad = column.saturating_sub(2 + label.width());
            labels.push(Span::styled("▎ ", t.faint()));
            labels.push(Span::styled(label, t.dim()));
            labels.push(Span::raw(" ".repeat(label_pad)));
        }
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(fit(values, width)));
        lines.push(Line::from(fit(labels, width)));
        start = end;
    }
    lines
}

/// How a column's cells line up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Clone, Debug)]
pub struct Column {
    pub title: String,
    pub align: Align,
}

impl Column {
    pub fn left(title: impl Into<String>) -> Column {
        Column { title: title.into(), align: Align::Left }
    }

    pub fn right(title: impl Into<String>) -> Column {
        Column { title: title.into(), align: Align::Right }
    }
}

pub type Cell = Vec<Span<'static>>;

/// Columns lined up from their widest cell; when the window cannot hold
/// them, the widest text columns give way first.
#[derive(Clone, Debug)]
pub struct Table {
    pub columns: Vec<Column>,
    pub widths: Vec<usize>,
}

impl Table {
    /// Each column's widest cell, or its title.
    pub fn measure<'a>(columns: Vec<Column>, rows: impl IntoIterator<Item = &'a [Cell]>) -> Table {
        let mut widths: Vec<usize> = columns.iter().map(|c| c.title.width()).collect();
        for cells in rows {
            for (i, cell) in cells.iter().enumerate().take(widths.len()) {
                widths[i] = widths[i].max(width_of(cell));
            }
        }
        Table { columns, widths }
    }

    /// The cells the table needs at its natural widths.
    pub fn width(&self) -> usize {
        self.widths.iter().sum::<usize>() + GAP * self.widths.len().saturating_sub(1)
    }

    /// The narrowest column `i` gives way to: its title, or a few cells; the
    /// first column, which names each row, keeps more. Numbers keep theirs.
    fn floor(&self, i: usize, first: bool) -> usize {
        let least = if first && i == 0 { FIRST_COLUMN } else { 6 };
        match self.columns[i].align {
            Align::Right => self.widths[i],
            Align::Left => self.widths[i].min(self.columns[i].title.width().max(least)),
        }
    }

    /// Fit the table in a row `width` wide: columns that cannot fit at their
    /// narrowest go, the last first, then the widest text columns give way,
    /// the first column last.
    pub fn fit(mut self, width: usize) -> Table {
        let room = text_width(width);
        while self.widths.len() > 1 {
            let least: usize =
                (0..self.widths.len()).map(|i| self.floor(i, true)).sum::<usize>() + GAP * (self.widths.len() - 1);
            if least <= room {
                break;
            }
            self.widths.pop();
            self.columns.pop();
        }
        for first in [true, false] {
            self = self.shrink(room, first);
        }
        self
    }

    fn shrink(mut self, room: usize, first: bool) -> Table {
        let mut over = self.width().saturating_sub(room);
        while over > 0 {
            let floor = |i: usize| self.floor(i, first);
            let shrinkable: Vec<usize> = (0..self.widths.len())
                .filter(|&i| self.columns[i].align == Align::Left && self.widths[i] > floor(i))
                .collect();
            let Some(&widest) = shrinkable.iter().max_by_key(|&&i| self.widths[i]) else { break };
            // The widest shrinks to the next widest, so long columns give
            // way together, never below its title.
            let next = shrinkable.iter().filter(|&&i| i != widest).map(|&i| self.widths[i]).max().unwrap_or(0);
            let (w, target) = (self.widths[widest], next.max(floor(widest)));
            let cut = if w > target { (w - target).min(over) } else { 1 };
            self.widths[widest] -= cut;
            over -= cut;
        }
        self
    }

    fn lay(&self, cells: Vec<Cell>) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        let last = self.widths.len().saturating_sub(1);
        for (i, (cell, &w)) in cells.into_iter().zip(&self.widths).enumerate() {
            if i > 0 {
                spans.push(Span::raw(" ".repeat(GAP)));
            }
            let cell = fit(cell, w);
            let pad = w.saturating_sub(width_of(&cell));
            match self.columns[i].align {
                Align::Right => {
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.extend(cell);
                }
                Align::Left => {
                    spans.extend(cell);
                    if i < last {
                        spans.push(Span::raw(" ".repeat(pad)));
                    }
                }
            }
        }
        spans
    }

    /// The column titles, dim capitals over their columns.
    pub fn header(&self, width: usize, t: &Theme) -> Line<'static> {
        let cells = self.columns.iter().map(|c| vec![Span::styled(c.title.to_uppercase(), t.dim())]).collect();
        let mut spans = vec![Span::raw(" ".repeat(INSET))];
        spans.extend(self.lay(cells));
        Line::from(fit(spans, width))
    }

    /// A row whose first cells line up and whose `rest` runs across the
    /// columns after them, such as why a machine has no readings.
    pub fn lead(&self, cells: Vec<Cell>, rest: Vec<Span<'static>>, width: usize, t: &Theme) -> Line<'static> {
        let n = cells.len();
        let mut spans = self.lay(cells);
        let used: usize = self.widths.iter().take(n).sum::<usize>() + GAP * n.saturating_sub(1);
        spans.push(Span::raw(" ".repeat(used.saturating_sub(width_of(&spans)) + GAP)));
        spans.extend(rest);
        row(spans, Vec::new(), width, false, t)
    }

    /// A row of cells; the selected row has the accent bar and its
    /// background.
    pub fn row(&self, cells: Vec<Cell>, width: usize, selected: bool, t: &Theme) -> Line<'static> {
        row(self.lay(cells), Vec::new(), width, selected, t)
    }
}
