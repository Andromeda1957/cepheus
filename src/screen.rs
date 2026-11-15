//! A serializable snapshot of a hosted terminal screen, built from the
//! emulator in the service and drawn into a ratatui buffer by the TUI.

use std::fmt::Write as _;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

pub const M_APP_CURSOR: u32 = 1;
pub const M_APP_KEYPAD: u32 = 1 << 1;
pub const M_BRACKETED_PASTE: u32 = 1 << 2;
pub const M_MOUSE_CLICK: u32 = 1 << 3;
pub const M_MOUSE_DRAG: u32 = 1 << 4;
pub const M_MOUSE_MOTION: u32 = 1 << 5;
pub const M_SGR_MOUSE: u32 = 1 << 6;
pub const M_ALT_SCREEN: u32 = 1 << 7;
pub const M_ALTERNATE_SCROLL: u32 = 1 << 8;
pub const M_FOCUS: u32 = 1 << 9;
pub const M_UTF8_MOUSE: u32 = 1 << 10;
pub const M_SHOW_CURSOR: u32 = 1 << 11;
pub const M_MOUSE_ANY: u32 = M_MOUSE_CLICK | M_MOUSE_DRAG | M_MOUSE_MOTION;

pub const F_BOLD: u16 = 1;
pub const F_ITALIC: u16 = 1 << 1;
pub const F_UNDERLINE: u16 = 1 << 2;
pub const F_DIM: u16 = 1 << 3;
pub const F_INVERSE: u16 = 1 << 4;
pub const F_HIDDEN: u16 = 1 << 5;
pub const F_STRIKE: u16 = 1 << 6;

pub const C_DEFAULT: u32 = 0;
const C_INDEXED: u32 = 0x0100_0000;
const C_RGB: u32 = 0x0200_0000;

pub const CURSOR_BLOCK: u8 = 0;
pub const CURSOR_UNDERLINE: u8 = 1;
pub const CURSOR_BEAM: u8 = 2;
pub const CURSOR_HOLLOW: u8 = 3;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ScreenFrame {
    /// Source terminal incarnation and parsed-output fence (protocol 18).
    #[serde(default)]
    pub epoch: u64,
    #[serde(default)]
    pub revision: u64,
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Vec<Run>>,
    pub cursor: Option<CursorPos>,
    pub modes: u32,
    /// Lines scrolled back into history (0 = following live output).
    pub offset: u32,
    pub history: u32,
}

pub const HISTORY_PROTOCOL: u32 = 18;
pub const MAX_HISTORY_LINES: usize = 4096;
pub const MAX_HISTORY_BYTES: usize = 2 * 1024 * 1024;

/// One frozen, styled tail. Navigation never changes the service's viewport.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HistorySnapshot {
    pub revision: u64,
    pub lines: Vec<Vec<Run>>,
    /// Full source history size; `lines` may retain only its recent tail.
    pub history: u32,
    pub truncated: bool,
    pub modes: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct CursorPos {
    pub x: u16,
    pub y: u16,
    pub shape: u8,
}

/// Consecutive cells on one line sharing a style and width.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Run {
    pub c: u16,
    pub w: u8,
    pub fg: u32,
    pub bg: u32,
    pub fl: u16,
    pub t: String,
    /// The whole text is one cell (a base character with combining marks).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub g: bool,
}

pub fn indexed(n: u8) -> u32 {
    C_INDEXED | u32::from(n)
}

