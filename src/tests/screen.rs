use super::*;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

fn term(cols: usize, rows: usize, input: &str) -> Term<VoidListener> {
    let config = Config { scrolling_history: 100, ..Config::default() };
    let mut term = Term::new(config, &TermSize::new(cols, rows), VoidListener);
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    parser.advance(&mut term, input.as_bytes());
    term
}

#[test]
fn frame_captures_text_colours_and_cursor() {
    let t = term(20, 4, "plain \x1b[1;31mred\x1b[0m\r\n\x1b[38;2;1;2;3;48;5;200mx\x1b[0m");
    let frame = build_frame(&t);
    assert_eq!((frame.cols, frame.rows), (20, 4));
    assert_eq!(frame_text(&frame), vec!["plain red", "x", "", ""]);
    let red = frame.lines[0].iter().find(|r| r.t == "red").unwrap();
    assert_eq!((red.c, red.fg, red.fl), (6, indexed(1), F_BOLD));
    let x = &frame.lines[1][0];
    assert_eq!((x.fg, x.bg), (rgb(1, 2, 3), indexed(200)));
    assert_eq!(frame.cursor, Some(CursorPos { x: 1, y: 1, shape: CURSOR_BLOCK }));
}

#[test]
fn blank_default_cells_are_skipped_but_backgrounds_kept() {
    let t = term(10, 2, "a    b\r\n\x1b[44m   \x1b[0m");
    let frame = build_frame(&t);
    let row0: Vec<(u16, &str)> = frame.lines[0].iter().map(|r| (r.c, r.t.as_str())).collect();
    assert_eq!(row0, vec![(0, "a"), (5, "b")]);
    assert_eq!(frame.lines[1].len(), 1);
    assert_eq!(frame.lines[1][0].t, "   ");
    assert_eq!(frame.lines[1][0].bg, indexed(4));
}

#[test]
fn wide_and_combining_characters_occupy_correct_cells() {
    let t = term(10, 1, "日x e\u{301}");
    let frame = build_frame(&t);
    assert_eq!(frame_text(&frame), vec!["日x e\u{301}"]);
    let wide = &frame.lines[0][0];
    assert_eq!((wide.c, wide.w, wide.t.as_str()), (0, 2, "日"));
    let x = frame.lines[0].iter().find(|r| r.t == "x").unwrap();
    assert_eq!(x.c, 2);
    let accent = frame.lines[0].iter().find(|r| r.g).unwrap();
    assert_eq!((accent.c, accent.t.as_str()), (4, "e\u{301}"));
}

#[test]
fn modes_and_hidden_cursor_are_reported() {
    let t = term(10, 2, "\x1b[?2004h\x1b[?1006h\x1b[?1002h\x1b[?1h\x1b[?25l");
    let frame = build_frame(&t);
    assert_ne!(frame.modes & M_BRACKETED_PASTE, 0);
    assert_ne!(frame.modes & M_SGR_MOUSE, 0);
    assert_ne!(frame.modes & M_MOUSE_DRAG, 0);
    assert_ne!(frame.modes & M_APP_CURSOR, 0);
    assert_eq!(frame.modes & M_SHOW_CURSOR, 0);
    assert_eq!(frame.cursor, None);
}

#[test]
fn scrolled_frame_shows_history_and_offset() {
    let mut text = String::new();
    for i in 0..10 {
        text.push_str(&format!("line{i}\r\n"));
    }
    // Ten lines and the empty cursor line: three on screen, eight in history.
    let mut t = term(10, 3, &text);
    assert_eq!(screen_text(&t), vec!["line8", "line9", ""]);
    t.scroll_display(Scroll::Delta(2));
    let frame = build_frame(&t);
    assert_eq!(frame.offset, 2);
    assert_eq!(frame.history, 8);
    assert_eq!(frame_text(&frame), vec!["line6", "line7", "line8"]);
    assert_eq!(frame.cursor, None, "the cursor row is scrolled out of view");
    // The live text ignores the scrollback position.
    assert_eq!(screen_text(&t), vec!["line8", "line9", ""]);
}

