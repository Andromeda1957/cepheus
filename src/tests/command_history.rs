use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc::Receiver;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::protocol::{ClientMsg, Status, Tool};
use crate::testsupport::TempDir;
use crate::tui::tests::{app_with, press, session};
use crate::tui::Mode;

fn deck() -> (App, Receiver<ClientMsg>) {
    app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![])
}

fn keys(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE);
    }
}

/// `:`, the line, Enter: as the owner enters a command.
fn command(app: &mut App, line: &str) {
    press(app, KeyCode::Char(':'), KeyModifiers::NONE);
    keys(app, line);
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

/// The command line after each of `steps` keys.
fn lines(app: &mut App, steps: &[(KeyCode, KeyModifiers)]) -> Vec<String> {
    steps
        .iter()
        .map(|(code, mods)| {
            press(app, *code, *mods);
            app.command.to_string()
        })
        .collect()
}

const UP: (KeyCode, KeyModifiers) = (KeyCode::Up, KeyModifiers::NONE);
const DOWN: (KeyCode, KeyModifiers) = (KeyCode::Down, KeyModifiers::NONE);
const CTRL_P: (KeyCode, KeyModifiers) = (KeyCode::Char('p'), KeyModifiers::CONTROL);
const CTRL_N: (KeyCode, KeyModifiers) = (KeyCode::Char('n'), KeyModifiers::CONTROL);

fn saved(path: &Path) -> Vec<String> {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn up_and_down_step_through_entered_lines_newest_first_and_back_to_the_typed_line() {
    let (mut app, rx) = deck();
    for line in ["rename alpha", "rename beta", "", "rename beta", "rename gamma"] {
        command(&mut app, line);
    }
    // A command a key or a script runs is not one the owner entered.
    app.run_command("rename delta");
    press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
    keys(&mut app, "dra");
    assert_eq!(
        lines(&mut app, &[UP, UP, CTRL_P, UP]),
        ["rename gamma", "rename beta", "rename alpha", "rename alpha"],
        "newest first, without the empty line or the repeat, stopping at the oldest"
    );
    assert_eq!(
        lines(&mut app, &[CTRL_N, DOWN, DOWN, DOWN]),
        ["rename beta", "rename gamma", "dra", "dra"],
        "past the newest comes the line being typed"
    );
    assert_eq!(app.mode, Mode::Command);
    // A recalled line runs as edited, and becomes the newest.
    assert_eq!(lines(&mut app, &[CTRL_P]), ["rename gamma"]);
    for _ in 0.."gamma".len() {
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    keys(&mut app, "omega");
    rx.try_iter().count();
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let ran: Vec<_> = rx.try_iter().map(|m| serde_json::to_value(m).unwrap()).collect();
    assert_eq!(ran, vec![serde_json::json!({"t": "rename", "id": 1, "name": "omega"})]);
    press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
    assert_eq!(lines(&mut app, &[UP, UP]), ["rename omega", "rename gamma"], "each `:` browses from the newest");
}

#[test]
fn history_is_kept_privately_for_the_next_deck_and_a_corrupt_file_reads_as_empty() {
    let dir = TempDir::new("command-history");
    let file = dir.path().join("state/command-history.json");
    let (mut first, _rx) = deck();
    first.command_history = CommandHistory::load(file.clone());
    command(&mut first, "rename alpha");
    command(&mut first, "rename beta");
    assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);

    let (mut next, _rx) = deck();
    next.command_history = CommandHistory::load(file.clone());
    press(&mut next, KeyCode::Char(':'), KeyModifiers::NONE);
    assert_eq!(lines(&mut next, &[UP, UP]), ["rename beta", "rename alpha"]);
    press(&mut next, KeyCode::Esc, KeyModifiers::NONE);
    // Two decks open at once both keep their lines.
    command(&mut first, "rename gamma");
    command(&mut next, "rename delta");
    assert_eq!(saved(&file), ["rename alpha", "rename beta", "rename gamma", "rename delta"]);

    fs::write(&file, b"{not a history").unwrap();
    let (mut fresh, _rx) = deck();
    fresh.command_history = CommandHistory::load(file.clone());
    press(&mut fresh, KeyCode::Char(':'), KeyModifiers::NONE);
    keys(&mut fresh, "x");
    assert_eq!(lines(&mut fresh, &[UP]), ["x"]);
    press(&mut fresh, KeyCode::Esc, KeyModifiers::NONE);
    command(&mut fresh, "rename epsilon");
    assert!(fresh.local_errors.is_empty(), "{:?}", fresh.local_errors);
    assert_eq!(saved(&file), ["rename epsilon"]);
}

#[test]
fn the_configured_size_keeps_only_the_newest_lines_and_zero_keeps_none() {
    let dir = TempDir::new("command-history-size");
    let file = dir.path().join("command-history.json");
    let (mut app, _rx) = deck();
    app.cfg.command_history = 2;
    app.command_history = CommandHistory::load(file.clone());
    for name in ["alpha", "beta", "gamma"] {
        command(&mut app, &format!("rename {name}"));
    }
    assert_eq!(saved(&file), ["rename beta", "rename gamma"]);
    press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
    assert_eq!(lines(&mut app, &[UP, UP, UP]), ["rename gamma", "rename beta", "rename beta"]);

    let (mut off, _rx) = deck();
    off.cfg.command_history = 0;
    off.command_history = CommandHistory::load(file.clone());
    press(&mut off, KeyCode::Char(':'), KeyModifiers::NONE);
    assert_eq!(lines(&mut off, &[UP, CTRL_P]), ["", ""], "0 recalls none of the lines on file");
    press(&mut off, KeyCode::Esc, KeyModifiers::NONE);
    command(&mut off, "rename delta");
    assert_eq!(saved(&file), ["rename beta", "rename gamma"], "0 keeps none");
}

#[test]
fn tab_completion_keeps_its_keys_beside_the_history() {
    const TAB: (KeyCode, KeyModifiers) = (KeyCode::Tab, KeyModifiers::NONE);
    const BACK_TAB: (KeyCode, KeyModifiers) = (KeyCode::BackTab, KeyModifiers::SHIFT);
    let (mut app, _rx) = deck();
    command(&mut app, "rename alpha");
    press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
    keys(&mut app, "re");
    press(&mut app, TAB.0, TAB.1);
    let items = app.completion.as_ref().map(|c| c.items.clone()).unwrap_or_default();
    assert!(items.len() > 1, "{items:?}");
    assert_eq!(app.command, "re");
    assert_eq!(lines(&mut app, &[TAB, TAB, BACK_TAB]), [items[0].clone(), items[1].clone(), items[0].clone()]);
    // Any other key closes the list, as before; ↑ then recalls.
    assert_eq!(lines(&mut app, &[UP]), ["rename alpha"]);
    assert_eq!(app.completion, None);
}
