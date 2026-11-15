use super::*;
use crate::screen::{frame_text, HistorySnapshot, Run, M_BRACKETED_PASTE};

fn fixture(id: u64) -> (App, Receiver<ClientMsg>) {
    let (mut app, rx) = tests::app_with(vec![tests::session(id, Tool::Claude, Status::Idle, true)], vec![]);
    app.state.sessions[0].metadata.insert("_deck_terminal_epoch".into(), serde_json::json!(11));
    app.frame = Some((
        id,
        ScreenFrame {
            epoch: 11,
            revision: 7,
            cols: 80,
            rows: 4,
            history: 100,
            lines: vec![vec![run("live")]],
            ..ScreenFrame::default()
        },
    ));
    rx.try_iter().for_each(drop);
    (app, rx)
}
fn run(text: &str) -> Run {
    Run { c: 0, w: 1, fg: 0, bg: 0, fl: 0, t: text.into(), g: false }
}
fn begin(app: &mut App, rx: &Receiver<ClientMsg>) -> ClientMsg {
    tests::press(app, KeyCode::PageUp, KeyModifiers::NONE);
    let sent: Vec<_> = rx.try_iter().collect();
    assert_eq!(sent.len(), 1);
    assert!(matches!(sent[0], ClientMsg::History { .. }));
    sent.into_iter().next().unwrap()
}
fn response(msg: ClientMsg) -> ServerMsg {
    let ClientMsg::History { id, request, epoch, cols, rows } = msg else { panic!("history request") };
    ServerMsg::History {
        id,
        request,
        epoch,
        cols,
        rows,
        error: None,
        snapshot: Some(HistorySnapshot {
            revision: 7,
            history: 100,
            truncated: false,
            modes: 0,
            lines: (0..104).map(|i| vec![run(&format!("line {i}"))]).collect(),
        }),
    }
}

#[test]
fn session_history_pages_are_local_and_frozen_while_live_output_advances() {
    let (mut app, rx) = fixture(1);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 10)).unwrap();
    terminal
        .draw(|f| {
            crate::view::render(f, &mut app);
        })
        .unwrap();
    let request = begin(&mut app, &rx);
    tests::press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    tests::press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert!(rx.try_recv().is_err(), "one snapshot in flight even while paging fast");
    let mut incoming = app.frame.as_ref().unwrap().1.clone();
    incoming.revision = 8;
    app.handle_server(ServerMsg::Frame { id: 1, frame: incoming });
    app.handle_server(response(request));
    assert_eq!(frame_text(app.session_frame(1).unwrap())[0], "line 91");
    terminal
        .draw(|f| {
            crate::view::render(f, &mut app);
        })
        .unwrap();
    let visible: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
    assert!(visible.contains("line 91"), "the pane renders cached rows rather than the live frame");
    app.handle_server(ServerMsg::State(app.state.clone()));
    assert_eq!(frame_text(app.session_frame(1).unwrap())[0], "line 91", "matching state keeps cached custody");
    let mut live = app.frame.as_ref().unwrap().1.clone();
    live.revision = 40;
    live.lines = vec![vec![run("new live output")]];
    app.handle_server(ServerMsg::Frame { id: 1, frame: live });
    tests::press(&mut app, KeyCode::PageUp, KeyModifiers::SHIFT);
    assert_eq!(app.session_frame(1).unwrap().offset, 49);
    tests::press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(frame_text(app.session_frame(1).unwrap())[0], "line 0");
    tests::press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(frame_text(app.session_frame(1).unwrap())[0], "line 3");
    tests::press(&mut app, KeyCode::End, KeyModifiers::NONE);
    assert_eq!(frame_text(app.session_frame(1).unwrap()), ["new live output"]);
    assert!(rx.try_recv().is_err(), "all pages and End are local");
    begin(&mut app, &rx); // Returning to history after new output requests a fresh snapshot.
}

#[test]
fn session_history_preserves_escape_and_forwards_typing_and_paste_once() {
    let (mut app, rx) = fixture(1);
    let request = begin(&mut app, &rx);
    app.handle_server(response(request));
    let offset = app.session_frame(1).unwrap().offset;
    tests::press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.session_frame(1).unwrap().offset, offset);
    assert!(rx.try_recv().is_err());
    app.mode = Mode::Insert;
    tests::press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), ClientMsg::Input { id: 1, data: "\x1b".into() });
    assert_eq!(app.session_frame(1).unwrap().offset, offset, "Escape never operates history");
    tests::press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), ClientMsg::Input { id: 1, data: "q".into() });
    assert_eq!(app.session_frame(1).unwrap().offset, 0);
    tests::press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert!(rx.try_recv().is_err(), "unchanged output reuses the cached snapshot");
    app.frame.as_mut().unwrap().1.modes = M_BRACKETED_PASTE;
    app.handle_input(Event::Paste("first\nsecond".into()));
    assert_eq!(rx.try_recv().unwrap(), ClientMsg::Input { id: 1, data: "\x1b[200~first\nsecond\x1b[201~".into() });
    assert_eq!(app.session_frame(1).unwrap().offset, 0);
    assert!(rx.try_recv().is_err());
    tests::press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    tests::press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    tests::press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), ClientMsg::Input { id: 1, data: "\x17".into() });
    assert_eq!(rx.try_recv().unwrap(), ClientMsg::Input { id: 1, data: "q".into() });
    assert_eq!(app.session_frame(1).unwrap().offset, 0);
    assert!(rx.try_recv().is_err(), "the unbound window prefix forwards both original keys once");
}

