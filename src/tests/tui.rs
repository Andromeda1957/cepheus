use super::*;
use crate::protocol::Usage;

pub(crate) fn session(id: u64, tool: Tool, status: Status, alive: bool) -> SessionInfo {
    SessionInfo {
        spawned_ms: 1000 + id,
        account_root: None,
        metadata: Default::default(),
        id,
        tool,
        title: format!("task {id}"),
        status,
        detail: if alive { String::new() } else { "exited".into() },
        pid: alive.then_some(1000 + id as u32),
        conversation: Some(format!("conv-{id}")),
        worktree: None,
        containers: Vec::new(),
        review: false,
        alive,
        has_screen: alive,
        created_at: 0,
        status_since: 0,
        host: None,
        subagents: Vec::new(),
        activity: None,
        last_active: 0,
        cwd: None,
        job: None,
    }
}

pub(crate) fn outside(pid: u32, conversation: Option<&str>) -> OutsideInfo {
    OutsideInfo {
        tool: Tool::Claude,
        pid,
        title: "outside work".into(),
        busy: Some(false),
        cwd: "/srv/repo".into(),
        conversation: conversation.map(str::to_string),
        started_at: 1000,
        start_ticks: 5,
        kitty_pid: Some(77),
        kitty_window: Some("3".into()),
        kitty_socket: true,
        workspace: Some("2".into()),
        worktree: None,
        host: None,
    }
}

pub(crate) fn app_with(sessions: Vec<SessionInfo>, outside: Vec<OutsideInfo>) -> (App, Receiver<ClientMsg>) {
    configured_app(Config::default(), sessions, outside)
}

fn configured_app(cfg: Config, sessions: Vec<SessionInfo>, outside: Vec<OutsideInfo>) -> (App, Receiver<ClientMsg>) {
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(tx, cfg);
    app.handle_server(ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: "/srv/repo".into() });
    app.handle_server(ServerMsg::State(DeckState {
        sessions,
        outside,
        usage: Usage::default(),
        start_dir: "/srv/repo".into(),
        ..DeckState::default()
    }));
    (app, rx)
}

fn sent(rx: &Receiver<ClientMsg>) -> Vec<serde_json::Value> {
    rx.try_iter().map(|m| serde_json::to_value(m).unwrap()).collect()
}

pub(crate) fn press(app: &mut App, code: KeyCode, m: KeyModifiers) {
    app.handle_input(Event::Key(KeyEvent::new(code, m)));
}

/// A message from the service, as the deck's connection delivers it.
pub(crate) fn server(app: &mut App, msg: ServerMsg) {
    app.handle_server(msg);
}

/// Alt and a command key, as kitty reports it: an uppercase letter is the
/// lowercase key with Shift.
fn alt(app: &mut App, c: char) {
    let shift = if c.is_ascii_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
    press(app, KeyCode::Char(c.to_ascii_lowercase()), KeyModifiers::ALT | shift);
}

fn command(app: &mut App, text: &str) {
    if app.dialog.is_none() && app.mode == Mode::Insert {
        press(app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        press(app, KeyCode::Char('h'), KeyModifiers::NONE);
    }
    press(app, KeyCode::Char(':'), KeyModifiers::NONE);
    for c in text.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

#[test]
fn ordinary_desktop_send_is_native_correlated_and_success_is_silent() {
    let (mut app, rx) = app_with(vec![session(1, Tool::Codex, Status::Working, true)], vec![]);
    sent(&rx);
    let messages = app.messages.len();
    command(&mut app, "send 1 exact owner message");
    let sent_messages = sent(&rx);
    assert_eq!(sent_messages.len(), 1);
    assert_eq!(sent_messages[0]["t"], "send_message");
    assert_eq!(sent_messages[0]["text"], "exact owner message");
    let request = sent_messages[0]["request"].as_u64().unwrap();
    app.handle_server(ServerMsg::MessageSent { id: 2, request, error: Some("wrong target".into()), delivered: None });
    assert!(app.owner_sends.contains_key(&request));
    app.handle_server(ServerMsg::MessageSent {
        id: 1,
        request,
        error: None,
        delivered: Some(crate::protocol::Delivered::Queued),
    });
    assert!(!app.owner_sends.contains_key(&request));
    assert_eq!(app.messages.len(), messages, "native success adds no acknowledgement notice");
    command(&mut app, "send 1 refused owner message");
    let request = sent(&rx)[0]["request"].as_u64().unwrap();
    app.handle_server(ServerMsg::MessageSent { id: 1, request, error: Some("native refusal".into()), delivered: None });
    assert!(app.local_errors.iter().any(|entry| entry.text == "native refusal"));
    app.service_version = 18;
    command(&mut app, "send 1 never downgrade");
    command(&mut app, "broadcast never downgrade");
    assert!(sent(&rx).is_empty());
}

#[test]
fn direct_program_commands_start_sessions_without_a_chooser() {
    let mut failures = Vec::new();
    for (name, tool) in [
        ("claude", "claude"),
        ("codex", "codex"),
        ("agy", "antigravity"),
        ("antigravity", "antigravity"),
        ("grok", "grok"),
        ("opencode", "opencode"),
        ("term", "shell"),
    ] {
        let (mut app, rx) = app_with(vec![], vec![]);
        app.service_version = PROTOCOL_VERSION;
        command(&mut app, name);
        let expected = vec![serde_json::json!({"t": "create", "request": 0, "tool": tool})];
        let actual = sent(&rx);
        if actual != expected || app.dialog.is_some() {
            failures.push(format!(":{name}: {actual:?}, notice={:?}", app.notice));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    let (mut app, rx) = app_with(vec![], vec![]);
    app.service_version = 14;
    app.handle_server(ServerMsg::Hosts { hosts: vec![host("fedora", "Fedora", true)] });
    command(&mut app, "claude fedora --cwd '/srv/my project' --name 'Review UI' --model sonnet");
    assert_eq!(
        sent(&rx),
        vec![serde_json::json!({
            "t": "create", "request": 0, "tool": "claude", "host": "fedora", "cwd": "/srv/my project",
            "name": "Review UI", "model": "sonnet"
        })]
    );
    command(&mut app, "term fedora --cwd '/srv/my project'");
    assert_eq!(
        sent(&rx),
        vec![serde_json::json!({
            "t": "create", "request": 0, "tool": "shell", "host": "fedora", "cwd": "/srv/my project"
        })]
    );
    command(&mut app, "claude here --cwd child");
    assert_eq!(sent(&rx)[0]["host"], serde_json::Value::Null, "here is this machine");
    app.start_session(Tool::Claude, None, None, None, Vec::new(), None);
    assert_eq!(sent(&rx)[0]["host"], serde_json::Value::Null, "Lua's omitted host is this machine too");
    app.state.branches = vec![crate::protocol::BranchInfo {
        branch: "feature".into(),
        path: "/srv/checked-out".into(),
        ..Default::default()
    }];
    app.open_branches();
    press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    let created = sent(&rx);
    assert_eq!(created[0]["host"], serde_json::Value::Null, "a branch launch stays with its checkout");
    assert_eq!(created[0]["cwd"], "/srv/checked-out");
}

#[test]
fn first_state_selects_a_session_that_needs_you() {
    let (app, _rx) = app_with(
        vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Codex, Status::NeedsYou, true)],
        vec![],
    );
    assert_eq!(app.current, Some(Sel::Session(2)));
    let (app, _rx) = app_with(
        vec![session(4, Tool::Claude, Status::Ended, false), session(5, Tool::Claude, Status::Idle, true)],
        vec![],
    );
    assert_eq!(app.current, Some(Sel::Session(5)));
    let (app, _rx) = app_with(vec![], vec![outside(9, None)]);
    assert_eq!(app.current, None);
}

#[test]
fn selection_survives_updates_and_falls_back_when_removed() {
    let (mut app, _rx) = app_with(
        vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Claude, Status::Idle, true)],
        vec![],
    );
    app.select(Sel::Session(2));
    let state = DeckState {
        processes: Vec::new(),
        sessions: vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Claude, Status::Working, true)],
        ..app.state.clone()
    };
    app.handle_server(ServerMsg::State(state));
    assert_eq!(app.current, Some(Sel::Session(2)));
    let state = DeckState {
        processes: Vec::new(),
        sessions: vec![session(1, Tool::Claude, Status::NeedsYou, true)],
        ..app.state.clone()
    };
    app.handle_server(ServerMsg::State(state));
    assert_eq!(app.current, Some(Sel::Session(1)));
}

