use super::*;
use crate::tui::tests::{app_with, press, session};
use serde_json::json;

fn command(app: &mut App, text: &str) {
    press(app, KeyCode::Char(':'), KeyModifiers::NONE);
    for c in text.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

#[test]
fn bulk_close_resolves_visible_numbers_before_removing_rows_and_deduplicates() {
    let ids = [101, 15, 72, 38, 9, 64, 27, 83, 46, 5];
    for (text, positions) in [
        ("close 7-10", vec![7, 8, 9, 10]),
        ("close 1,3,5", vec![1, 3, 5]),
        ("close #1, 3 - 5,4, #9-#10", vec![1, 3, 4, 5, 9, 10]),
    ] {
        let sessions = ids.iter().rev().map(|id| session(*id, Tool::Codex, Status::Idle, true)).collect();
        let (mut app, rx) = app_with(sessions, vec![]);
        app.order = ids.to_vec();
        command(&mut app, text);
        let actual: Vec<_> = rx.try_iter().map(|msg| serde_json::to_value(msg).unwrap()).collect();
        let expected: Vec<_> = positions.iter().map(|n| json!({"t": "close", "id": ids[n - 1]})).collect();
        assert_eq!(actual, expected, "{text}");
        let remaining: Vec<_> =
            ids.iter().enumerate().filter(|(n, _)| !positions.contains(&(n + 1))).map(|(_, id)| *id).collect();
        assert_eq!(app.ordered_sessions().iter().map(|s| s.id).collect::<Vec<_>>(), remaining, "{text}");
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.dialog.is_none());
    }
}

#[test]
fn bulk_close_uses_close_or_forget_for_local_and_remote_sessions() {
    let local_live = session(501, Tool::Codex, Status::Idle, true);
    let local_ended = session(23, Tool::Codex, Status::Ended, false);
    let mut remote_live = session(9, Tool::Codex, Status::Idle, true);
    remote_live.host = Some("remote".into());
    let mut remote_ended = session(700, Tool::Codex, Status::Ended, false);
    remote_ended.host = Some("remote".into());
    let sessions = vec![remote_ended, local_ended, remote_live, local_live];
    let (mut app, rx) = app_with(sessions.clone(), vec![]);
    command(&mut app, "close 1-2");
    for id in [23, 700] {
        app.current = Some(crate::tui::Sel::Session(id));
        app.run_hotkey(Hotkey::Close);
    }
    let actual: Vec<_> = rx.try_iter().map(|msg| serde_json::to_value(msg).unwrap()).collect();
    assert_eq!(
        actual,
        vec![
            json!({"t": "close", "id": 9}),
            json!({"t": "close", "id": 501}),
            json!({"t": "forget", "id": 23}),
            json!({"t": "forget", "id": 700}),
        ]
    );
    assert!(app.state.sessions.is_empty());
    app.handle_server(ServerMsg::State(crate::protocol::DeckState { sessions, ..app.state.clone() }));
    assert!(app.state.sessions.is_empty());
    assert!(rx.try_iter().next().is_none(), "stale updates must not close anything else");
}

#[test]
fn bulk_close_rejects_the_entire_selection_before_closing_any_session() {
    for text in [
        "close 1,11",
        "close 1,0",
        "close 1,3-2",
        "close 1,,3",
        "close 1,2-",
        "close 1,foo",
        "close 1,2-18446744073709551615",
        "close 1,18446744073709551616",
        "close 1 3",
        "close ''",
    ] {
        let sessions = (1..=10).map(|id| session(id, Tool::Codex, Status::Idle, true)).collect();
        let (mut app, rx) = app_with(sessions, vec![]);
        let selected = app.current;
        command(&mut app, text);
        assert!(rx.try_iter().next().is_none(), "{text} must not close anything");
        assert_eq!(app.state.sessions.len(), 10, "{text}");
        assert_eq!(app.current, selected, "{text}");
        assert!(
            app.notice.as_ref().is_some_and(|n| !n.1 && n.0.contains("Usage: :close")),
            "{text} must explain the invalid command as a message"
        );
    }
}