#[test]
fn history_snapshot_preserves_styles_wide_and_combining_cells_without_scrolling() {
    let mut t = term(24, 4, &("\x1b[1;31mred\x1b[0m 日e\u{301}\r\n".repeat(80)));
    t.scroll_display(Scroll::Delta(12));
    let before = build_frame(&t);
    let history = build_history(&t, 77);
    assert_eq!(build_frame(&t), before, "history reads never move the source viewport");
    assert_eq!(history.revision, 77);
    let line = history.lines.iter().find(|line| line.iter().any(|r| r.t == "red")).unwrap();
    let red = line.iter().find(|r| r.t == "red").unwrap();
    assert_eq!((red.fg, red.fl), (indexed(1), F_BOLD));
    assert!(line.iter().any(|r| r.w == 2 && r.t == "日"));
    assert!(line.iter().any(|r| r.g && r.t == "e\u{301}"));
    assert_eq!(build_live_frame(&t).offset, 0);
    assert_eq!(frame_text(&build_live_frame(&t)), ["red 日e\u{301}", "red 日e\u{301}", "red 日e\u{301}", ""]);
}

#[test]
fn history_snapshot_retains_a_contiguous_tail_inside_wire_and_row_limits() {
    let mut t = Term::new(Config { scrolling_history: 6000, ..Config::default() }, &TermSize::new(80, 4), VoidListener);
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    let row = "\x1b[31mx\x1b[32my".repeat(40) + "\r\n";
    parser.advance(&mut t, row.repeat(2000).as_bytes());
    let history = build_history(&t, 5);
    assert!(history.truncated);
    assert!(history.lines.len() < 2000 && history.lines.len() <= MAX_HISTORY_LINES);
    assert!(serde_json::to_vec(&history).unwrap().len() <= MAX_HISTORY_BYTES);
    let live = build_live_frame(&t);
    assert_eq!(&history.lines[history.lines.len() - 4..], live.lines.as_slice());
}

#[test]
fn render_frame_draws_into_area_and_clears_old_content() {
    let t = term(6, 2, "ab\x1b[7mc\x1b[0m\r\n日");
    let frame = build_frame(&t);
    let area = Rect { x: 2, y: 1, width: 6, height: 2 };
    let mut buf = Buffer::filled(Rect { x: 0, y: 0, width: 10, height: 4 }, ratatui::buffer::Cell::new("#"));
    render_frame(&frame, area, &mut buf);
    let row = |y: u16| (0..10).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>();
    assert_eq!(row(0), "##########", "outside the area is untouched");
    assert_eq!(row(1), "##abc   ##");
    assert!(buf[(4, 1)].modifier.contains(Modifier::REVERSED));
    assert_eq!(buf[(2, 2)].symbol(), "日");
    assert_eq!(row(3), "##########");
}

/// What a real terminal shows after the deck draws `frames` one after another
/// into `area` of a `width`×`height` screen: each frame is diffed against the
/// last as ratatui's `Terminal::draw` does, written out by crossterm, and read
/// by a second emulator standing in for the owner's terminal.
fn shown_on_terminal(frames: &[ScreenFrame], area: Rect, width: u16, height: u16) -> Vec<Vec<String>> {
    use ratatui::backend::{Backend, CrosstermBackend};
    let full = Rect { x: 0, y: 0, width, height };
    let mut outer = term(width as usize, height as usize, "");
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    let mut previous = Buffer::empty(full);
    let mut shown = Vec::new();
    for frame in frames {
        let mut next = Buffer::empty(full);
        render_frame(frame, area, &mut next);
        let mut bytes = Vec::new();
        CrosstermBackend::new(&mut bytes).draw(previous.diff(&next).into_iter()).unwrap();
        parser.advance(&mut outer, &bytes);
        shown.push(screen_text(&outer));
        previous = next;
    }
    shown
}