#[test]
fn super_up_and_down_move_the_selected_session_and_a_new_deck_keeps_the_order() {
    // Owner, 2026-10-26: "make it so super+up/down arrows moves the sessions
    // in the list up or down".
    use Sel::Session as S;
    let dir = crate::testsupport::TempDir::new("order");
    let file = dir.path().join("state/order.json");
    let sessions = vec![
        session(1, Tool::Claude, Status::Idle, true),
        session(2, Tool::Codex, Status::Idle, true),
        session(3, Tool::Claude, Status::Idle, true),
        session(4, Tool::Claude, Status::Ended, false),
    ];
    let (mut app, rx) = app_with(sessions.clone(), vec![outside(9, None)]);
    app.service_version = 12;
    app.use_order_file(file.clone());
    app.select(S(3));
    let up = |app: &mut App| press(app, KeyCode::Up, KeyModifiers::SUPER);
    let down = |app: &mut App| press(app, KeyCode::Down, KeyModifiers::SUPER);
    up(&mut app);
    assert_eq!(app.rows(), [S(1), S(3), S(2)]);
    assert_eq!(app.current, Some(S(3)), "the highlight moves with it");
    up(&mut app);
    up(&mut app);
    assert_eq!(app.rows(), [S(3), S(1), S(2)], "the first stays first");
    // Typing into it, Super+↓ still moves it and the program gets nothing.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Insert);
    sent(&rx);
    down(&mut app);
    assert_eq!(app.rows(), [S(1), S(3), S(2)]);
    assert_eq!(app.mode, Mode::Insert);
    assert!(sent(&rx).iter().all(|m| m["t"] != "input"));
    // Moving at the last visible session stops at the list's end.
    app.select(S(2));
    down(&mut app);
    assert_eq!(app.rows(), [S(1), S(3), S(2)]);
    // A plain ↑ still only moves the highlight.
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.current, Some(S(3)));
    assert_eq!(app.rows(), [S(1), S(3), S(2)]);
    // Number keys follow the order on screen.
    press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
    assert_eq!(app.current, Some(S(3)));

    // The next deck reads the order back; a new session goes below the live
    // ones it knows.
    let (mut again, _rx) = app_with(sessions.clone(), vec![]);
    again.service_version = 12;
    again.use_order_file(file.clone());
    assert_eq!(again.rows(), [S(1), S(3), S(2)]);
    let mut state = again.state.clone();
    state.sessions.push(session(5, Tool::Codex, Status::Working, true));
    again.handle_server(ServerMsg::State(state));
    assert_eq!(again.rows(), [S(1), S(3), S(2), S(5)]);
    // A missing or unreadable file is the plain order.
    std::fs::write(&file, "not json").unwrap();
    let (mut fresh, _rx) = app_with(sessions, vec![]);
    fresh.service_version = 12;
    fresh.use_order_file(file);
    assert_eq!(fresh.rows(), [S(1), S(2), S(3)]);
}

/// Owner, 2026-10-27: "the sessions grouping should reflect the deck ...
/// allowing me to hold drag and reorder them". The phone's gateway writes the
/// deck's order file; a running deck follows it.
#[test]
fn the_deck_follows_an_order_the_phone_saved() {
    use Sel::Session as S;
    let dir = crate::testsupport::TempDir::new("order-watch");
    let file = dir.path().join("state/order.json");
    let sessions = vec![
        session(1, Tool::Claude, Status::Idle, true),
        session(2, Tool::Codex, Status::Idle, true),
        session(3, Tool::Claude, Status::Idle, true),
    ];
    let (mut app, _rx) = app_with(sessions, vec![]);
    app.service_version = 12;
    app.use_order_file(file.clone());
    app.select(S(3));
    press(&mut app, KeyCode::Up, KeyModifiers::SUPER);
    assert_eq!(app.rows(), [S(1), S(3), S(2)]);
    app.watch_order();
    assert_eq!(app.rows(), [S(1), S(3), S(2)], "its own save changes nothing");

    crate::util::write_atomic(&file, b"[2,1,3]").unwrap();
    app.dirty = false;
    app.watch_order();
    assert_eq!(app.rows(), [S(2), S(1), S(3)]);
    assert!(app.dirty, "the list is drawn again");
    // A broken file leaves the order as it was.
    crate::util::write_atomic(&file, b"[2,").unwrap();
    app.watch_order();
    assert_eq!(app.rows(), [S(2), S(1), S(3)]);
}

fn ctrl(app: &mut App, c: char) {
    press(app, KeyCode::Char(c), KeyModifiers::CONTROL);
}

#[test]
fn enter_types_into_the_session_and_ctrl_w_w_gives_the_keys_back() {
    let (mut app, rx) = app_with(
        vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Codex, Status::Idle, true)],
        vec![],
    );
    app.select(Sel::Session(1));
    assert_eq!(app.mode, Mode::Normal, "the deck starts with the keys");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Session(2)), "arrows pick a session");
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    // Owner, 2026-11-13: "sometimes just pressing an arrow key enters insert
    // mode in cepheus ... should be either hitting the enter key or ctrl + w + l".
    for code in [KeyCode::Right, KeyCode::Left, KeyCode::Char('i')] {
        press(&mut app, code, KeyModifiers::NONE);
        assert_eq!(app.mode, Mode::Normal, "{code:?} on a live session keeps the keys with the deck");
    }
    assert!(sent(&rx).is_empty(), "nothing is typed while picking");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Insert);
    press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    press(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    assert_eq!(app.current, Some(Sel::Session(1)), "arrows belong to the program while typing");
    assert_eq!(app.mode, Mode::Insert, "Esc stays with the program");
    let got = sent(&rx);
    let data: Vec<&str> = got.iter().map(|m| m["data"].as_str().unwrap()).collect();
    assert_eq!(data, vec!["h", "\x1b[13;2u", "\x1b[B", " ", "\x03", "\x1b", "\x1b[1;2D"]);
    assert!(got.iter().all(|m| m["t"] == "input" && m["id"] == 1));
    ctrl(&mut app, 'w');
    press(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal);
    assert!(sent(&rx).is_empty(), "Ctrl-w w is the deck's, not the program's");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Session(2)), "after Ctrl-w w the arrows pick sessions again");
}

#[test]
fn esc_interrupts_the_agent_and_ctrl_w_moves_between_the_list_and_the_session() {
    // Owner, 2026-10-26: "escape needs to interrupt the agent, not go back to
    // slection, make it ctrl w w to focus not the agent and ctrl w l to focus
    // the active agent letting esc interrupt the agent".
    let (mut app, rx) = app_with(
        vec![session(1, Tool::Codex, Status::Working, true), session(2, Tool::Claude, Status::Ended, false)],
        vec![],
    );
    app.select(Sel::Session(1));
    let input = |data: &str| serde_json::json!({"t": "input", "id": 1, "data": data});
    // Normal mode: Ctrl-w l, like Neovim's window to the right, types into it.
    ctrl(&mut app, 'w');
    press(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Insert);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(sent(&rx), vec![input("\x1b")], "Esc interrupts the agent");
    assert_eq!(app.mode, Mode::Insert);
    // With Ctrl held for both keys, and with the arrows.
    ctrl(&mut app, 'w');
    ctrl(&mut app, 'w');
    assert_eq!(app.mode, Mode::Normal, "Ctrl-w Ctrl-w is Ctrl-w w");
    ctrl(&mut app, 'w');
    press(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal, "from the list, Ctrl-w w stays on the list; only Ctrl-w l types");
    ctrl(&mut app, 'w');
    ctrl(&mut app, 'w');
    assert_eq!(app.mode, Mode::Normal);
    ctrl(&mut app, 'w');
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal);
    ctrl(&mut app, 'w');
    press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal, "Ctrl-w h on the list stays there");
    ctrl(&mut app, 'w');
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Insert);
    assert!(sent(&rx).is_empty());
    // Ctrl-w and any other key are the program's: Claude and Codex delete a
    // word with Ctrl-w.
    ctrl(&mut app, 'w');
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    assert_eq!(sent(&rx), vec![input("\x17"), input("a")]);
    // A lone Ctrl-w reaches the program once the wait for a second key ends.
    ctrl(&mut app, 'w');
    assert!(sent(&rx).is_empty());
    app.expire_window_key(Instant::now());
    assert!(sent(&rx).is_empty(), "the second key may still come");
    app.expire_window_key(Instant::now() + Duration::from_secs(2));
    assert_eq!(sent(&rx), vec![input("\x17")]);
    assert_eq!(app.mode, Mode::Insert);
    // Arrows skip hidden sessions. A stale ended selection cannot take input.
    ctrl(&mut app, 'w');
    ctrl(&mut app, 'w');
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Session(1)));
    app.select(Sel::Session(2));
    ctrl(&mut app, 'w');
    press(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.notice.as_ref().is_some_and(|n| n.0.contains("Enter reopens")), "{:?}", app.notice);
    assert!(sent(&rx).is_empty());
}

#[test]
fn alt_letters_reach_the_session_and_colon_actions_work_after_returning_to_the_list() {
    let (mut app, rx) = app_with(vec![session(1, Tool::Codex, Status::Idle, true)], vec![]);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    for c in ['b', 'd', 'x', 'n', 'q', 'R', ';'] {
        press(&mut app, KeyCode::Char(c), KeyModifiers::ALT);
    }
    let input = sent(&rx);
    assert_eq!(input.len(), 7);
    assert!(input.iter().all(|m| m["t"] == "input"));
    assert_eq!(app.mode, Mode::Insert);
    assert!(app.dialog.is_none() && !app.quit && app.state.sessions.len() == 1);
    command(&mut app, "close");
    assert!(app.dialog.is_none());
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "close", "id": 1})]);
    command(&mut app, "stop");
    assert!(matches!(app.dialog, Some(Dialog::Confirm { action: Action::Shutdown, .. })));
}

