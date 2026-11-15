use super::*;
use ratatui::{backend::TestBackend, Terminal};

fn draw(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| {
            view::render(f, app);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height).map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect()).collect()
}

fn wheel(app: &mut App, down: bool) {
    app.handle_input(Event::Mouse(MouseEvent {
        kind: if down { MouseEventKind::ScrollDown } else { MouseEventKind::ScrollUp },
        column: 20,
        row: 8,
        modifiers: KeyModifiers::NONE,
    }));
}

fn key(app: &mut App, code: KeyCode) {
    app.handle_input(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

#[test]
fn help_navigation_keeps_the_menu_open_and_reaches_both_ends() {
    for (width, height) in [(147, 38), (200, 55), (60, 15)] {
        let (mut app, rx) = tests::app_with(vec![], vec![]);
        app.lua_keys.push(crate::lua::Keymap {
            chord: crate::hotkeys::Chord::parse("alt+x").unwrap(),
            key: "Alt+x".into(),
            desc: "last plugin shortcut".into(),
            action: crate::lua::KeyAction::Command("help".into()),
        });
        app.dialog = Some(Dialog::Help);
        let first = draw(&mut app, width, height);
        rx.try_iter().for_each(drop);
        assert!(app.dialog_viewport.limit > 0, "Help includes the full command catalog at {width}x{height}");
        wheel(&mut app, true);
        let after_wheel = draw(&mut app, width, height);
        assert_ne!(first, after_wheel, "wheel scrolling visibly moves Help");
        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Down);
        assert!(matches!(app.dialog, Some(Dialog::Help)), "an arrow scrolls help");
        let after_arrow = draw(&mut app, width, height);
        assert_ne!(first, after_arrow);
        let limit = app.dialog_viewport.limit;
        key(&mut app, KeyCode::Char('j'));
        let after_j = draw(&mut app, width, height);
        assert_eq!(app.dialog_viewport.offset, 2.min(limit));
        if limit == 1 {
            assert_eq!(after_arrow, after_j, "the second key clamps at the one-row boundary");
            key(&mut app, KeyCode::Home);
            assert_eq!(first, draw(&mut app, width, height));
            key(&mut app, KeyCode::Char('j'));
            assert_eq!(after_arrow, draw(&mut app, width, height), "j still scrolls from the top");
        } else {
            assert_ne!(after_arrow, after_j);
        }
        for _ in 0..app.dialog_viewport.limit.saturating_add(5) {
            wheel(&mut app, true);
        }
        let end = draw(&mut app, width, height);
        assert!(end.join("\n").contains("last plugin shortcut"), "last help entry is reachable: {end:#?}");
        key(&mut app, KeyCode::Up);
        assert_ne!(end, draw(&mut app, width, height), "no overscroll dead zone");
        key(&mut app, KeyCode::Home);
        assert_eq!(first, draw(&mut app, width, height));
        assert!(rx.try_iter().next().is_none(), "menu input must stay out of the hosted session");
        key(&mut app, KeyCode::Char('/'));
        for c in "resume".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        let searched = draw(&mut app, width, height).join("\n");
        assert!(searched.contains("/ resume"), "search stays above its live results: {searched}");
        assert!(!searched.contains("Change them under"), "unrelated help rows are filtered");
        key(&mut app, KeyCode::Esc);
        assert_eq!(first, draw(&mut app, width, height), "Esc restores the complete help sheet");
        key(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none());
    }
}

#[test]
fn readers_scroll_rendered_lines_including_a_single_long_error() {
    let (mut app, _) = tests::app_with(vec![], vec![]);
    let lines: Vec<String> = (0..60).map(|i| format!("message line {i:02}")).collect();
    app.local_errors.push(crate::protocol::LogEntry::new(util::now_secs(), lines.join("\n")));
    for report in [false, true] {
        app.dialog = Some(if report {
            Dialog::Report { title: "Output".into(), lines: lines.clone(), scroll: 0, confirm: None, waiting: 0 }
        } else {
            Dialog::Errors { scroll: 0 }
        });
        draw(&mut app, 60, 15);
        for _ in 0..100 {
            wheel(&mut app, true);
        }
        let end = draw(&mut app, 60, 15);
        assert!(end.join("\n").contains("message line 59"), "all output is reachable: {end:#?}");
        wheel(&mut app, false);
        assert_ne!(end, draw(&mut app, 60, 15));
        key(&mut app, KeyCode::Char('/'));
        app.handle_input(Event::Paste("line 59".into()));
        let searched = draw(&mut app, 60, 15).join("\n");
        assert!(searched.contains("message line 59") && !searched.contains("message line 58"), "{searched}");
        key(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_some(), "clearing search keeps the reader open");
    }
}

#[test]
fn wheel_and_vim_navigation_keep_filter_text_and_broadcast_newlines() {
    let (mut app, _) = tests::app_with(vec![], vec![]);
    let items = (0..40)
        .map(|i| {
            crate::lua::PickItem::choice(vec![crate::lua::Segment {
                text: format!("choice {i:02}"),
                style: String::new(),
            }])
        })
        .collect();
    let pick = Box::new(crate::lua::Pick::new("Choose", items));
    app.dialog = Some(Dialog::Choose { pick, sel: 0, filter: String::new(), callback: 0 });
    draw(&mut app, 60, 15);
    for _ in 0..60 {
        wheel(&mut app, true);
    }
    assert!(draw(&mut app, 60, 15).join("\n").contains("choice 39"));
    app.handle_input(Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT)));
    assert!(matches!(&app.dialog, Some(Dialog::Choose { sel: 38, filter, .. }) if filter.is_empty()));
    key(&mut app, KeyCode::Char('j'));
    key(&mut app, KeyCode::Char('k'));
    assert!(matches!(&app.dialog, Some(Dialog::Choose { sel: 38, filter, .. }) if filter.is_empty()));
    app.handle_input(Event::Paste("ignored".into()));
    assert!(matches!(&app.dialog, Some(Dialog::Choose { filter, .. }) if filter.is_empty()));
    key(&mut app, KeyCode::Char('/'));
    key(&mut app, KeyCode::Char('j'));
    key(&mut app, KeyCode::Char('k'));
    assert!(matches!(&app.dialog, Some(Dialog::Choose { filter, .. }) if filter == "jk"));
    key(&mut app, KeyCode::Esc);
    assert!(matches!(&app.dialog, Some(Dialog::Choose { filter, .. }) if filter.is_empty()));
    app.dialog = Some(Dialog::Broadcast { text: String::new(), targets: vec![], sel: 0 });
    app.handle_input(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)));
    assert!(matches!(&app.dialog, Some(Dialog::Broadcast { text, .. }) if text == "\n"));
}