pub fn rgb(r: u8, g: u8, b: u8) -> u32 {
    C_RGB | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

pub fn encode_color(color: AnsiColor) -> u32 {
    match color {
        AnsiColor::Spec(c) => rgb(c.r, c.g, c.b),
        AnsiColor::Indexed(n) => indexed(n),
        AnsiColor::Named(named) => {
            let i = named as usize;
            if i < 16 {
                return indexed(i as u8);
            }
            match named {
                NamedColor::DimBlack => indexed(0),
                NamedColor::DimRed => indexed(1),
                NamedColor::DimGreen => indexed(2),
                NamedColor::DimYellow => indexed(3),
                NamedColor::DimBlue => indexed(4),
                NamedColor::DimMagenta => indexed(5),
                NamedColor::DimCyan => indexed(6),
                NamedColor::DimWhite => indexed(7),
                _ => C_DEFAULT,
            }
        }
    }
}

/// Encode a cell colour, using the RGB value when the hosted program has
/// redefined that palette entry (OSC 4, 10 or 11). Untouched entries stay
/// palette references so the outer terminal draws them in its own theme.
pub fn resolve_color(color: AnsiColor, colors: &Colors) -> u32 {
    let index = match color {
        AnsiColor::Spec(c) => return rgb(c.r, c.g, c.b),
        AnsiColor::Indexed(n) => usize::from(n),
        AnsiColor::Named(named) => named as usize,
    };
    match colors[index] {
        Some(c) => rgb(c.r, c.g, c.b),
        None => encode_color(color),
    }
}

pub fn decode_color(c: u32) -> Color {
    if c & C_RGB != 0 {
        Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
    } else if c & C_INDEXED != 0 {
        Color::Indexed(c as u8)
    } else {
        Color::Reset
    }
}

fn encode_flags(flags: Flags) -> u16 {
    let mut out = 0;
    if flags.contains(Flags::BOLD) {
        out |= F_BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        out |= F_ITALIC;
    }
    if flags.intersects(
        Flags::UNDERLINE
            | Flags::DOUBLE_UNDERLINE
            | Flags::UNDERCURL
            | Flags::DOTTED_UNDERLINE
            | Flags::DASHED_UNDERLINE,
    ) {
        out |= F_UNDERLINE;
    }
    if flags.contains(Flags::DIM) {
        out |= F_DIM;
    }
    if flags.contains(Flags::INVERSE) {
        out |= F_INVERSE;
    }
    if flags.contains(Flags::HIDDEN) {
        out |= F_HIDDEN;
    }
    if flags.contains(Flags::STRIKEOUT) {
        out |= F_STRIKE;
    }
    out
}

pub fn encode_modes(mode: TermMode) -> u32 {
    let pairs = [
        (TermMode::APP_CURSOR, M_APP_CURSOR),
        (TermMode::APP_KEYPAD, M_APP_KEYPAD),
        (TermMode::BRACKETED_PASTE, M_BRACKETED_PASTE),
        (TermMode::MOUSE_REPORT_CLICK, M_MOUSE_CLICK),
        (TermMode::MOUSE_DRAG, M_MOUSE_DRAG),
        (TermMode::MOUSE_MOTION, M_MOUSE_MOTION),
        (TermMode::SGR_MOUSE, M_SGR_MOUSE),
        (TermMode::ALT_SCREEN, M_ALT_SCREEN),
        (TermMode::ALTERNATE_SCROLL, M_ALTERNATE_SCROLL),
        (TermMode::FOCUS_IN_OUT, M_FOCUS),
        (TermMode::UTF8_MOUSE, M_UTF8_MOUSE),
        (TermMode::SHOW_CURSOR, M_SHOW_CURSOR),
    ];
    pairs.iter().filter(|(m, _)| mode.contains(*m)).fold(0, |acc, (_, bit)| acc | bit)
}

/// The character a cell shows. The emulator keeps a `'\t'` in the cell a tab started from, so copied text keeps
/// the tab, but the tab only moved the cursor: written out as text it moves the real terminal's cursor to that
/// terminal's own tab stop, and the rest of the line lands in the wrong columns.
fn shown(cell: &Cell) -> char {
    match cell.c {
        '\0' | '\t' => ' ',
        c => c,
    }
}

struct LineBuilder {
    runs: Vec<Run>,
}

impl LineBuilder {
    fn push(&mut self, col: u16, width: u8, fg: u32, bg: u32, fl: u16, cell: &Cell) {
        let zerowidth = cell.zerowidth().filter(|z| !z.is_empty());
        let ch = shown(cell);
        if let Some(extra) = zerowidth {
            let mut t = String::new();
            t.push(ch);
            t.extend(extra.iter());
            self.runs.push(Run { c: col, w: width, fg, bg, fl, t, g: true });
            return;
        }
        if let Some(last) = self.runs.last_mut() {
            if !last.g
                && last.fg == fg
                && last.bg == bg
                && last.fl == fl
                && last.w == width
                && u32::from(last.c) + last.t.chars().count() as u32 * u32::from(width) == u32::from(col)
            {
                last.t.push(ch);
                return;
            }
        }
        self.runs.push(Run { c: col, w: width, fg, bg, fl, t: ch.to_string(), g: false });
    }
}

/// Snapshot the visible viewport of `term`.
pub fn build_frame<T: EventListener>(term: &Term<T>) -> ScreenFrame {
    let cols = term.columns();
    let rows = term.screen_lines();
    let colors = term.colors();
    let content = term.renderable_content();
    let offset = content.display_offset;
    let mut lines: Vec<LineBuilder> = (0..rows).map(|_| LineBuilder { runs: Vec::new() }).collect();
    for indexed in content.display_iter {
        let cell = indexed.cell;
        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let y = indexed.point.line.0 + offset as i32;
        if y < 0 || y as usize >= rows {
            continue;
        }
        let x = indexed.point.column.0;
        if x >= cols {
            continue;
        }
        let fg = resolve_color(cell.fg, colors);
        let bg = resolve_color(cell.bg, colors);
        let fl = encode_flags(cell.flags);
        let blank = shown(cell) == ' '
            && bg == C_DEFAULT
            && fl & (F_INVERSE | F_UNDERLINE | F_STRIKE) == 0
            && cell.zerowidth().is_none_or(|z| z.is_empty());
        if blank {
            continue;
        }
        let width = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
        lines[y as usize].push(x as u16, width, fg, bg, fl, cell);
    }
    let cursor = {
        let point = content.cursor.point;
        let y = point.line.0 + offset as i32;
        let shape = match content.cursor.shape {
            CursorShape::Hidden => None,
            CursorShape::Block => Some(CURSOR_BLOCK),
            CursorShape::Underline => Some(CURSOR_UNDERLINE),
            CursorShape::Beam => Some(CURSOR_BEAM),
            CursorShape::HollowBlock => Some(CURSOR_HOLLOW),
        };
        match shape {
            Some(shape) if y >= 0 && (y as usize) < rows && point.column.0 < cols => {
                Some(CursorPos { x: point.column.0 as u16, y: y as u16, shape })
            }
            _ => None,
        }
    };
    ScreenFrame {
        epoch: 0,
        revision: 0,
        cols: cols as u16,
        rows: rows as u16,
        lines: lines.into_iter().map(|l| l.runs).collect(),
        cursor,
        modes: encode_modes(*term.mode()),
        offset: offset as u32,
        history: term.grid().history_size() as u32,
    }
}

/// Snapshot physical grid rows, preserving wide cells, combining marks and colours.
fn history_line<T: EventListener>(term: &Term<T>, y: i32, text_limit: usize) -> Option<Vec<Run>> {
    let mut line = LineBuilder { runs: Vec::new() };
    let mut text_bytes = 0usize;
    for x in 0..term.columns() {
        let cell = &term.grid()[Line(y)][Column(x)];
        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let extra = cell.zerowidth();
        text_bytes = text_bytes.saturating_add(shown(cell).len_utf8());
        if let Some(extra) = extra {
            text_bytes = text_bytes.saturating_add(extra.iter().map(|c| c.len_utf8()).sum::<usize>());
        }
        if text_bytes > text_limit {
            return None;
        }
        let bg = resolve_color(cell.bg, term.colors());
        let fl = encode_flags(cell.flags);
        if shown(cell) == ' '
            && bg == C_DEFAULT
            && fl & (F_INVERSE | F_UNDERLINE | F_STRIKE) == 0
            && extra.is_none_or(|z| z.is_empty())
        {
            continue;
        }
        line.push(
            x as u16,
            if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 },
            resolve_color(cell.fg, term.colors()),
            bg,
            fl,
            cell,
        );
    }
    Some(line.runs)
}