#[test]
fn hotkeys_come_from_the_config_file() {
    // Owner, 2026-10-26: "make it so the hotkey changes dont need a rebuild.
    // make it where it uses a config file".
    let cfg: Config = serde_json::from_str(r#"{"keys": {"errors": "f3", "close": "alt+c"}}"#).unwrap();
    let (mut app, rx) = configured_app(cfg, vec![session(1, Tool::Codex, Status::Idle, true)], vec![]);
    press(&mut app, KeyCode::F(3), KeyModifiers::NONE);
    assert!(matches!(app.dialog, Some(Dialog::Errors { .. })), "F3 lists the errors");
    app.dialog = None;
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    assert!(app.dialog.is_none(), "F2 is no longer the errors key");
    // Typing into the session, Alt+x is the program's again and Alt+c closes.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('x'), KeyModifiers::ALT);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::ALT);
    assert_eq!(
        sent(&rx),
        vec![serde_json::json!({"t": "input", "id": 1, "data": "\x1bx"}), serde_json::json!({"t": "close", "id": 1})]
    );
}

#[test]
fn a_running_deck_takes_changed_keys_from_the_config_file() {
    let dir = crate::testsupport::TempDir::new("deck-keys");
    let path = dir.write("config.json", r#"{"sidebar_width": 40}"#);
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    app.follow_config(path.clone());
    app.check_config();
    assert!(app.notice.is_none(), "an unchanged file changes nothing");
    std::fs::write(&path, r#"{"sidebar_width": 40, "keys": {"sidebar": "alt+b"}}"#).unwrap();
    app.check_config();
    assert!(app.notice.as_ref().is_some_and(|n| n.0.contains("Keys updated") && !n.1), "{:?}", app.notice);
    press(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
    assert!(!app.sidebar, "Alt+b hides the sidebar now");
    press(&mut app, KeyCode::Char('e'), KeyModifiers::ALT);
    assert!(!app.sidebar, "Alt+e no longer does");
    // A half-written file keeps the keys the deck has and says why.
    std::fs::write(&path, r#"{"keys": {"sidebar": "#).unwrap();
    app.check_config();
    assert!(
        app.notice.as_ref().is_some_and(|n| n.1 && n.0.contains("keys and plugins stay as they were")),
        "{:?}",
        app.notice
    );
    press(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
    assert!(app.sidebar);
    // A key that does not parse is reported, and its hotkey keeps the default.
    std::fs::write(&path, r#"{"keys": {"sidebar": "alt+banana"}}"#).unwrap();
    app.check_config();
    assert!(app.notice.as_ref().is_some_and(|n| n.1 && n.0.contains("keys.sidebar")), "{:?}", app.notice);
    press(&mut app, KeyCode::Char('e'), KeyModifiers::ALT);
    assert!(app.sidebar, "an invalid binding returns to the unbound default");
    command(&mut app, "sidebar");
    assert!(!app.sidebar);
}

#[test]
fn a_rebound_window_key_types_itself_when_no_window_key_follows() {
    let cfg: Config = serde_json::from_str(r#"{"keys": {"window": "ctrl+g"}}"#).unwrap();
    let (mut app, rx) = configured_app(cfg, vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    let input = |data: &str| serde_json::json!({"t": "input", "id": 1, "data": data});
    assert_eq!(sent(&rx), vec![input("\x17"), input("\x07"), input("a")], "Ctrl-w is the program's now");
    assert_eq!(app.mode, Mode::Insert);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(app.mode, Mode::Normal, "Ctrl-g twice goes to the list");
    assert!(sent(&rx).is_empty());
}

#[test]
fn colon_q_quits_the_view_and_other_commands_are_refused() {
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    command(&mut app, "wq");
    assert!(app.quit);
    let (mut app, _rx) = app_with(vec![], vec![]);
    command(&mut app, "frobnicate now");
    assert!(!app.quit);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.notice.as_ref().is_some_and(|n| n.0.contains("Not a deck command: :frobnicate")), "{:?}", app.notice);
    press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal, "backspace on an empty command line leaves it");
}

#[test]
fn a_new_selection_never_inherits_insert_mode() {
    let (mut app, rx) = app_with(
        vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Codex, Status::Idle, true)],
        vec![],
    );
    app.select(Sel::Session(1));
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    app.select(Sel::Session(2));
    assert_eq!(app.mode, Mode::Normal);
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert!(sent(&rx).is_empty());
    // The typed-into session ending hands the keys back too.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let state = DeckState {
        processes: Vec::new(),
        sessions: vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Codex, Status::Ended, false)],
        ..app.state.clone()
    };
    app.handle_server(ServerMsg::State(state));
    assert_eq!(app.mode, Mode::Normal);
    press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
    assert!(sent(&rx).is_empty() && !app.quit);
}

// Owner, 2026-11-04: ":codex ... the new one should open right under the
// session im on, not on the bottom, same with claude"; "when i open a new
// session, it shouldnt automatically put me in insert mode".
#[test]
fn a_new_session_opens_right_under_the_highlighted_one_in_normal_mode() {
    for (tool, word) in [(Tool::Codex, "codex"), (Tool::Claude, "claude")] {
        let (mut app, rx) = app_with(
            vec![
                session(1, Tool::Claude, Status::Idle, true),
                session(2, Tool::Codex, Status::Idle, true),
                session(3, Tool::Codex, Status::Idle, true),
            ],
            vec![],
        );
        app.select(Sel::Session(1));
        sent(&rx);
        command(&mut app, word);
        let create = sent(&rx);
        assert!(create.len() == 1 && create[0]["t"] == "create" && create[0]["request"] == 1, "{create:?}");
        app.handle_server(ServerMsg::Created { id: 4, request: Some(1) });
        let mut state = app.state.clone();
        state.sessions.push(session(4, tool, Status::Starting, true));
        app.handle_server(ServerMsg::State(state));
        assert_eq!(app.rows(), [Sel::Session(1), Sel::Session(4), Sel::Session(2), Sel::Session(3)], "{word}");
        assert_eq!((app.current, app.mode), (Some(Sel::Session(4)), Mode::Normal), "{word}");
        let saved = sent(&rx);
        assert!(
            saved.iter().any(|m| m["t"] == "set_order" && m["ids"] == serde_json::json!([1, 4, 2, 3])),
            "{saved:?}"
        );
    }
}

#[test]
fn an_older_service_is_named_and_never_sent_what_it_cannot_read() {
    // A message an older service cannot parse ends the connection, and the
    // owner kept running fixes that a still-running old service lacked.
    let (mut app, rx) = app_with(vec![], vec![]);
    app.handle_server(ServerMsg::Welcome { version: 1, pid: 1, start_dir: "/srv/repo".into() });
    let (text, error, _) = app.notice.clone().unwrap();
    assert!(error && text.contains("older build") && text.contains(":restart restarts it"), "{text}");
    assert_eq!(app.error_list().len(), 1, "kept for F2");
    command(&mut app, "new");
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    command(&mut app, "dev on");
    let refused = sent(&rx);
    assert!(refused.iter().all(|m| m["t"] != "create" && m["t"] != "dev_server"), "{refused:?}");
    assert!(app.notice.as_ref().unwrap().0.contains("needs the new service"));
    command(&mut app, "new");
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    let creates = sent(&rx).into_iter().filter(|m| m["t"] == "create").count();
    assert_eq!(creates, 1, "Claude still starts on the old service");
}

#[test]
fn dev_commands_and_the_dev_button_start_and_stop_the_dev_server() {
    let (mut app, rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    app.state.dev_server.command = "server 0.0.0.0:9".into();
    app.state.dev_server.port = Some(9);
    command(&mut app, "dev on");
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "dev_server", "run": true})]);
    app.state.dev_server.pid = Some(42);
    app.state.dev_server.started_at = Some(1);
    command(&mut app, "dev on");
    assert!(sent(&rx).is_empty(), "on is idempotent");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Insert);
    ctrl(&mut app, 'w');
    press(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
    command(&mut app, "dev off");
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "dev_server", "run": false})]);
    // The button at the top of the sidebar does the same.
    app.dev_hit = Some((0, 20, 31));
    app.handle_input(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 25,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "dev_server", "run": false})]);
    app.state.dev_server.pid = None;
    command(&mut app, "dev off");
    assert!(sent(&rx).is_empty(), "stopping: wait for it");
}