/// A program's tab leaves a `'\t'` in the emulator cell it started from;
/// drawn as text it moved the real terminal's cursor to its own tab stop, so
/// the rest of the line went to the wrong columns and stayed behind when the
/// program cleared it. Owner, 2026-10-31, with a screenshot of letters left
/// in Antigravity's pane: "AGY / usage screen leaves letters on the screen".
/// Antigravity's slash-command menu, which `/usage` opens, pads with tabs.
#[test]
fn a_programs_tabs_leave_nothing_behind_on_the_real_terminal() {
    // The pane starts at a column that is not a tab stop, as the deck's does.
    let area = Rect { x: 3, y: 0, width: 30, height: 2 };
    let frames: Vec<ScreenFrame> = ["abcdefghijklmnopqrstuvwxyz\r\nmenu", "ab\tcd\r\nmenu", "\r\nmenu"]
        .iter()
        .map(|input| build_frame(&term(30, 2, input)))
        .collect();
    let shown = shown_on_terminal(&frames, area, 40, 2);
    assert_eq!(shown[0], vec!["   abcdefghijklmnopqrstuvwxyz", "   menu"]);
    assert_eq!(shown[1], vec!["   ab      cd", "   menu"]);
    assert_eq!(shown[2], vec!["", "   menu"], "the cleared line leaves no letters");
}

#[test]
fn tab_cells_are_blank_in_frames_and_screen_text() {
    let t = term(20, 1, "ab\tcd");
    assert_eq!(t.grid()[Line(0)][Column(2)].c, '\t', "the emulator keeps the tab in its cell");
    let frame = build_frame(&t);
    let row: Vec<(u16, &str)> = frame.lines[0].iter().map(|r| (r.c, r.t.as_str())).collect();
    assert_eq!(row, vec![(0, "ab"), (8, "cd")]);
    assert_eq!(screen_text(&t), vec!["ab      cd"]);
}

#[test]
fn render_frame_clips_to_a_smaller_area() {
    let t = term(6, 2, "abcdef\r\n日日日");
    let frame = build_frame(&t);
    let area = Rect { x: 0, y: 0, width: 3, height: 1 };
    let mut buf = Buffer::empty(Rect { x: 0, y: 0, width: 6, height: 2 });
    render_frame(&frame, area, &mut buf);
    let row0: String = (0..6).map(|x| buf[(x, 0)].symbol().to_string()).collect();
    assert_eq!(row0, "abc   ");
    let row1: String = (0..6).map(|x| buf[(x, 1)].symbol().to_string()).collect();
    assert_eq!(row1, "      ");
}

#[test]
fn colour_encoding_round_trips() {
    assert_eq!(decode_color(C_DEFAULT), Color::Reset);
    assert_eq!(decode_color(indexed(9)), Color::Indexed(9));
    assert_eq!(decode_color(rgb(10, 20, 30)), Color::Rgb(10, 20, 30));
    assert_eq!(encode_color(AnsiColor::Named(NamedColor::Red)), indexed(1));
    assert_eq!(encode_color(AnsiColor::Named(NamedColor::DimBlue)), indexed(4));
    assert_eq!(encode_color(AnsiColor::Named(NamedColor::Foreground)), C_DEFAULT);
}

#[test]
fn theme_colours_stay_palette_references_unless_the_program_redefines_them() {
    let t = term(10, 1, "\x1b[31ma\x1b[38;5;200mb\x1b]4;1;rgb:12/34/56\x07\x1b[31mc\x1b]10;rgb:aa/bb/cc\x07\x1b[39md");
    let frame = build_frame(&t);
    let fg: Vec<u32> = frame.lines[0].iter().flat_map(|r| std::iter::repeat_n(r.fg, r.t.chars().count())).collect();
    // `a` was drawn before the redefinition, but the cell keeps naming palette
    // entry 1, so it shows the program's new colour as a real terminal would.
    assert_eq!(fg, vec![rgb(0x12, 0x34, 0x56), indexed(200), rgb(0x12, 0x34, 0x56), rgb(0xaa, 0xbb, 0xcc)]);
    let plain = build_frame(&term(4, 1, "\x1b[31ma\x1b[94mb\x1b[39mc"));
    let fg: Vec<u32> = plain.lines[0].iter().flat_map(|r| std::iter::repeat_n(r.fg, r.t.chars().count())).collect();
    assert_eq!(fg, vec![indexed(1), indexed(12), C_DEFAULT], "untouched entries follow the outer terminal's theme");
}

#[test]
fn the_text_cursor_counts_characters_not_cells_and_hides_with_the_cursor() {
    // Codex's input box: the prompt, then the cursor where typing goes.
    let t = term(20, 3, "\x1b[2;1H›\x1b[2;3H");
    assert_eq!(screen_text(&t)[1], "›");
    assert_eq!(text_cursor(&t), Some((2, 1)));
    // A wide character takes two cells but one character of the text.
    let wide = term(20, 2, "界x\x1b[1;4H");
    assert_eq!(screen_text(&wide)[0], "界x");
    assert_eq!(text_cursor(&wide), Some((2, 0)));
    assert_eq!(text_cursor(&term(20, 2, "busy\x1b[?25l")), None, "a hidden cursor is no input box");
}