/// A recent contiguous tail, capped in both rows and serialized bytes. It is
/// produced once on demand, never for every frame or navigation key.
pub fn build_history<T: EventListener>(term: &Term<T>, revision: u64) -> HistorySnapshot {
    let total = term.screen_lines() + term.grid().history_size();
    let count = total.min(MAX_HISTORY_LINES);
    let mut lines = Vec::new();
    let mut bytes = 1024usize; // Envelope and metadata reserve.
    for y in (term.screen_lines() as i32 - count as i32..term.screen_lines() as i32).rev() {
        let Some(line) = history_line(term, y, MAX_HISTORY_BYTES / 2) else { break };
        let size = serde_json::to_vec(&line).map_or(MAX_HISTORY_BYTES, |b| b.len() + 1);
        if bytes.saturating_add(size) > MAX_HISTORY_BYTES {
            break;
        }
        bytes += size;
        lines.push(line);
    }
    lines.reverse();
    HistorySnapshot {
        revision,
        truncated: lines.len() < total,
        lines,
        history: term.grid().history_size() as u32,
        modes: encode_modes(*term.mode()),
    }
}

/// Current output independently of legacy clients' shared scroll position.
pub fn build_live_frame<T: EventListener>(term: &Term<T>) -> ScreenFrame {
    let mut frame = build_frame(term);
    if frame.offset != 0 {
        frame.lines =
            (0..term.screen_lines()).map(|y| history_line(term, y as i32, usize::MAX).unwrap_or_default()).collect();
        frame.offset = 0;
        let cursor = term.grid().cursor.point;
        frame.cursor = (term.mode().contains(TermMode::SHOW_CURSOR)
            && term.renderable_content().cursor.shape != CursorShape::Hidden)
            .then_some(CursorPos {
                x: cursor.column.0 as u16,
                y: cursor.line.0.max(0) as u16,
                shape: match term.renderable_content().cursor.shape {
                    CursorShape::Underline => CURSOR_UNDERLINE,
                    CursorShape::Beam => CURSOR_BEAM,
                    CursorShape::HollowBlock => CURSOR_HOLLOW,
                    _ => CURSOR_BLOCK,
                },
            });
    }
    frame
}