#[test]
fn f2_lists_errors_from_the_list_and_reaches_a_session_being_typed_into() {
    // Owner, 2026-10-26: "a f2 in the agent-desk itself for errors say for
    // example if the dev server fails to start", then "f2 blocks seeing codex
    // warnings, f2 should only trigger when a session isnt selected,
    // otherwise it goes through to the session".
    let (mut app, rx) = app_with(vec![session(1, Tool::Codex, Status::Idle, true)], vec![]);
    let err = |at: u64, text: &str| LogEntry::new(at, text);
    let mut state = app.state.clone();
    state.errors = vec![err(100, "Dev server failed (exit 1 after 1s): boom\nTraceback\nboom")];
    app.handle_server(ServerMsg::State(state.clone()));
    assert_eq!(app.unseen_errors(), 1);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Insert);
    sent(&rx);
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    assert!(app.dialog.is_none(), "typing into a session, F2 is the session's");
    let input: Vec<_> = sent(&rx).into_iter().filter(|m| m["t"] == "input").collect();
    assert_eq!(input.len(), 1, "{input:?}");
    assert_eq!(input[0]["data"], "\x1bOQ", "the program gets F2");
    assert_eq!(app.unseen_errors(), 1);
    // Back in the list, F2 opens the deck's errors and sends the program nothing.
    press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal);
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    assert!(matches!(app.dialog, Some(Dialog::Errors { scroll: 0 })));
    assert!(sent(&rx).iter().all(|m| m["t"] != "input"), "{:?}", sent(&rx));
    assert_eq!(app.unseen_errors(), 0);
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    assert!(app.dialog.is_none());
    assert_eq!(app.mode, Mode::Normal, "closing returns to the list");
    state.errors.push(err(130, "Codex #2 exited with status 1"));
    app.handle_server(ServerMsg::State(state.clone()));
    assert_eq!(app.unseen_errors(), 1);
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    sent(&rx);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(app.error_list().is_empty(), "cleared here");
    // Owner, 2026-10-26: "there should be a way to clear the error messages";
    // the service clears its copy too, so a deck opened later lists none.
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "clear_errors"})]);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.dialog.is_none());
    // A service older than protocol 4 cannot read it: only this deck clears.
    app.service_version = 3;
    state.errors.push(err(160, "Claude #3 could not start"));
    app.handle_server(ServerMsg::State(state));
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(app.error_list().is_empty());
    assert!(sent(&rx).is_empty(), "nothing an older service would choke on");
}

#[path = "creation_position.rs"]
mod creation_position;

#[test]
fn navigation_wraps_jumps_and_finds_attention() {
    let (mut app, _rx) = app_with(
        vec![
            session(1, Tool::Claude, Status::Idle, true),
            session(2, Tool::Codex, Status::Finished, true),
            session(3, Tool::Claude, Status::Ended, false),
        ],
        vec![outside(50, None)],
    );
    app.select(Sel::Session(1));
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Session(2)), "moving up from the top wraps to the last row");
    press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Session(1)));
    press(&mut app, KeyCode::Char('3'), KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Session(1)));
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Session(2)), "Tab finds the finished session");
    let state = DeckState {
        processes: Vec::new(),
        sessions: vec![session(1, Tool::Claude, Status::Idle, true)],
        ..app.state.clone()
    };
    app.handle_server(ServerMsg::State(state));
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.notice.as_ref().map(|n| n.0.as_str()), Some("Nothing needs you right now."));
}

#[test]
fn colon_close_closes_and_removes_at_once_and_ended_sessions_reopen_on_enter() {
    // Owner, 2026-10-26, with a closed session still listed as "exit 1":
    // "close closes it, alt x and i should not see that session anymore".
    // Then, with the closed session still listed as "exit 129" because the
    // service running was an older build that only ends the program: "alt-x
    // closes it = not listed anymore = gone, not i still see the session".
    let live = |id| session(id, Tool::Claude, Status::Idle, true);
    let ended = |id| session(id, Tool::Claude, Status::Ended, false);
    let (mut app, rx) = app_with(vec![live(1), ended(2), live(3)], vec![]);
    let listed = |app: &App| app.state.sessions.iter().map(|s| s.id).collect::<Vec<_>>();
    let state =
        |app: &App, sessions| ServerMsg::State(DeckState { processes: Vec::new(), sessions, ..app.state.clone() });
    app.select(Sel::Session(1));
    command(&mut app, "close");
    assert!(app.dialog.is_none(), "closing asks nothing");
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "close", "id": 1})]);
    assert_eq!(listed(&app), vec![2, 3], "gone from the list at once");
    assert!(app.current.is_some_and(|c| c != Sel::Session(1)));
    assert!(app.notice.as_ref().is_some_and(|n| n.0.contains(":resume")), "{:?}", app.notice);
    // That older service then lists it as ended: it stays gone, and the deck
    // asks the service to forget it, once.
    for _ in 0..2 {
        let msg = state(&app, vec![ended(1), ended(2), live(3)]);
        app.handle_server(msg);
    }
    assert_eq!(listed(&app), vec![2, 3]);
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "forget", "id": 1})]);
    let msg = state(&app, vec![ended(2), live(3)]);
    app.handle_server(msg);
    assert!(app.closed.is_empty(), "forgotten by the service, nothing left to hide");

    app.select(Sel::Session(2));
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "reopen", "id": 2})], "nothing is typed into an ended session");
    // An ended session goes the same way, at once.
    let msg = state(&app, vec![ended(2), live(3)]);
    app.handle_server(msg);
    app.select(Sel::Session(2));
    command(&mut app, "close");
    assert!(app.dialog.is_none());
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "forget", "id": 2})]);
    assert_eq!(listed(&app), vec![3]);
    let msg = state(&app, vec![ended(2), live(3)]);
    app.handle_server(msg);
    assert_eq!(listed(&app), vec![3], "still gone before the service catches up");
    assert!(sent(&rx).is_empty(), "forgotten once");
    // Alt+d is no longer a deck command.
    alt(&mut app, 'd');
    assert!(app.dialog.is_none() && sent(&rx).is_empty());
    // Resuming a closed session's conversation before the service dropped
    // it brings that session back.
    app.select(Sel::Session(3));
    command(&mut app, "close");
    assert!(listed(&app).is_empty());
    app.handle_server(ServerMsg::Created { id: 3, request: None });
    let msg = state(&app, vec![live(3)]);
    app.handle_server(msg);
    assert_eq!(listed(&app), vec![3]);
    assert_eq!(app.current, Some(Sel::Session(3)));
}

#[test]
fn colon_close_selects_the_previous_session_and_keeps_it_after_service_updates() {
    let sessions: Vec<_> = (1..=10).map(|id| session(id, Tool::Claude, Status::Idle, true)).collect();
    let (mut app, rx) = app_with(sessions.clone(), vec![]);
    app.select(Sel::Session(9));
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    sent(&rx);
    command(&mut app, "close");
    assert_eq!(app.current, Some(Sel::Session(8)));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "close", "id": 9})]);
    assert!(!app.rows().contains(&Sel::Session(9)));

    // A stale state followed by the service's removal must keep that choice.
    app.handle_server(ServerMsg::State(DeckState { processes: Vec::new(), sessions, ..app.state.clone() }));
    assert_eq!(app.current, Some(Sel::Session(8)));
    let state = app.state.clone();
    app.handle_server(ServerMsg::State(state));
    assert_eq!(app.current, Some(Sel::Session(8)));
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    assert!(sent(&rx).is_empty(), "closing never starts typing into the neighbor");
}

#[test]
fn colon_close_uses_visible_order() {
    for alive in [true, false] {
        let status = if alive { Status::Idle } else { Status::Ended };
        let sessions = [1, 3, 7, 9].into_iter().map(|id| session(id, Tool::Claude, status, alive)).collect();
        let (mut app, rx) = app_with(sessions, vec![]);
        app.order = vec![1, 7, 3, 9];
        app.select(Sel::Session(3));
        command(&mut app, "close");
        assert_eq!(app.current, alive.then_some(Sel::Session(7)));
        let action = if alive { "close" } else { "forget" };
        assert_eq!(sent(&rx), vec![serde_json::json!({"t": action, "id": 3})]);
    }
}

#[test]
fn colon_close_handles_the_first_last_and_only_session() {
    for (count, closing, expected) in [(3, 1, Some(2)), (3, 3, Some(2)), (1, 1, None)] {
        let sessions = (1..=count).map(|id| session(id, Tool::Claude, Status::Idle, true)).collect();
        let (mut app, _rx) = app_with(sessions, vec![]);
        app.select(Sel::Session(closing));
        command(&mut app, "close");
        assert_eq!(app.current, expected.map(Sel::Session), "closing {closing} of {count}");
    }
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![outside(99, None)]);
    command(&mut app, "close");
    assert_eq!(app.current, None);
}

#[test]
fn plain_keys_drive_the_deck_when_no_program_has_them() {
    // Outside processes do not become a selection in an empty deck.
    let (mut app, rx) = app_with(vec![], vec![outside(9, None), outside(10, None)]);
    assert_eq!(app.current, None);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.current, None);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.current, None);
    command(&mut app, "new");
    assert!(matches!(app.dialog, Some(Dialog::New { sel: 0, .. })), ":new opens the chooser");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "create", "request": 0, "tool": "claude"})]);
    // An empty deck opens the new-session choice on Enter.
    let (mut app, _rx) = app_with(vec![], vec![]);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(app.dialog, Some(Dialog::New { .. })));
}

#[path = "resume_picker.rs"]
mod resume_picker;

fn frame_with_modes(modes: u32) -> ScreenFrame {
    ScreenFrame { cols: 80, rows: 24, modes, ..ScreenFrame::default() }
}