#[test]
fn session_history_drag_selects_the_cached_page_without_mouse_navigation() {
    let (mut app, rx) = fixture(1);
    app.set_pane(Rect::new(0, 0, 80, 4));
    let request = begin(&mut app, &rx);
    app.handle_server(response(request));
    let mouse = |kind, column| Event::Mouse(MouseEvent { kind, column, row: 1, modifiers: KeyModifiers::NONE });
    app.handle_input(mouse(MouseEventKind::Down(MouseButton::Left), 0));
    app.handle_input(mouse(MouseEventKind::Drag(MouseButton::Left), 6));
    assert_eq!(app.selection.as_ref().unwrap().text(), "line 98");
    let mut live = app.frame.as_ref().unwrap().1.clone();
    live.revision = 8;
    live.lines = vec![vec![run("new live output")]];
    app.handle_server(ServerMsg::Frame { id: 1, frame: live });
    app.handle_input(mouse(MouseEventKind::Up(MouseButton::Left), 6));
    assert_eq!(app.notice.as_ref().unwrap().0, "Selection copied.");
    assert!(app.selection.as_ref().is_some_and(|s| s.text() == "line 98" && !s.pressed));
    assert_eq!(app.session_frame(1).unwrap().offset, 3, "mouse selection never navigates history");
    tests::press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert!(app.selection.is_none(), "a new page clears the previous page's highlight");
    tests::press(&mut app, KeyCode::End, KeyModifiers::NONE);
    assert_eq!(frame_text(app.session_frame(1).unwrap()), ["new live output"]);
    assert!(rx.try_recv().is_err(), "selecting and paging cached text stays local");
}

#[test]
fn session_history_rejects_late_resize_reopen_switch_and_foreign_host_replies() {
    for change in 0..6 {
        let id = crate::protocol::host_id(1, 1);
        let (mut app, rx) = fixture(id);
        let request = begin(&mut app, &rx);
        let mut reply = response(request);
        match change {
            0 => app.handle_input(Event::Resize(100, 30)),
            1 => {
                let mut frame = app.frame.as_ref().unwrap().1.clone();
                frame.epoch = 12;
                app.handle_server(ServerMsg::Frame { id, frame });
            }
            2 => app.select(Sel::Session(crate::protocol::host_id(2, 1))),
            3 => {
                let mut state = app.state.clone();
                state.sessions[0].metadata.insert("_deck_terminal_epoch".into(), serde_json::json!(12));
                app.handle_server(ServerMsg::State(state));
            }
            4 => {
                let mut state = app.state.clone();
                state.sessions[0].metadata.remove("_deck_terminal_epoch");
                app.handle_server(ServerMsg::State(state));
            }
            _ => {
                if let ServerMsg::History { id, .. } = &mut reply {
                    *id = crate::protocol::host_id(2, 1);
                }
            }
        }
        app.handle_server(reply);
        assert!(app.session_frame(id).is_none_or(|f| f.offset == 0));
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn session_history_cancelled_requests_cannot_replace_new_navigation() {
    let (mut app, rx) = fixture(1);
    let old = begin(&mut app, &rx);
    tests::press(&mut app, KeyCode::End, KeyModifiers::NONE);
    let current = begin(&mut app, &rx);
    app.handle_server(response(old));
    assert_eq!(app.session_frame(1).unwrap().offset, 0);
    app.handle_server(response(current));
    assert!(app.history_active());
    assert!(rx.try_recv().is_err());
}

#[test]
fn session_history_keys_work_while_typing_and_normal_commands_keep_their_roles() {
    let (mut app, rx) = fixture(1);
    app.mode = Mode::Insert;
    let request = begin(&mut app, &rx);
    app.handle_server(response(request));
    assert!(app.history_active());
    tests::press(&mut app, KeyCode::End, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Insert);
    assert!(!app.history_active());
    tests::press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert!(app.history_active());
    app.mode = Mode::Normal;
    tests::press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Command);
    assert!(rx.try_recv().is_err(), "Normal ':' never types into the session");
    app.mode = Mode::Normal;
    tests::press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Insert);
    assert!(!app.history_active());
    assert!(rx.try_recv().is_err(), "Enter enters the typing pane without sending a key");
}