/// Plain text of the live screen (ignoring any scrollback offset).
pub fn screen_text<T: EventListener>(term: &Term<T>) -> Vec<String> {
    let cols = term.columns();
    let grid = term.grid();
    (0..term.screen_lines())
        .map(|y| {
            let row = &grid[Line(y as i32)];
            let mut s = String::with_capacity(cols);
            for x in 0..cols {
                let cell = &row[Column(x)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                s.push(shown(cell));
            }
            s.trim_end().to_string()
        })
        .collect()
}

/// A bounded tail of history and live rows, independent of the user's viewport.
/// Physical rows retain indentation so provider adapters can distinguish chrome.
pub fn tail_text<T: EventListener>(term: &Term<T>, max: usize) -> Vec<String> {
    let max = max.clamp(1, 512);
    let rows = term.screen_lines();
    let start = rows as i32 - (rows + term.grid().history_size()).min(max) as i32;
    (start..rows as i32)
        .map(|y| {
            let row = &term.grid()[Line(y)];
            let mut text = String::new();
            for x in 0..term.columns() {
                let cell = &row[Column(x)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                text.push(shown(cell));
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra);
                }
            }
            text.trim_end().to_string()
        })
        .collect()
}

/// Where the program shows its cursor on the live screen, as (character
/// index, row) in `screen_text`'s lines; `None` while it hides the cursor.
pub fn text_cursor<T: EventListener>(term: &Term<T>) -> Option<(usize, usize)> {
    if !term.mode().contains(TermMode::SHOW_CURSOR) {
        return None;
    }
    let point = term.grid().cursor.point;
    let y = usize::try_from(point.line.0).ok().filter(|y| *y < term.screen_lines())?;
    let row = &term.grid()[point.line];
    let x = (0..point.column.0.min(term.columns()))
        .filter(|&x| !row[Column(x)].flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER))
        .count();
    Some((x, y))
}