#[test]
fn selection_menus_keep_the_last_item_visible_in_a_short_terminal() {
    let sessions = (1..=40).map(|id| tests::session(id, Tool::Codex, Status::Idle, true)).collect();
    let (mut app, rx) = tests::app_with(sessions, vec![]);
    app.hosts.push(tests::host("fedora", "Fedora", true));
    app.state.branches = (0..40)
        .map(|i| crate::protocol::BranchInfo {
            branch: format!("branch-{i:02}"),
            path: format!("/repo/{i}"),
            ..Default::default()
        })
        .collect();
    app.state.scans =
        (0..40).map(|i| crate::protocol::ScanInfo { id: format!("scan-{i:02}"), ..Default::default() }).collect();
    let conversations = (0..40)
        .map(|i| Conversation {
            search_words: Vec::new(),
            tool: Tool::Codex,
            id: format!("conversation-{i:02}"),
            title: format!("conversation-{i:02}"),
            created_at: 0,
            updated_at: 0,
            open_in: None,
            host: None,
        })
        .collect();
    let dir = crate::testsupport::TempDir::new("menu-pairing");
    let mut pairing = Pairing::open(dir.path(), &Config::default(), util::now_secs());
    pairing.devices = (0..40)
        .map(|i| crate::remote::store::Device {
            id: format!("phone-{i:02}"),
            name: format!("phone-{i:02}"),
            key_sha256: String::new(),
            added: 0,
            last_seen: None,
        })
        .collect();
    let dialogs = [
        (Dialog::New { sel: 0, model: 0, effort: 0 }, "opencode"),
        (
            Dialog::Picker {
                items: conversations,
                sel: 0,
                filter: String::new(),
                loading: false,
                search_due: None,
                search_request: None,
            },
            "conversation-39",
        ),
        (Dialog::Branches { sel: 0, filter: String::new() }, "branch-39"),
        (Dialog::Scans { sel: 0 }, "scan-39"),
        (
            Dialog::Broadcast { text: String::new(), targets: (1..=40).map(|id| (id, true)).collect(), sel: 0 },
            "task 40",
        ),
        (Dialog::Pair(Box::new(pairing)), "phone-39"),
    ];
    for (dialog, expected) in dialogs {
        app.dialog = Some(dialog);
        draw(&mut app, 60, 15);
        rx.try_iter().for_each(drop);
        for _ in 0..60 {
            wheel(&mut app, true);
        }
        let rows = draw(&mut app, 60, 15);
        assert!(rows.join("\n").contains(expected), "{expected} must be visible: {rows:#?}");
        assert!(rx.try_iter().next().is_none(), "navigation never sends input or performs an action");
        draw(&mut app, 120, 50);
        let smaller = draw(&mut app, 60, 10);
        assert!(smaller.join("\n").contains(expected), "selection remains visible after shrinking: {smaller:#?}");
    }
}