#[test]
fn frame_serializes_compactly() {
    let t = term(4, 1, "e\u{301}x");
    let json = serde_json::to_string(&build_frame(&t)).unwrap();
    assert!(json.contains("\"g\":true"));
    assert_eq!(json.matches("\"g\"").count(), 1, "plain runs omit the grapheme flag: {json}");
}

/// Every cell of history and screen, top to bottom.
fn all_cells(t: &Term<VoidListener>) -> Vec<Vec<Cell>> {
    let grid = t.grid();
    (grid.topmost_line().0..=grid.bottommost_line().0)
        .map(|y| (0..grid.columns()).map(|x| grid[Line(y)][Column(x)].clone()).collect())
        .collect()
}

/// What an update must carry over: the cells, palette, modes and cursor.
fn assert_same_terminal(copy: &Term<VoidListener>, original: &Term<VoidListener>) {
    assert_eq!(copy.grid().history_size(), original.grid().history_size(), "scrollback length");
    let (copy_cells, original_cells) = (all_cells(copy), all_cells(original));
    for (y, (a, b)) in copy_cells.iter().zip(&original_cells).enumerate() {
        assert_eq!(a, b, "line {y} of history and screen");
    }
    assert_eq!(*copy.mode(), *original.mode());
    let palette = |t: &Term<VoidListener>| (0..269).map(|i| t.colors()[i]).collect::<Vec<_>>();
    assert_eq!(palette(copy), palette(original));
    assert_eq!(copy.cursor_style(), original.cursor_style());
    assert_eq!(copy.grid().cursor.point, original.grid().cursor.point);
    assert_eq!(
        Pen::of(&copy.grid().cursor.template),
        Pen::of(&original.grid().cursor.template),
        "the pen typing continues with"
    );
}

#[test]
fn replay_rebuilds_scrollback_screen_palette_modes_and_cursor_in_a_new_emulator() {
    let mut input = String::new();
    for i in 0..12 {
        input.push_str(&format!("\x1b[3{}mhistory {i}\x1b[0m\r\n", i % 8));
    }
    input.push_str(
        "\x1b[1;4:3;58;5;9;38;5;200;48;2;1;2;3mstyled\x1b[0m \x1b[94;101mbright\x1b[0m \x1b[2;3;7;9mmix\x1b[0m\r\n",
    );
    // A line that wraps onto a row holding just the space that moved it on.
    input.push_str("abcdefghijklmnopqrst \r\n");
    // A wide character that did not fit at the end of the row.
    input.push_str("1234567890123456789日x\r\n");
    input.push_str("e\u{301} \x1b]8;id=a;https://example.com\x1b\\link\x1b]8;;\x1b\\ \x1b]8;;https://anon.example\x1b\\anon\x1b]8;;\x1b\\\r\n");
    input.push_str("\x1b[44m  \x1b[0m indented background");
    input.push_str("\x1b]4;1;rgb:12/34/56\x1b\\\x1b]11;rgb:01/02/03\x1b\\");
    input.push_str("\x1b[?2004h\x1b[?1006h\x1b[?1002h\x1b[?1h\x1b=\x1b[?1004h\x1b[5 q");
    input.push_str("\x1b[3;7H\x1b[1;32m");
    let original = term(20, 6, &input);
    assert!(original.grid().history_size() > 10, "the scenario fills history");

    let copy = term(20, 6, &replay(&original));
    assert_same_terminal(&copy, &original);
    assert_eq!(screen_text(&copy), screen_text(&original));
    assert_eq!(build_frame(&copy), build_frame(&original), "the deck draws the same screen");
}

#[test]
fn replay_keeps_text_after_a_tab_in_its_columns() {
    let original = term(20, 2, "ab\tcd\r\n\x1b[44mx\tz\x1b[0m");
    let copy = term(20, 2, &replay(&original));
    assert_eq!(screen_text(&copy), vec!["ab      cd", "x       z"]);
    assert_eq!(screen_text(&copy), screen_text(&original));
    assert_eq!(build_frame(&copy), build_frame(&original), "the deck draws the same screen");
}