#[test]
fn switching_sessions_never_flashes_starting() {
    // Owner, 2026-10-26: "when i switch sessions fast i see a "starting"
    // message flash on the screen". A session's screen arrives a moment after
    // it is selected; until then the pane showed the card of a starting one.
    use crate::screen::Run;
    let (mut app, _rx) = app_with(
        vec![
            session(1, Tool::Claude, Status::Idle, true),
            session(2, Tool::Codex, Status::Working, true),
            session(3, Tool::Codex, Status::Starting, true),
        ],
        vec![],
    );
    let text_frame = |text: &str| ScreenFrame {
        cols: 80,
        rows: 24,
        lines: vec![vec![Run { c: 0, w: 1, fg: 0, bg: 0, fl: 0, t: text.into(), g: false }]],
        ..ScreenFrame::default()
    };
    let shown = |app: &mut App| {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        term.draw(|f| {
            crate::view::render(f, app);
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..30).map(|y| (0..120).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    };
    app.select(Sel::Session(1));
    app.handle_server(ServerMsg::Frame { id: 1, frame: text_frame("first screen") });
    app.select(Sel::Session(2));
    let text = shown(&mut app);
    assert!(!text.contains("Starting"), "a running session not seen yet: {text}");
    app.handle_server(ServerMsg::Frame { id: 2, frame: text_frame("second screen") });
    app.select(Sel::Session(1));
    let text = shown(&mut app);
    assert!(text.contains("first screen"), "its last screen until the new one comes: {text}");
    assert!(!text.contains("Starting"), "{text}");
    app.handle_server(ServerMsg::Frame { id: 1, frame: text_frame("first screen, later") });
    assert!(shown(&mut app).contains("first screen, later"));
    // A session that is starting says so.
    app.select(Sel::Session(3));
    assert!(shown(&mut app).contains("Starting…"));
}

// The header is read off the screen thread, and again only when the
// repository changes: never on a timer.
#[test]
fn the_header_branch_and_worktree_count_are_read_only_when_the_repository_changes() {
    let dir = crate::testsupport::TempDir::new("deck-git");
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .current_dir(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=master"])
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "one"]);
    let mut watch = GitWatch::default();
    // Each poll returns at once; a changed reading lands on a later one.
    let next = |watch: &mut GitWatch| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !watch.poll(&repo) {
            assert!(Instant::now() < deadline, "no new reading came");
            thread::sleep(Duration::from_millis(10));
        }
        watch.now.clone().unwrap()
    };
    assert_eq!(next(&mut watch), BranchStatus { branch: "master".into(), ..BranchStatus::default() });
    thread::sleep(Duration::from_millis(800));
    assert!(!watch.poll(&repo), "nothing changed, so nothing was read again");
    git(&["checkout", "-q", "-b", "topic"]);
    assert_eq!(next(&mut watch).branch, "topic");
    // Owner, 2026-10-26: "how many worktrees that exist on disk ../".
    git(&["worktree", "add", "-q", "../repo-agent-x"]);
    assert_eq!(next(&mut watch).worktrees, 1);
    std::fs::remove_dir_all(dir.path().join("repo-agent-x")).unwrap();
    assert_eq!(next(&mut watch).worktrees, 0, "a deleted folder no longer counts");
}

#[test]
fn plain_drag_highlights_unsubmitted_input_without_typing_mouse_reports() {
    use crate::screen::{Run, M_MOUSE_DRAG, M_SGR_MOUSE};
    use ratatui::{backend::TestBackend, Terminal};

    for (tool, modes) in [(Tool::Claude, 0), (Tool::Codex, M_MOUSE_DRAG | M_SGR_MOUSE), (Tool::Antigravity, 0)] {
        let (mut app, rx) = app_with(vec![session(1, tool, Status::Idle, true)], vec![]);
        app.sidebar = false;
        app.mode = Mode::Insert;
        app.frame = Some((
            1,
            ScreenFrame {
                cols: 80,
                rows: 24,
                modes,
                lines: vec![vec![Run { c: 2, w: 1, fg: 0, bg: 0, fl: 0, t: "draft message".into(), g: false }]],
                ..ScreenFrame::default()
            },
        ));
        let mut term = Terminal::new(TestBackend::new(80, 25)).unwrap();
        term.draw(|f| {
            view::render(f, &mut app);
        })
        .unwrap();
        for (kind, column) in
            [(MouseEventKind::Down(MouseButton::Left), 2), (MouseEventKind::Drag(MouseButton::Left), 6)]
        {
            app.handle_input(Event::Mouse(MouseEvent { kind, column, row: 0, modifiers: KeyModifiers::NONE }));
        }
        term.draw(|f| {
            view::render(f, &mut app);
        })
        .unwrap();
        for x in 2..=6 {
            assert_eq!(
                term.backend().buffer()[(x, 0)].bg,
                app.theme.accent,
                "{tool:?}: ordinary drag must highlight the draft"
            );
        }
        assert!(sent(&rx).is_empty(), "selecting draft text must not send mouse clicks or drag reports to the agent");
    }
}

#[test]
fn plain_drag_release_copies_the_original_draft_while_output_updates() {
    use crate::screen::Run;
    let (mut app, rx) = app_with(vec![session(1, Tool::Antigravity, Status::Idle, true)], vec![]);
    app.set_pane(Rect::new(35, 2, 80, 24));
    app.frame = Some((
        1,
        ScreenFrame {
            cols: 80,
            rows: 24,
            lines: vec![vec![Run { c: 2, w: 1, fg: 0, bg: 0, fl: 0, t: "draft message".into(), g: false }]],
            ..ScreenFrame::default()
        },
    ));
    let mouse = |kind, column| Event::Mouse(MouseEvent { kind, column, row: 2, modifiers: KeyModifiers::NONE });
    app.handle_input(mouse(MouseEventKind::Down(MouseButton::Left), 37));
    app.handle_input(mouse(MouseEventKind::Drag(MouseButton::Left), 41));
    assert_eq!(app.selection.as_ref().unwrap().text(), "draft");
    let mut newer = app.frame.as_ref().unwrap().1.clone();
    newer.lines[0][0].t = "replacement text".into();
    app.handle_server(ServerMsg::Frame { id: 1, frame: newer });
    assert_eq!(app.selection.as_ref().unwrap().text(), "draft", "incoming frames must not change an ongoing selection");
    app.handle_input(mouse(MouseEventKind::Up(MouseButton::Left), 41));
    assert_eq!(app.notice.as_ref().unwrap().0, "Selection copied.");
    assert!(app.selection.is_none(), "a replaced draft must not retain a highlight over different text");
    assert!(sent(&rx).is_empty(), "a selection never edits or submits the draft");
}

#[test]
fn typing_pasting_scrolling_resizing_and_session_switches_clear_the_highlight() {
    let mouse = |kind, column| Event::Mouse(MouseEvent { kind, column, row: 3, modifiers: KeyModifiers::NONE });
    let key = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    for event in [
        Some(key),
        Some(Event::Paste("pasted".into())),
        Some(Event::Resize(90, 30)),
        Some(mouse(MouseEventKind::ScrollUp, 41)),
        None,
    ] {
        let (mut app, _rx) = app_with(
            vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Codex, Status::Idle, true)],
            vec![],
        );
        app.select(Sel::Session(1));
        app.set_pane(Rect::new(35, 2, 80, 24));
        app.frame = Some((1, frame_with_modes(0)));
        app.handle_input(mouse(MouseEventKind::Down(MouseButton::Left), 37));
        app.handle_input(mouse(MouseEventKind::Drag(MouseButton::Left), 41));
        app.handle_input(mouse(MouseEventKind::Up(MouseButton::Left), 41));
        assert!(app.selection.as_ref().is_some_and(|s| s.dragged && !s.pressed));
        match event {
            Some(event) => app.handle_input(event),
            None => app.select(Sel::Session(2)),
        }
        assert!(app.selection.is_none(), "interaction must resume the latest screen and clear the old highlight");
    }
}

#[test]
fn paste_and_mouse_follow_the_program_modes() {
    use crate::screen::{M_BRACKETED_PASTE, M_MOUSE_CLICK, M_SGR_MOUSE};
    let (mut app, rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    app.set_pane(Rect { x: 35, y: 0, width: 80, height: 24 });
    app.handle_server(ServerMsg::Frame { id: 1, frame: frame_with_modes(M_BRACKETED_PASTE) });
    app.handle_input(Event::Paste("a\nb".into()));
    assert_eq!(app.mode, Mode::Insert, "pasting is typing");
    let wheel = |kind, column, row| Event::Mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE });
    app.handle_input(wheel(MouseEventKind::ScrollUp, 40, 5));
    app.handle_server(ServerMsg::Frame { id: 1, frame: frame_with_modes(M_MOUSE_CLICK | M_SGR_MOUSE) });
    app.handle_input(wheel(MouseEventKind::Down(MouseButton::Left), 40, 5));
    app.handle_input(wheel(MouseEventKind::Up(MouseButton::Left), 40, 5));
    app.handle_server(ServerMsg::Frame { id: 1, frame: frame_with_modes(M_ALT_SCREEN | M_ALTERNATE_SCROLL) });
    app.handle_input(wheel(MouseEventKind::ScrollDown, 40, 5));
    assert_eq!(
        sent(&rx),
        vec![
            serde_json::json!({"t": "input", "id": 1, "data": "\x1b[200~a\nb\x1b[201~"}),
            serde_json::json!({"t": "input", "id": 1, "data": "\x1b[<0;6;6M"}),
            serde_json::json!({"t": "input", "id": 1, "data": "\x1b[<0;6;6m"}),
            serde_json::json!({"t": "input", "id": 1, "data": "\x1b[B\x1b[B\x1b[B"}),
        ]
    );
}