#[test]
fn a_question_can_be_pasted_and_up_brings_back_an_earlier_one() {
    let (mut app, _rx) = tests::app_with(vec![], vec![]);
    let asked = crate::ask::Asked { question: "earlier question".into(), answer: "Lima.".into(), ..Default::default() };
    app.ask_history.push(asked);
    app.dialog = Some(Dialog::Ask { draft: Default::default(), route: None, scroll: 0 });
    let typed = |app: &App| match &app.dialog {
        Some(Dialog::Ask { draft, .. }) => draft.text().to_string(),
        _ => panic!("the box stays open"),
    };
    // A paste into the box was dropped (owner, 2026-11-10).
    app.handle_input(Event::Paste("capital\r\nof peru".into()));
    assert_eq!(typed(&app), "capital\nof peru");
    assert!(draw(&mut app, 80, 20).join("\n").contains("› capital"));
    key(&mut app, KeyCode::Up);
    assert_eq!(typed(&app), "earlier question");
    key(&mut app, KeyCode::Down);
    assert_eq!(typed(&app), "capital\nof peru");
}

#[test]
fn answer_and_confirmation_scrolling_are_bounded_without_performing_actions() {
    let (mut app, rx) = tests::app_with(vec![], vec![]);
    app.ask_history.push(crate::ask::Asked {
        question: "A question".into(),
        who: "test".into(),
        at: 1,
        answer: (0..60).map(|i| format!("answer {i:02}")).collect::<Vec<_>>().join("\n"),
        ..Default::default()
    });
    app.dialog = Some(Dialog::Ask { draft: Default::default(), route: None, scroll: 0 });
    draw(&mut app, 60, 15);
    for _ in 0..100 {
        wheel(&mut app, false);
    }
    let first = draw(&mut app, 60, 15);
    assert!(first.join("\n").contains("A question"));
    wheel(&mut app, true);
    assert_ne!(first, draw(&mut app, 60, 15), "the first reverse step moves after overscrolling");
    app.dialog = Some(Dialog::Confirm { text: "A long confirmation. ".repeat(80), action: Action::Shutdown });
    let first = draw(&mut app, 60, 15);
    rx.try_iter().for_each(drop);
    for _ in 0..100 {
        wheel(&mut app, true);
    }
    let last = draw(&mut app, 60, 15);
    assert_ne!(first, last);
    assert!(matches!(app.dialog, Some(Dialog::Confirm { .. })));
    assert!(rx.try_iter().next().is_none() && !app.quit);
}

#[test]
fn a_panel_skips_headings_and_text_and_answers_its_keys() {
    let dir = crate::testsupport::TempDir::new("panel-picker");
    let (mut app, _) = tests::app_with(vec![], vec![]);
    app.load_lua(
        crate::lua::Places { config_dir: dir.path().join("config"), start_dir: dir.path().into() },
        std::sync::Arc::new(|_| {}),
    );
    let open = |app: &mut App| {
        app.lua_eval(
            r#"deck.ui.pick({title = "Panel", filter = false, keys = {{key = "r", label = "refresh"}},
            items = {{"Group", heading = true}, {"read only", inert = true}, "first", {"second", right = "›"},
                {"tail", inert = true}},
            on_choose = function(item, index, key) chosen = tostring(item) .. "|" .. tostring(index) .. "|" .. tostring(key) end,
            on_close = function() closed = true end})"#,
        );
    };
    let eval = |app: &mut App, code: &str| app.lua.as_mut().unwrap().eval(code);
    open(&mut app);
    let selected = |app: &App| match &app.dialog {
        Some(Dialog::Choose { pick, sel, .. }) => crate::lua::styled_text(&pick.items[*sel].text),
        _ => panic!("the panel is not open"),
    };
    assert_eq!(selected(&app), "first", "the heading and text rows are not selected");
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Down);
    assert_eq!(selected(&app), "second", "the trailing text row is not selected");
    key(&mut app, KeyCode::Up);
    key(&mut app, KeyCode::Up);
    assert_eq!(selected(&app), "first");
    key(&mut app, KeyCode::Char('x'));
    key(&mut app, KeyCode::Char('j'));
    assert!(matches!(&app.dialog, Some(Dialog::Choose { filter, .. }) if filter.is_empty()), "letters never filter");
    assert_eq!(selected(&app), "second");
    key(&mut app, KeyCode::Enter);
    assert_eq!(eval(&mut app, "=chosen"), Some("second  ›|4|nil".into()));
    open(&mut app);
    key(&mut app, KeyCode::Char('r'));
    assert_eq!(eval(&mut app, "=chosen"), Some("first|3|r".into()));
    assert_eq!(eval(&mut app, "=tostring(closed)"), Some("nil".into()), "a choice is not a close");
    open(&mut app);
    key(&mut app, KeyCode::Char('/'));
    app.handle_input(Event::Paste("second".into()));
    let results = draw(&mut app, 80, 20).join("\n");
    assert!(results.contains("/ second") && !results.contains("read only"), "{results}");
    open(&mut app);
    let refreshed = draw(&mut app, 80, 20).join("\n");
    assert!(refreshed.contains("/ second") && !refreshed.contains("read only"), "{refreshed}");
    key(&mut app, KeyCode::Enter);
    assert_eq!(eval(&mut app, "=chosen"), Some("second  ›|4|nil".into()));
    open(&mut app);
    key(&mut app, KeyCode::Esc);
    assert!(app.dialog.is_none());
    assert_eq!(eval(&mut app, "=tostring(closed)"), Some("true".into()));
}