#[test]
fn replay_keeps_the_alternate_screen_and_switched_off_modes() {
    let original =
        term(12, 4, "main text\r\n\x1b[?1049h\x1b[2;3Halt \x1b[7mscreen\x1b[0m\x1b[?25l\x1b[?7l\x1b[?1007l\x1b[2 q");
    let copy = term(12, 4, &replay(&original));
    assert!(copy.mode().contains(TermMode::ALT_SCREEN));
    assert_same_terminal(&copy, &original);
    assert_eq!(build_frame(&copy).cursor, None, "the hidden cursor stays hidden");
}

#[test]
fn automatic_input_requires_the_entire_active_composer_to_be_empty() {
    use crate::{protocol::Tool, status::empty_automatic_composer};
    let claude = "\x1b[?2004hready\r\n────────────────────\r\n❯ \r\n────────────────────\x1b[1A\x1b[3G";
    let codex = "\x1b[?2004hready\r\n\r\n› \x1b[2mAny configured placeholder\x1b[0m\r\n\x1b[1A\x1b[3G";
    assert!(empty_automatic_composer(Tool::Claude, &term(60, 12, claude)));
    assert!(empty_automatic_composer(Tool::Codex, &term(60, 12, codex)));
    for (tool, text) in [
        (Tool::Claude, claude.replace("❯ ", "❯ owner draft")),
        (Tool::Claude, claude.replace("❯ \r\n", "❯ \r\n  continuation draft\r\n").replace("\x1b[1A", "\x1b[2A")),
        (Tool::Claude, claude.replace("────────────────────\x1b[1A", "missing boundary\x1b[1A")),
        (Tool::Claude, "\x1b[?2004h❯ ".into()),
        (Tool::Codex, codex.replace("\x1b[2mAny configured placeholder\x1b[0m", "owner draft")),
        (Tool::Codex, codex.replace("› ", "\x1b[2m› ")),
        (Tool::Codex, codex.replace("\x1b[0m\r\n", "\x1b[0m\r\n  continuation draft")),
        (Tool::Codex, codex.replace("Any configured placeholder", "Any configured placeholder ↑")),
        (Tool::Codex, codex.replace("ready\r\n\r\n", "ready\r\n[Image #1]\r\n")),
        (Tool::Codex, codex.replace("ready\r\n\r\n", "ready\r\n\x1b[36m[Image #1]\x1b[0m\r\n\r\n")),
        (Tool::Codex, codex.replace("ready\r\n\r\n", "ready\r\n\x1b[36;7m[Image #123456789…\x1b[0m\r\n\r\n")),
        (Tool::Codex, "\x1b[?2004h\r\n› ".into()),
    ] {
        assert!(!empty_automatic_composer(tool, &term(60, 12, &text)), "{tool:?}: {text:?}");
    }
    let mut scrolled = term(60, 4, &format!("older\r\n{codex}"));
    scrolled.scroll_display(Scroll::Top);
    assert!(empty_automatic_composer(Tool::Codex, &scrolled), "only live grid cells count");
}

#[test]
fn only_explicit_active_text_submissions_clear_owner_edit_uncertainty() {
    use crate::{protocol::Tool, status::owner_composer_submission};
    let empty = "\x1b[?2004h\r\n\r\n› \x1b[2mAny placeholder\x1b[0m\r\n\x1b[1A\x1b[3G";
    assert!(!owner_composer_submission(Tool::Codex, &term(60, 12, empty), ""));
    assert!(owner_composer_submission(Tool::Codex, &term(60, 12, empty), "normal owner text"));
    for inline in ["/help", "!rm old", " /clear"] {
        assert!(!owner_composer_submission(Tool::Codex, &term(60, 12, empty), inline));
    }
    let draft = empty.replace("\x1b[2mAny placeholder\x1b[0m", "normal owner text");
    assert!(owner_composer_submission(Tool::Codex, &term(60, 12, &draft), ""));
    assert!(!owner_composer_submission(Tool::Codex, &term(60, 12, &draft.replace("› ", "\x1b[2m› ")), "normal text"));
}