/// How a cell is drawn, as far as SGR sets it; replay emits SGR only when it
/// changes.
#[derive(Clone, Debug, PartialEq)]
struct Pen {
    fg: AnsiColor,
    bg: AnsiColor,
    flags: Flags,
    underline: Option<AnsiColor>,
}

impl Pen {
    fn of(cell: &Cell) -> Pen {
        let drawn = Flags::BOLD
            | Flags::DIM
            | Flags::ITALIC
            | Flags::ALL_UNDERLINES
            | Flags::INVERSE
            | Flags::HIDDEN
            | Flags::STRIKEOUT;
        Pen { fg: cell.fg, bg: cell.bg, flags: cell.flags & drawn, underline: cell.underline_color() }
    }

    fn plain() -> Pen {
        Pen::of(&Cell::default())
    }

    fn sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        let f = self.flags;
        for (flag, code) in [(Flags::BOLD, "1"), (Flags::DIM, "2"), (Flags::ITALIC, "3")] {
            if f.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        let underline = [
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "4:2"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
        ];
        if let Some((_, code)) = underline.iter().find(|(flag, _)| f.contains(*flag)) {
            out.push(';');
            out.push_str(code);
        }
        for (flag, code) in [(Flags::INVERSE, "7"), (Flags::HIDDEN, "8"), (Flags::STRIKEOUT, "9")] {
            if f.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        sgr_color(out, self.fg, 30, 90, 38, 39);
        sgr_color(out, self.bg, 40, 100, 48, 49);
        if let Some(color) = self.underline {
            sgr_color(out, color, 0, 0, 58, 59);
        }
        out.push('m');
    }
}

/// Append one colour to an SGR sequence: `base`/`bright` for the 16 named
/// colours (0 to use the 256-colour form), `ext` for indexed and RGB colours,
/// `default` for the terminal's own.
fn sgr_color(out: &mut String, color: AnsiColor, base: usize, bright: usize, ext: usize, default: usize) {
    let named = match color {
        AnsiColor::Spec(c) => {
            let _ = write!(out, ";{ext};2;{};{};{}", c.r, c.g, c.b);
            return;
        }
        AnsiColor::Indexed(n) => {
            let _ = write!(out, ";{ext};5;{n}");
            return;
        }
        AnsiColor::Named(named) => match named as usize {
            i if i < 16 => i,
            i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
                i - NamedColor::DimBlack as usize
            }
            _ => {
                let _ = write!(out, ";{default}");
                return;
            }
        },
    };
    let _ = match named {
        i if base == 0 => write!(out, ";{ext};5;{i}"),
        i if i < 8 => write!(out, ";{}", base + i),
        i => write!(out, ";{}", bright + i - 8),
    };
}

/// Whether a cell shows nothing, so a line can end before it.
fn blank(cell: &Cell) -> bool {
    shown(cell) == ' '
        && cell.bg == AnsiColor::Named(NamedColor::Background)
        && !cell.flags.intersects(Flags::ALL_UNDERLINES | Flags::INVERSE | Flags::STRIKEOUT)
        && cell.zerowidth().is_none_or(|z| z.is_empty())
        && cell.hyperlink().is_none()
}