fn wheel(app: &mut App, up: bool) {
    let kind = if up { MouseEventKind::ScrollUp } else { MouseEventKind::ScrollDown };
    app.handle_input(Event::Mouse(MouseEvent { kind, column: 10, row: 1, modifiers: KeyModifiers::NONE }));
}

#[test]
fn session_wheel_reads_local_history_like_a_web_page() {
    let (mut app, rx) = fixture(1);
    app.set_pane(Rect::new(0, 0, 80, 4));
    wheel(&mut app, false);
    assert!(rx.try_recv().is_err(), "scrolling toward live output has nothing to fetch");
    wheel(&mut app, true);
    let sent: Vec<_> = rx.try_iter().collect();
    assert!(matches!(sent.as_slice(), [ClientMsg::History { id: 1, .. }]), "{sent:?}");
    app.handle_server(response(sent.into_iter().next().unwrap()));
    let offset = |app: &App| app.session_frame(1).unwrap().offset as usize;
    assert_eq!(offset(&app), scroll::WHEEL_ROWS, "one report moves three rows");
    assert_eq!(frame_text(app.session_frame(1).unwrap())[0], "line 97");
    app.wheel_scroll.reset();
    wheel(&mut app, true);
    assert_eq!(offset(&app), 2 * scroll::WHEEL_ROWS, "an isolated report stays precise");
    app.wheel_scroll.reset();
    let before = offset(&app);
    for _ in 0..8 {
        wheel(&mut app, true);
    }
    let travelled = offset(&app) - before;
    assert!(travelled > 8 * scroll::WHEEL_ROWS, "a fast swipe travels further: {travelled}");
    assert!(travelled <= 8 * 4 * scroll::WHEEL_ROWS, "acceleration stays bounded: {travelled}");
    for _ in 0..200 {
        wheel(&mut app, true);
    }
    assert_eq!(frame_text(app.session_frame(1).unwrap())[0], "line 0", "the oldest retained row is reachable");
    let mut live = app.frame.as_ref().unwrap().1.clone();
    live.revision = 9;
    live.lines = vec![vec![run("new live output")]];
    app.handle_server(ServerMsg::Frame { id: 1, frame: live });
    assert_eq!(frame_text(app.session_frame(1).unwrap())[0], "line 0", "new output leaves the page in place");
    for _ in 0..200 {
        wheel(&mut app, false);
    }
    assert!(!app.history_active(), "scrolling down returns to live output without Esc");
    assert_eq!(frame_text(app.session_frame(1).unwrap()), ["new live output"]);
    assert!(rx.try_recv().is_err(), "every step after the first snapshot is local");
}

#[test]
fn session_wheel_without_history_sends_and_reports_nothing() {
    let (mut app, rx) = fixture(1);
    app.set_pane(Rect::new(0, 0, 80, 4));
    app.frame.as_mut().unwrap().1.history = 0;
    let before = (app.notice.clone(), app.local_errors.len(), app.messages.len());
    for _ in 0..5 {
        wheel(&mut app, true);
    }
    assert!(rx.try_recv().is_err());
    assert_eq!((app.notice.clone(), app.local_errors.len(), app.messages.len()), before, "no notice per wheel step");
}

#[test]
fn full_screen_programs_keep_their_page_keys_and_wheel() {
    use crate::screen::{M_MOUSE_CLICK, M_SGR_MOUSE};
    let (mut app, rx) = fixture(1);
    app.set_pane(Rect::new(0, 0, 80, 4));
    let modes = M_ALT_SCREEN | M_MOUSE_CLICK | M_SGR_MOUSE;
    {
        let frame = &mut app.frame.as_mut().unwrap().1;
        frame.modes = modes;
        frame.history = 0;
    }
    let encoded = |code, m| keys::encode_key(&KeyEvent::new(code, m), modes).unwrap();
    app.mode = Mode::Insert;
    for (code, m) in [
        (KeyCode::PageUp, KeyModifiers::NONE),
        (KeyCode::PageDown, KeyModifiers::NONE),
        (KeyCode::End, KeyModifiers::CONTROL),
        (KeyCode::Home, KeyModifiers::CONTROL),
    ] {
        tests::press(&mut app, code, m);
        assert_eq!(rx.try_recv().unwrap(), ClientMsg::Input { id: 1, data: encoded(code, m) }, "{code:?} {m:?}");
        assert!(rx.try_recv().is_err());
    }
    app.mode = Mode::Normal;
    tests::press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(rx.try_recv().unwrap(), ClientMsg::Input { id: 1, data: encoded(KeyCode::PageUp, KeyModifiers::NONE) });
    tests::press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    assert!(rx.try_recv().is_err(), "other Normal-mode keys never type into the program");
    wheel(&mut app, true);
    assert!(
        matches!(rx.try_recv(), Ok(ClientMsg::Input { id: 1, data }) if data.starts_with("\x1b[<64;")),
        "the program's own wheel reports"
    );
    assert!(!app.history_active() && rx.try_recv().is_err(), "the program's scrollback is never fetched");
}