#[test]
fn sidebar_clicks_select_rows() {
    let (mut app, _rx) = app_with(
        vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Claude, Status::Idle, true)],
        vec![],
    );
    app.set_pane(Rect { x: 35, y: 0, width: 80, height: 24 });
    // A row past a gap: the ENDED heading sits between them.
    app.row_hits = vec![(8, 2, Sel::Session(1)), (12, 2, Sel::Session(2))];
    let click = |row| {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    app.handle_input(click(13));
    assert_eq!(app.current, Some(Sel::Session(2)), "any line of a row selects it");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    app.handle_input(click(8));
    assert_eq!(app.current, Some(Sel::Session(1)));
    assert_eq!(app.mode, Mode::Normal, "the sidebar is the list: clicking it stops typing");
    app.handle_input(click(3));
    assert_eq!(app.current, Some(Sel::Session(1)), "header rows select nothing");
}

#[test]
fn frames_for_other_sessions_are_ignored_and_view_changes_are_sent() {
    let (mut app, rx) = app_with(
        vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Claude, Status::Idle, true)],
        vec![],
    );
    app.select(Sel::Session(1));
    app.handle_server(ServerMsg::Frame { id: 2, frame: frame_with_modes(0) });
    assert!(app.frame.is_none());
    app.set_pane(Rect { x: 35, y: 0, width: 80, height: 24 });
    app.sync_view();
    app.sync_view();
    app.set_pane(Rect { x: 35, y: 0, width: 100, height: 30 });
    app.sync_view();
    app.select(Sel::Session(2));
    app.sync_view();
    assert_eq!(
        sent(&rx),
        vec![
            serde_json::json!({"t": "attach", "id": 1, "cols": 80, "rows": 24}),
            serde_json::json!({"t": "resize", "cols": 100, "rows": 30}),
            serde_json::json!({"t": "attach", "id": 2, "cols": 100, "rows": 30}),
        ]
    );
}

#[test]
fn old_services_never_receive_an_unknown_history_request() {
    let (mut app, rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    app.service_version = 17;
    app.frame = Some((1, ScreenFrame { epoch: 1, history: 100, ..frame_with_modes(0) }));
    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert!(sent(&rx).is_empty());
    assert!(app.notice.as_ref().unwrap().0.contains("protocol 18"));
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE);
    }
}

#[test]
fn one_message_goes_to_every_ticked_live_session() {
    // Codex queues the questions its agent asks mid-turn behind its own key,
    // so Enter there sends the message and answers nothing.
    let mut queued = session(5, Tool::Codex, Status::NeedsYou, true);
    queued.detail = crate::protocol::QUEUED_QUESTION.into();
    let (mut app, rx) = app_with(
        vec![
            session(1, Tool::Claude, Status::Working, true),
            session(2, Tool::Codex, Status::Idle, true),
            session(3, Tool::Claude, Status::NeedsYou, true),
            session(4, Tool::Codex, Status::Ended, false),
            queued,
        ],
        vec![],
    );
    sent(&rx);
    command(&mut app, "broadcast");
    let Some(Dialog::Broadcast { targets, .. }) = &app.dialog else { panic!("no broadcast dialog") };
    let mut ticked: Vec<(u64, bool)> = targets.clone();
    ticked.sort();
    assert_eq!(
        ticked,
        vec![(1, true), (5, true)],
        "only working agents and queued mid-turn questions receive broadcasts"
    );

    type_text(&mut app, "run the tests");
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    app.handle_input(Event::Paste("and report\r\nback".into()));
    assert!(sent(&rx).is_empty(), "nothing reaches a session while composing");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.dialog.is_none());
    let msgs = sent(&rx);
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    assert_eq!(msgs[0]["t"], "broadcast");
    assert_eq!(msgs[0]["running_only"], true);
    assert_eq!(msgs[0]["text"], "run the tests\nand report\nback");
    let mut ids: Vec<u64> = serde_json::from_value(msgs[0]["ids"].clone()).unwrap();
    ids.sort();
    assert!(ids.is_empty(), "the default selection broadcasts across all hosts");
    command(&mut app, "broadcast stop if this is yours");
    let msgs = sent(&rx);
    assert_eq!(msgs[0]["running_only"], true);
    let mut ids: Vec<u64> = serde_json::from_value(msgs[0]["ids"].clone()).unwrap();
    ids.sort();
    assert!(ids.is_empty(), "the service selects current recipients on every host");
    for version in [11, 12, 13, 14] {
        app.service_version = version;
        command(&mut app, "broadcast stop if this is yours");
        command(&mut app, "broadcast");
        assert!(app.dialog.is_none() && sent(&rx).is_empty(), "protocol {version} cannot enforce broadcast scope");
    }
}

#[test]
fn broadcast_ticks_toggle_and_empty_sends_are_refused() {
    let (mut app, rx) = app_with(
        vec![session(1, Tool::Claude, Status::Working, true), session(2, Tool::Codex, Status::Working, true)],
        vec![],
    );
    sent(&rx);
    command(&mut app, "broadcast");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.dialog.is_some() && sent(&rx).is_empty(), "an empty message is not sent");
    type_text(&mut app, "hi");
    // Ctrl-A clears every tick (all were ticked); Enter then refuses.
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.dialog.is_some() && sent(&rx).is_empty(), "no recipients, nothing sent");
    // Tick only the highlighted (first) session.
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    let Some(Dialog::Broadcast { targets, .. }) = &app.dialog else { panic!() };
    let first = targets[0].0;
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let msgs = sent(&rx);
    assert_eq!(msgs[0]["ids"], serde_json::json!([first]));
    assert_eq!(msgs[0]["text"], "hi");
    // Esc abandons a draft without sending.
    command(&mut app, "broadcast");
    type_text(&mut app, "never mind");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.dialog.is_none() && sent(&rx).is_empty());
    // With nothing live there is nobody to message.
    let (mut app, _rx) = app_with(vec![session(4, Tool::Codex, Status::Ended, false)], vec![]);
    command(&mut app, "broadcast");
    assert!(app.dialog.is_none());
}