/// Escape sequences that rebuild `term`'s screen in a new emulator of the
/// same size and scrollback: every line of history and screen with its
/// colours, attributes and links, then the palette, modes, cursor shape and
/// cursor position. The service hands this to its next build so a session's
/// screen survives an update (`handover.rs`).
///
/// While a program uses the alternate screen, only that screen is rebuilt:
/// the emulator does not expose the normal screen kept behind it, so the
/// program finds it blank when it leaves the alternate screen.
pub fn replay<T: EventListener>(term: &Term<T>) -> String {
    let grid = term.grid();
    let cols = grid.columns();
    let mode = *term.mode();
    let mut out = String::new();
    if mode.contains(TermMode::ALT_SCREEN) {
        out.push_str("\x1b[?1049h");
    }
    out.push_str("\x1b[0m");
    let mut pen = Pen::plain();
    let mut link: Option<Hyperlink> = None;
    let top = grid.topmost_line().0;
    let bottom = grid.bottommost_line().0;
    let mut continued = false;
    for y in top..=bottom {
        let row = &grid[Line(y)];
        let wrapped = cols > 0 && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        let mut end =
            if wrapped { cols } else { (0..cols).rev().find(|&x| !blank(&row[Column(x)])).map_or(0, |x| x + 1) };
        // A wrapped line moves on only when the next character arrives, so
        // the line after one always writes at least one cell.
        if continued {
            end = end.max(1);
        }
        for x in 0..end {
            let cell = &row[Column(x)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let cell_pen = Pen::of(cell);
            if cell_pen != pen {
                cell_pen.sgr(&mut out);
                pen = cell_pen;
            }
            let cell_link = cell.hyperlink();
            if cell_link != link {
                match &cell_link {
                    Some(l) => {
                        let _ = write!(out, "\x1b]8;id={};{}\x1b\\", l.id(), l.uri());
                    }
                    None => out.push_str("\x1b]8;;\x1b\\"),
                }
                link = cell_link;
            }
            out.push(shown(cell));
            if let Some(extra) = cell.zerowidth() {
                out.extend(extra.iter());
            }
        }
        continued = wrapped;
        if y < bottom && !wrapped {
            // Reset first so the new line is not filled with a background.
            if pen != Pen::plain() {
                out.push_str("\x1b[0m");
                pen = Pen::plain();
            }
            out.push_str("\r\n");
        }
    }
    if link.is_some() {
        out.push_str("\x1b]8;;\x1b\\");
    }
    out.push_str("\x1b[0m");

    let colors = term.colors();
    for i in 0..259 {
        if let Some(c) = colors[i] {
            let spec = format!("rgb:{:02x}/{:02x}/{:02x}", c.r, c.g, c.b);
            let _ = match i {
                256 => write!(out, "\x1b]10;{spec}\x1b\\"),
                257 => write!(out, "\x1b]11;{spec}\x1b\\"),
                258 => write!(out, "\x1b]12;{spec}\x1b\\"),
                _ => write!(out, "\x1b]4;{i};{spec}\x1b\\"),
            };
        }
    }

    let set = |out: &mut String, on: bool, code: u32| {
        let _ = write!(out, "\x1b[?{code}{}", if on { 'h' } else { 'l' });
    };
    set(&mut out, mode.contains(TermMode::APP_CURSOR), 1);
    set(&mut out, mode.contains(TermMode::BRACKETED_PASTE), 2004);
    if mode.contains(TermMode::MOUSE_MOTION) {
        set(&mut out, true, 1003);
    } else if mode.contains(TermMode::MOUSE_DRAG) {
        set(&mut out, true, 1002);
    } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
        set(&mut out, true, 1000);
    }
    set(&mut out, mode.contains(TermMode::FOCUS_IN_OUT), 1004);
    set(&mut out, mode.contains(TermMode::UTF8_MOUSE), 1005);
    set(&mut out, mode.contains(TermMode::SGR_MOUSE), 1006);
    set(&mut out, mode.contains(TermMode::ALTERNATE_SCROLL), 1007);
    set(&mut out, mode.contains(TermMode::SHOW_CURSOR), 25);
    set(&mut out, mode.contains(TermMode::LINE_WRAP), 7);
    if mode.contains(TermMode::INSERT) {
        out.push_str("\x1b[4h");
    }
    if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
        out.push_str("\x1b[20h");
    }
    out.push_str(if mode.contains(TermMode::APP_KEYPAD) { "\x1b=" } else { "\x1b>" });

    let style = term.cursor_style();
    let shape = match style.shape {
        CursorShape::Block => Some(1),
        CursorShape::Underline => Some(3),
        CursorShape::Beam => Some(5),
        CursorShape::HollowBlock | CursorShape::Hidden => None,
    };
    if let Some(blinking) = shape {
        let _ = write!(out, "\x1b[{} q", if style.blinking { blinking } else { blinking + 1 });
    }

    let cursor = &grid.cursor;
    let _ = write!(out, "\x1b[{};{}H", cursor.point.line.0 + 1, cursor.point.column.0 + 1);
    let template = Pen::of(&cursor.template);
    if template != Pen::plain() {
        template.sgr(&mut out);
    }
    out
}

