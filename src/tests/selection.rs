use super::*;
use crate::screen::Run;
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

fn mouse(column: u16, row: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE }
}

fn draft() -> ScreenFrame {
    ScreenFrame {
        cols: 12,
        rows: 3,
        lines: vec![
            vec![
                Run { c: 2, w: 2, fg: 0, bg: 0, fl: 0, t: "中".into(), g: false },
                Run { c: 4, w: 1, fg: 0, bg: 0, fl: 0, t: "e\u{301}".into(), g: true },
                Run { c: 5, w: 2, fg: 0, bg: 0, fl: 0, t: "🙂".into(), g: false },
            ],
            vec![Run { c: 2, w: 1, fg: 0, bg: 0, fl: 0, t: "next line".into(), g: false }],
        ],
        ..ScreenFrame::default()
    }
}

#[test]
fn forward_and_backward_drags_copy_whole_glyphs_and_visible_line_breaks() {
    let pane = Rect::new(35, 2, 12, 3);
    for (start, end) in [(mouse(38, 2), mouse(39, 3)), (mouse(39, 3), mouse(38, 2))] {
        let mut selection = TextSelection::new(draft(), pane, start);
        selection.update(pane, end);
        assert_eq!(selection.text(), "中e\u{301}🙂\n  nex");
        let mut buffer = Buffer::empty(pane);
        screen::render_frame(&selection.frame, pane, &mut buffer);
        let theme = Theme::from_palette(&crate::config::Palette::from_kitty_entries(&[]));
        selection.highlight(pane, &mut buffer, &theme);
        assert_eq!(buffer[(37, 2)].bg, theme.accent, "selecting the second cell includes the whole wide glyph");
        assert_eq!(buffer[(39, 3)].bg, theme.accent);
        assert_ne!(buffer[(40, 3)].bg, theme.accent, "text past the endpoint stays unselected");
    }
}

#[test]
fn dragging_past_the_pane_stops_at_its_edge() {
    let pane = Rect::new(35, 2, 12, 2);
    let mut selection = TextSelection::new(draft(), pane, mouse(37, 2));
    selection.update(pane, mouse(400, 300));
    assert_eq!(selection.text(), "中e\u{301}🙂\n  next line");
}

#[test]
fn cursor_updates_keep_selection_but_replaced_text_or_scroll_clears_it() {
    let pane = Rect::new(0, 0, 12, 3);
    let mut selection = TextSelection::new(draft(), pane, mouse(2, 0));
    selection.update(pane, mouse(6, 0));
    let mut next = draft();
    next.cursor = Some(screen::CursorPos { x: 3, y: 0, shape: 0 });
    assert!(selection.matches(&next));
    next.lines[1][0].t = "other line".into();
    assert!(selection.matches(&next), "output below the selection can keep streaming");
    next.lines[0][0].t = "字".into();
    assert!(!selection.matches(&next));
    next = draft();
    next.offset = 1;
    assert!(!selection.matches(&next));
}