#[test]
fn each_redraw_reaches_the_terminal_as_one_synchronized_update() {
    // Owner, 2026-10-26: "my cursor flickers an jumps around". kitty shows a
    // frame only when its synchronized update ends, so the cursor is never
    // seen travelling across the screen while cells are written.
    use crate::screen::{CursorPos, Run, CURSOR_BEAM};
    use ratatui::{TerminalOptions, Viewport};
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    let run = Run { c: 2, w: 1, fg: 0, bg: 0, fl: 0, t: "hello pane".into(), g: false };
    let cursor = Some(CursorPos { x: 4, y: 0, shape: CURSOR_BEAM });
    app.frame = Some((1, ScreenFrame { cols: 80, rows: 24, lines: vec![vec![run]], cursor, ..ScreenFrame::default() }));
    app.mode = Mode::Insert;
    // The terminal's writer, shared so the test can read what reached it.
    #[derive(Clone, Default)]
    struct Screen(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
    impl Write for Screen {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let screen = Screen::default();
    let options = TerminalOptions { viewport: Viewport::Fixed(Rect::new(0, 0, 120, 30)) };
    let mut term = Terminal::with_options(CrosstermBackend::new(screen.clone()), options).unwrap();
    let mut written = Vec::new();
    for _ in 0..2 {
        app.draw(&mut term).unwrap();
        written.push(String::from_utf8(std::mem::take(&mut *screen.0.borrow_mut())).unwrap());
    }
    for out in &written {
        assert!(out.starts_with("\x1b[?2026h") && out.ends_with("\x1b[?2026l"), "{out:?}");
        assert_eq!((out.matches("\x1b[?2026h").count(), out.matches("\x1b[?2026l").count()), (1, 1), "{out:?}");
    }
    // The first frame's cells, its cursor and the cursor's shape are all
    // inside the update.
    let first = &written[0];
    assert!(first.contains('h') && first.contains("\x1b[?25h") && first.contains("\x1b[6 q"), "{first:?}");
}

/// The next reconnect result the deck's link reports, skipping messages.
fn reconnect_result(events: &Receiver<AppEv>) -> AppEv {
    loop {
        let ev = events.recv_timeout(Duration::from_secs(10)).expect("the reconnect reported back");
        let ev = match ev {
            AppEv::Linked(_, ev) => *ev,
            ev => ev,
        };
        if matches!(ev, AppEv::Reconnected(_) | AppEv::ReconnectFailed | AppEv::RestartFailed(_)) {
            return ev;
        }
    }
}

#[test]
fn the_deck_reconnects_when_the_service_updates_and_quits_once_it_is_gone() {
    // The service closes every connection when it updates itself in place
    // (owner, 2026-10-26: "make it so the service just auto updates in
    // place"); the deck carries on over a new one.
    let dir = crate::testsupport::TempDir::new("tui-reconnect");
    let socket = dir.path().join("deck.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    app.current = Some(Sel::Session(1));
    let (tx, events) = mpsc::channel();
    let path = socket.clone();
    app.link = Some(Link {
        tx,
        connect: Arc::new(move || UnixStream::connect(&path)),
        start: Arc::new(|| Err(io::Error::other("never asked"))),
    });

    app.handle(AppEv::Disconnected);
    assert!(!app.quit, "the deck stays open while it reconnects");
    let ev = reconnect_result(&events);
    assert!(matches!(ev, AppEv::Reconnected(_)));
    app.handle(ev);
    assert!(!app.quit);
    let (conn, _) = listener.accept().unwrap();
    let mut from_deck = BufReader::new(conn);
    let hello: ClientMsg = read_msg(&mut from_deck).unwrap().unwrap();
    assert!(matches!(hello, ClientMsg::Hello { role: ClientRole::Tui, .. }), "it introduces itself again");
    app.sync_view();
    let attach: ClientMsg = read_msg(&mut from_deck).unwrap().unwrap();
    assert!(matches!(attach, ClientMsg::Attach { id: Some(1), .. }), "it shows the same session again");

    // A stopped service removes its socket: the deck ends as before.
    drop(listener);
    std::fs::remove_file(&socket).unwrap();
    app.handle(AppEv::Disconnected);
    let ev = reconnect_result(&events);
    assert!(matches!(ev, AppEv::ReconnectFailed));
    app.handle(ev);
    assert!(app.quit && app.disconnected);
}

#[test]
fn after_stopping_the_service_a_closed_connection_ends_the_deck() {
    let (mut app, _rx) = app_with(Vec::new(), vec![]);
    let (tx, events) = mpsc::channel();
    let never: Connector = Arc::new(|| Err(io::Error::other("never asked")));
    app.link = Some(Link { tx, connect: never.clone(), start: never });
    app.confirm(Action::Shutdown);
    app.handle(AppEv::Disconnected);
    assert!(app.quit && app.disconnected, ":stop stopped it on purpose");
    assert!(events.recv_timeout(Duration::from_millis(200)).is_err(), "no reconnect was tried");
}

#[test]
fn the_restart_key_restarts_a_new_service_in_place() {
    // Owner, 2026-10-26: "in the desk there should be a key that restarts
    // the service , and reopens all sessions as they were before the
    // restart". A service of this build runs its program again in its own
    // process, so every session carries on and nothing needs confirming.
    let (mut app, rx) = app_with(vec![session(1, Tool::Claude, Status::Working, true)], vec![]);
    command(&mut app, "restart");
    assert!(app.dialog.is_none(), "{:?}", app.notice);
    let asked: Vec<_> = sent(&rx).into_iter().map(|m| m["t"].clone()).collect();
    assert_eq!(asked, ["restart"]);
    // The connection closes as the program starts again; the deck reconnects.
    app.handle_server(ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: "/srv/repo".into() });
    assert!(app.notice.as_ref().unwrap().0.contains("every session kept running"), "{:?}", app.notice);
    assert!(sent(&rx).is_empty(), "nothing to reopen");
}

#[test]
fn the_restart_key_stops_an_older_service_starts_it_and_reopens_its_sessions() {
    // An older service cannot pass its sessions on: the owner's ran
    // protocol 1 with agents in it ("cant lose whats running"). It is
    // stopped once confirmed, started again, and each session reopened in
    // its conversation.
    let dir = crate::testsupport::TempDir::new("tui-restart");
    let socket = dir.path().join("deck.sock");
    let mut unknown = session(2, Tool::Codex, Status::Idle, true);
    unknown.conversation = None;
    let before =
        vec![session(1, Tool::Claude, Status::Working, true), unknown, session(3, Tool::Codex, Status::Idle, false)];
    let (mut app, rx) = app_with(before.clone(), vec![]);
    // Its process has exited by the time the deck starts the service again:
    // no process has an id above Linux's limit of 4194304.
    app.handle_server(ServerMsg::Welcome { version: 1, pid: 4_194_305, start_dir: "/srv/repo".into() });
    let (tx, events) = mpsc::channel();
    let path = socket.clone();
    app.link = Some(Link {
        tx,
        // The stopped service's socket is gone; only starting it connects.
        connect: Arc::new(|| Err(io::Error::from(io::ErrorKind::NotFound))),
        start: Arc::new(move || UnixStream::connect(&path)),
    });
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    command(&mut app, "restart");
    let Some(Dialog::Confirm { text, .. }) = &app.dialog else { panic!("it asks first: {:?}", app.notice) };
    assert!(text.contains("its 2 live session(s) end and reopen in their conversations"), "{text}");
    assert!(text.contains("1 working stop mid-turn") && text.contains("1 without a known conversation"), "{text}");
    assert!(sent(&rx).is_empty(), "nothing is stopped before the owner says so");
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    let asked: Vec<_> = sent(&rx).into_iter().map(|m| m["t"].clone()).collect();
    assert_eq!(asked, ["shutdown"]);

    app.handle(AppEv::Disconnected);
    assert!(!app.quit, "the deck stays open while the service starts again");
    let ev = reconnect_result(&events);
    assert!(matches!(ev, AppEv::Reconnected(_)));
    app.handle(ev);
    let (conn, _) = listener.accept().unwrap();
    conn.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let mut from_deck = BufReader::new(conn);
    let hello: ClientMsg = read_msg(&mut from_deck).unwrap().unwrap();
    assert!(matches!(hello, ClientMsg::Hello { .. }));
    // The new service lists the sessions the old one ended.
    app.handle_server(ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 4321, start_dir: "/srv/repo".into() });
    let ended = before.into_iter().map(|s| SessionInfo {
        account_root: None,
        metadata: Default::default(),
        alive: false,
        pid: None,
        status: Status::Ended,
        ..s
    });
    app.handle_server(ServerMsg::State(DeckState { sessions: ended.collect(), ..DeckState::default() }));
    let mut reopened = Vec::new();
    while let Ok(Some(msg)) = read_msg::<_, ClientMsg>(&mut from_deck) {
        if let ClientMsg::Reopen { id } = msg {
            reopened.push(id);
        }
    }
    assert_eq!(reopened, [1], "the live session with a conversation; not the ended one");
    assert!(app.notice.as_ref().unwrap().0.contains("reopened 1 session"), "{:?}", app.notice);
    assert!(!app.quit);
}

#[test]
fn colon_pair_pairs_a_phone_and_alt_p_reaches_the_program_while_typing() {
    let dir = crate::testsupport::TempDir::new("tui-pairing");
    crate::remote::store::load_or_create_identity(dir.path(), "198.51.100.102").unwrap();
    let cfg = Config { remote: true, ..Config::default() };
    let (mut app, rx) = configured_app(cfg, vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    app.remote_dir = Some(dir.path().to_path_buf());
    let code_file = dir.path().join("pairing.json");
    command(&mut app, "pair");
    let Some(Dialog::Pair(pairing)) = &app.dialog else { panic!("the pairing screen opens") };
    let code = pairing.offer.as_ref().unwrap().code.clone();
    assert!(code_file.exists(), "a code is open while the screen shows it");
    press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    let Some(Dialog::Pair(pairing)) = &app.dialog else { panic!("still open") };
    let renewed = pairing.offer.as_ref().unwrap().code.clone();
    assert!(crate::remote::auth::redeem_code(dir.path(), &code, util::now_secs()).is_err() || code == renewed);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.dialog.is_none(), "Esc closes pairing");
    assert!(!code_file.exists(), "closing the screen ends the code");
    command(&mut app, "pair");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.dialog.is_none() && !code_file.exists());

    // Typing into a session, Alt+p is the program's: Claude Code switches
    // models with it.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Insert);
    sent(&rx);
    alt(&mut app, 'p');
    assert!(app.dialog.is_none());
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "input", "id": 1, "data": "\x1bp"})]);
    assert!(!code_file.exists());
}

#[test]
fn a_deck_without_the_gateways_folder_never_opens_a_code() {
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    command(&mut app, "pair");
    assert!(app.dialog.is_none());
    assert!(app.notice.as_ref().unwrap().0.contains("cannot pair"), "{:?}", app.notice);
}

// ---- the 2026-10-30 review of the deck's keys and messages ----

#[test]
fn enter_does_not_stop_the_service() {
    // An empty-service stop still needs explicit confirmation.
    let (mut app, rx) = app_with(Vec::new(), vec![]);
    command(&mut app, "stop");
    assert!(matches!(app.dialog, Some(Dialog::Confirm { action: Action::Shutdown, .. })));
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(app.dialog, Some(Dialog::Confirm { .. })), "still asking");
    assert!(sent(&rx).is_empty());
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "shutdown"})]);
}

