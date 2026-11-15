use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use crate::protocol::{Status, Tool};
use crate::tui::tests::{app_with, press, session};
use crate::tui::App;

fn keys(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE);
    }
}

fn key(app: &mut App, code: KeyCode) {
    press(app, code, KeyModifiers::NONE);
}

/// The footer row as the deck draws it.
fn footer(app: &mut App) -> String {
    let mut term = Terminal::new(TestBackend::new(100, 10)).unwrap();
    term.draw(|f| {
        crate::view::render(f, app);
    })
    .unwrap();
    let buf = term.backend().buffer();
    (0..100).map(|x| buf[(x, 9)].symbol()).collect()
}

/// The names the service was asked to give session 1.
fn renames(rx: &std::sync::mpsc::Receiver<crate::protocol::ClientMsg>) -> Vec<serde_json::Value> {
    rx.try_iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .filter(|m| m["t"] == "rename")
        .map(|m| m["name"].clone())
        .collect()
}

/// Owner, 2026-11-11: "when i use : command in cepheus i cant use my arrow
/// keys to edit the command".
#[test]
fn arrow_keys_move_the_cursor_and_edits_land_there() {
    let (mut app, rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    key(&mut app, KeyCode::Char(':'));
    keys(&mut app, "renam alpa");
    key(&mut app, KeyCode::Left);
    keys(&mut app, "h");
    assert_eq!(app.command, "renam alpha");
    assert!(footer(&mut app).contains(":renam alph▏a "), "the caret is drawn where typing goes");
    key(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Right, KeyModifiers::CONTROL);
    keys(&mut app, "e");
    key(&mut app, KeyCode::End);
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char(':'));
    keys(&mut app, "rename betta");
    press(&mut app, KeyCode::Left, KeyModifiers::CONTROL);
    for _ in 0..3 {
        key(&mut app, KeyCode::Right);
    }
    key(&mut app, KeyCode::Delete);
    key(&mut app, KeyCode::Enter);
    assert_eq!(renames(&rx), ["alpha", "beta"], "Enter runs the whole edited line wherever the cursor is");
}