#[test]
fn the_wheel_scrolls_text_by_rows_and_lists_by_entries_with_gesture_gain() {
    let (mut app, rx) = tests::app_with(vec![], vec![]);
    app.dialog = Some(Dialog::Help);
    draw(&mut app, 60, 15);
    assert!(app.dialog_viewport.limit > 9 * scroll::WHEEL_ROWS, "Help is long enough to measure gain");
    app.wheel_scroll.reset();
    wheel(&mut app, true);
    assert_eq!(app.dialog_viewport.offset, scroll::WHEEL_ROWS, "one isolated report moves three rows");
    app.wheel_scroll.reset();
    let before = app.dialog_viewport.offset;
    for _ in 0..8 {
        wheel(&mut app, true);
    }
    let travelled = app.dialog_viewport.offset - before;
    assert!(travelled > 8 * scroll::WHEEL_ROWS && travelled <= 8 * 4 * scroll::WHEEL_ROWS, "{travelled}");
    let items = (0..40)
        .map(|i| {
            crate::lua::PickItem::choice(vec![crate::lua::Segment {
                text: format!("choice {i:02}"),
                style: String::new(),
            }])
        })
        .collect();
    app.dialog = Some(Dialog::Choose {
        pick: Box::new(crate::lua::Pick::new("Choose", items)),
        sel: 0,
        filter: String::new(),
        callback: 0,
    });
    draw(&mut app, 60, 15);
    app.wheel_scroll.reset();
    wheel(&mut app, true);
    assert!(matches!(app.dialog, Some(Dialog::Choose { sel: 1, .. })), "one report moves one entry");
    assert!(rx.try_iter().next().is_none());
}

#[test]
fn menus_reach_their_last_entry_and_keep_the_selection_visible_at_every_size() {
    let sizes = [(30, 8), (40, 10), (60, 12), (80, 8), (147, 38)];
    for (width, height) in sizes {
        let (mut app, _) = tests::app_with(vec![], vec![]);
        app.dialog = Some(Dialog::Help);
        draw(&mut app, width, height);
        for _ in 0..400 {
            wheel(&mut app, true);
        }
        let last = app.command_help().last().cloned().unwrap();
        let word = last.split_whitespace().last().unwrap().trim_matches(['(', ')']).to_string();
        let tail = word.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<String>();
        let end = draw(&mut app, width, height).join("\n");
        assert!(end.contains(&tail), "Help's last row ({last}) is reachable at {width}x{height}: {end}");
        let items = (0..40)
            .map(|i| {
                crate::lua::PickItem::choice(vec![crate::lua::Segment {
                    text: format!("c{i:02}"),
                    style: String::new(),
                }])
            })
            .collect();
        app.dialog = Some(Dialog::Choose {
            pick: Box::new(crate::lua::Pick::new("Choose", items)),
            sel: 0,
            filter: String::new(),
            callback: 0,
        });
        draw(&mut app, width, height);
        for step in 0..45 {
            app.wheel_scroll.reset();
            wheel(&mut app, step < 42);
            let Some(Dialog::Choose { sel, .. }) = &app.dialog else { panic!("the menu stays open") };
            let selected = format!("c{sel:02}");
            let rows = draw(&mut app, width, height).join("\n");
            assert!(rows.contains(&selected), "{selected} stays visible at {width}x{height}: {rows}");
        }
    }
}