#[test]
fn colon_rename_names_the_session_and_a_bare_one_clears_it() {
    let (mut app, rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    command(&mut app, "rename Ship the parser");
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "rename", "id": 1, "name": "Ship the parser"})]);
    command(&mut app, "rename");
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "rename", "id": 1, "name": null})]);
    // Control characters never reach the service, and the name is capped.
    command(&mut app, &format!("rename a\u{7}b {}", "x".repeat(200)));
    let named = sent(&rx);
    let name = named[0]["name"].as_str().unwrap();
    assert!(name.starts_with("ab ") && name.chars().count() == crate::protocol::MAX_NAME, "{name}");
    // An older service cannot read it.
    app.service_version = 5;
    command(&mut app, "rename Old");
    assert!(sent(&rx).is_empty());
    assert!(app.notice.as_ref().unwrap().0.contains("needs the new service"), "{:?}", app.notice);
    // Nothing selected: nothing to name.
    let (mut app, rx) = app_with(vec![], vec![outside(9, None)]);
    command(&mut app, "rename x");
    assert!(sent(&rx).is_empty());
    assert!(app.notice.as_ref().unwrap().1, "{:?}", app.notice);
}

#[test]
fn the_new_dialog_starts_a_program_with_the_model_and_effort_chosen() {
    // The phone could choose them; the deck's own dialog could not.
    let dir = crate::testsupport::TempDir::new("tui-models");
    let (mut app, rx) = app_with(vec![], vec![]);
    app.model_dirs = Some(crate::remote::models::Homes::under(dir.path()));
    command(&mut app, "new");
    press(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('E'), KeyModifiers::SHIFT);
    assert!(matches!(app.dialog, Some(Dialog::New { sel: 0, model: 2, effort: 5 })), "Sonnet, and back to max");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        sent(&rx),
        vec![serde_json::json!({"t": "create", "request": 0, "tool": "claude", "model": "sonnet", "effort": "max"})]
    );
    assert!(
        app.notice.as_ref().unwrap().0.starts_with("Starting Claude with sonnet, max effort in "),
        "{:?}",
        app.notice
    );
    // Another program starts from its own defaults; Grok offers none here.
    command(&mut app, "new");
    press(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert!(matches!(app.dialog, Some(Dialog::New { sel: 1, model: 0, effort: 0 })));
    press(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "create", "request": 0, "tool": "grok"})]);
    // A service from before protocol 5 cannot start with one.
    app.service_version = 4;
    command(&mut app, "new");
    press(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(sent(&rx).is_empty());
    assert!(app.notice.as_ref().unwrap().0.contains("needs the new service"), "{:?}", app.notice);
}

#[test]
fn native_codex_new_keeps_model_choice_in_create_without_a_terminal_picker() {
    let dir = crate::testsupport::TempDir::new("tui-codex-model");
    let levels = serde_json::json!([{"effort":"low"},{"effort":"high"}]);
    dir.write(
        "codex/models_cache.json",
        serde_json::json!({"models":[{
            "slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol","supported_reasoning_levels":levels
        }]})
        .to_string(),
    );
    let (mut app, rx) = app_with(vec![], vec![]);
    app.model_dirs = Some(crate::remote::models::Homes::under(dir.path()));
    command(&mut app, "new");
    for key in ['j', 'm', 'e', 'e'] {
        press(&mut app, KeyCode::Char(key), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        sent(&rx),
        vec![serde_json::json!({
            "t":"create","request":0,"tool":"codex","model":"gpt-6.1-sol","effort":"high"
        })]
    );
    app.handle_server(ServerMsg::Created { id: 7, request: None });
    app.handle_server(ServerMsg::State(DeckState {
        sessions: vec![session(7, Tool::Codex, Status::Idle, true)],
        ..app.state.clone()
    }));
    assert!(sent(&rx).is_empty(), "Created already attests API settings; no model keys or second request");
    command(&mut app, "codex --model gpt-6.1-sol");
    assert_eq!(sent(&rx)[0]["model"], "gpt-6.1-sol");
    app.service_version = 18;
    command(&mut app, "codex --model gpt-6.1-sol");
    assert!(sent(&rx).is_empty(), "old coordinator must not use the terminal model path");
}

pub(crate) fn host(name: &str, label: &str, online: bool) -> HostInfo {
    HostInfo {
        role: Default::default(),
        name: name.into(),
        label: label.into(),
        online,
        reason: (!online).then(|| "connection refused".into()),
        start_dir: format!("/home/{name}/repo"),
        stats: None,
    }
}

pub(crate) fn on(host: &str, mut s: SessionInfo) -> SessionInfo {
    s.id = crate::protocol::host_id(if host == "fedora" { 1 } else { 2 }, s.id);
    s.host = Some(host.into());
    s
}

#[test]
fn the_new_dialog_defaults_to_this_machine_and_offers_only_online_agent_hosts() {
    // Owner, 2026-11-08: agents run only on this machine or Fedora, and
    // Ubuntu and WSL are build hosts.
    let remote = on("fedora", session(1, Tool::Codex, Status::Working, true));
    let remote_id = remote.id;
    let (mut app, rx) = app_with(vec![remote, session(2, Tool::Claude, Status::Idle, true)], vec![]);
    app.service_version = 14;
    let mut build = host("controller", "Ubuntu", true);
    build.role = crate::config::HostRole::Build;
    app.hosts = vec![host("fedora", "Fedora", true), build, host("wsl", "WSL", false)];
    assert_eq!(app.start_hosts(), vec![None, Some("fedora".into())], "build and offline hosts start nothing");
    assert!(app.resolve_host("ubuntu").is_err(), "a build host is not a place to start");
    app.current = Some(Sel::Session(remote_id));
    command(&mut app, "new");
    assert_eq!(app.new_host, None, "the highlighted host never pins New");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let creates: Vec<_> = sent(&rx).into_iter().filter(|m| m["t"] == "create").collect();
    assert_eq!(creates[0]["host"], serde_json::Value::Null);
    app.current = Some(Sel::Session(2));
    command(&mut app, "new");
    press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    assert_eq!(app.new_host.as_deref(), Some("fedora"));
    assert_eq!(app.start_dir_on(app.new_host.as_deref()), "/home/fedora/repo");
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.new_host, None, "the host choices wrap");
    assert_eq!(app.start_dir_on(app.new_host.as_deref()), "/srv/repo");
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.new_host.as_deref(), Some("fedora"));
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    let creates: Vec<_> = sent(&rx).into_iter().filter(|m| m["t"] == "create").collect();
    assert_eq!(creates[0]["host"], "fedora");
}

/// A clipboard holding `types`, each of whose content is `content`.
fn clipboard(dir: &crate::testsupport::TempDir, name: &str, types: &str, content: &str) -> crate::clipboard::Reader {
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in --list-types) [ -n '{types}' ] || exit 1; printf '{types}';; *) printf '{content}';; esac\n"
    );
    let path = dir.executable(name, script);
    crate::clipboard::Reader { wayland: true, wl_paste: path.to_string_lossy().into_owned(), xclip: String::new() }
}

#[test]
fn ctrl_v_in_a_session_on_another_host_pastes_this_machines_clipboard() {
    // Owner, 2026-11-01: "my clipboard uses the fedora clipboard instead of
    // this one, thats weird". Claude Code's Ctrl+V reads the clipboard of
    // the machine it runs on.
    let dir = crate::testsupport::TempDir::new("deck-clipboard");
    let fedora = crate::protocol::host_id(1, 4);
    let mut remote = session(fedora, Tool::Claude, Status::Idle, true);
    remote.host = Some("fedora".into());
    let (mut app, rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true), remote], vec![]);
    app.select(Sel::Session(fedora));
    app.mode = Mode::Insert;
    app.clipboard = clipboard(&dir, "text", "text/plain;charset=utf-8\\n", "copied here");
    ctrl(&mut app, 'v');
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "input", "id": fedora, "data": "copied here"})]);
    // A screenshot is kept on the host and its path pasted there.
    app.clipboard = clipboard(&dir, "image", "text/plain\\nimage/png\\n", "PNGDATA");
    ctrl(&mut app, 'v');
    let got = sent(&rx);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0]["t"].as_str(), got[0]["id"].as_u64()), (Some("paste_file"), Some(fedora)));
    assert_eq!(got[0]["data"], util::base64(b"PNGDATA"));
    assert!(got[0]["name"].as_str().is_some_and(|n| n.ends_with(".png")), "{got:?}");
    // Nothing copied: nothing typed, rather than the host's clipboard.
    app.clipboard = clipboard(&dir, "empty", "", "");
    ctrl(&mut app, 'v');
    assert!(sent(&rx).is_empty());
    // A session on this machine reads this machine's clipboard itself.
    app.select(Sel::Session(1));
    app.mode = Mode::Insert;
    app.clipboard = clipboard(&dir, "unused", "text/plain\\n", "not read");
    ctrl(&mut app, 'v');
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "input", "id": 1, "data": "\u{16}"})]);
}