pub fn style_of(fg: u32, bg: u32, fl: u16) -> Style {
    let mut style = Style::default().fg(decode_color(fg)).bg(decode_color(bg));
    let mut m = Modifier::empty();
    if fl & F_BOLD != 0 {
        m |= Modifier::BOLD;
    }
    if fl & F_ITALIC != 0 {
        m |= Modifier::ITALIC;
    }
    if fl & F_UNDERLINE != 0 {
        m |= Modifier::UNDERLINED;
    }
    if fl & F_DIM != 0 {
        m |= Modifier::DIM;
    }
    if fl & F_INVERSE != 0 {
        m |= Modifier::REVERSED;
    }
    if fl & F_HIDDEN != 0 {
        m |= Modifier::HIDDEN;
    }
    if fl & F_STRIKE != 0 {
        m |= Modifier::CROSSED_OUT;
    }
    style = style.add_modifier(m);
    style
}

fn put(buf: &mut Buffer, area: Rect, col: u16, y: u16, symbol: &str, width: u8, style: Style) {
    if col >= area.width || (width == 2 && col + 1 >= area.width) {
        return;
    }
    let x = area.x + col;
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(symbol);
        cell.set_style(style);
    }
    if width == 2 {
        if let Some(cell) = buf.cell_mut((x + 1, y)) {
            cell.reset();
            cell.set_style(style);
        }
    }
}

/// Draw a frame into `area` of `buf`, clearing what was there.
pub fn render_frame(frame: &ScreenFrame, area: Rect, buf: &mut Buffer) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
    let mut tmp = [0u8; 4];
    for (row, runs) in frame.lines.iter().enumerate() {
        if row >= area.height as usize {
            break;
        }
        let y = area.y + row as u16;
        for run in runs {
            let style = style_of(run.fg, run.bg, run.fl);
            if run.g {
                put(buf, area, run.c, y, &run.t, run.w, style);
                continue;
            }
            let mut col = run.c;
            for ch in run.t.chars() {
                put(buf, area, col, y, ch.encode_utf8(&mut tmp), run.w, style);
                col = col.saturating_add(u16::from(run.w));
            }
        }
    }
}

/// Plain text of a frame, one string per row.
#[cfg(test)]
pub fn frame_text(frame: &ScreenFrame) -> Vec<String> {
    frame
        .lines
        .iter()
        .map(|runs| {
            let mut cells: Vec<String> = vec![" ".to_string(); frame.cols as usize];
            for run in runs {
                if run.g {
                    if let Some(slot) = cells.get_mut(run.c as usize) {
                        *slot = run.t.clone();
                    }
                    continue;
                }
                let mut col = run.c as usize;
                for ch in run.t.chars() {
                    if let Some(slot) = cells.get_mut(col) {
                        *slot = ch.to_string();
                    }
                    if run.w == 2 {
                        if let Some(slot) = cells.get_mut(col + 1) {
                            slot.clear();
                        }
                    }
                    col += run.w as usize;
                }
            }
            cells.concat().trim_end().to_string()
        })
        .collect()
}

#[cfg(test)]
#[path = "tests/screen.rs"]
mod tests;
