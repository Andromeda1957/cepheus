use super::tests::{app_with, host, on, session};
use super::*;

#[test]
fn sessions_share_a_stable_manual_order_across_hosts() {
    let mut fedora = on("fedora", session(1, Tool::Codex, Status::Working, true));
    fedora.created_at = 10;
    let mut controller = on("controller", session(3, Tool::Claude, Status::Idle, true));
    controller.created_at = 30;
    let mut local = session(4, Tool::Claude, Status::Working, true);
    local.created_at = 20;
    let (f, c) = (fedora.id, controller.id);
    let (mut app, rx) =
        app_with(vec![controller, local, fedora, session(2, Tool::Claude, Status::Ended, false)], vec![]);
    app.service_version = 14;
    app.hosts = vec![host("fedora", "Fedora", true), host("controller", ".142", true)];
    assert_eq!(app.rows(), [Sel::Session(f), Sel::Session(4), Sel::Session(c)]);
    app.order = vec![c, f, 4];
    app.select(Sel::Session(f));
    let mut state = app.state.clone();
    state.session_order = vec![c, f, 4];
    state.sessions.reverse();
    app.handle_server(ServerMsg::State(state));
    assert_eq!(app.current, Some(Sel::Session(f)), "a refresh keeps the selected identity");
    assert_eq!(app.position(4), Some(3), "numbers follow the global saved order");
    app.move_session(-1);
    assert_eq!(app.rows(), [Sel::Session(f), Sel::Session(c), Sel::Session(4)], "a manual move crosses hosts");
    assert!(rx
        .try_iter()
        .map(|msg| serde_json::to_value(msg).unwrap())
        .collect::<Vec<_>>()
        .iter()
        .any(|msg| msg["t"] == "set_order" && msg["ids"] == serde_json::json!([f, c, 4])));
    app.handle_server(ServerMsg::Notice { text: "Could not save session order: disk full".into(), error: true });
    assert_eq!(
        app.rows(),
        [Sel::Session(c), Sel::Session(f), Sel::Session(4)],
        "a refused move restores canonical order"
    );
    let mut state = app.state.clone();
    state.session_order = vec![4, c, f];
    app.handle_server(ServerMsg::State(state));
    assert_eq!(app.rows(), [Sel::Session(4), Sel::Session(c), Sel::Session(f)]);
}

/// Owner, 2026-11-11: "sessions start at #2 instead of number #1 as i think
/// its counting the dot agent as a session when it shouldnt". Dot heads the
/// list under its own name; the sessions after it number from 1, and the
/// number keys and commands count them the same way.
#[test]
fn dot_takes_no_session_number() {
    let sessions =
        vec![session(9, Tool::Dynamic("dot"), Status::Idle, true), session(1, Tool::Claude, Status::Idle, true)];
    let (mut app, _rx) = app_with([sessions, vec![session(2, Tool::Codex, Status::Idle, true)]].concat(), vec![]);
    app.state.providers.push(crate::providers::Info {
        id: "dot".into(),
        label: "Dot".into(),
        cloud: true,
        ..Default::default()
    });
    assert_eq!(app.rows(), [Sel::Session(9), Sel::Session(1), Sel::Session(2)]);
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
    term.draw(|f| {
        view::render(f, &mut app);
    })
    .unwrap();
    let buf = term.backend().buffer();
    let row = (0..30)
        .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .find(|row| row.contains("task 1"))
        .unwrap();
    let number = row.split("task 1").next().unwrap().trim_start_matches([' ', '▌']);
    assert!(number.starts_with("1 "), "{row}");
    super::tests::press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Session(2)));
    app.run_command("go 1");
    assert_eq!(app.current, Some(Sel::Session(1)));
}

fn orders(rx: &Receiver<ClientMsg>) -> Vec<Vec<u64>> {
    rx.try_iter()
        .filter_map(|msg| match msg {
            ClientMsg::SetOrder { ids } => Some(ids),
            _ => None,
        })
        .collect()
}

/// A new state from the service, which saved the order the deck sent.
fn update(app: &mut App, change: impl FnOnce(&mut DeckState)) {
    let mut state = app.state.clone();
    state.session_order = app.order.clone();
    change(&mut state);
    app.handle_server(ServerMsg::State(state));
}

fn find(state: &mut DeckState, id: u64) -> &mut SessionInfo {
    state.sessions.iter_mut().find(|s| s.id == id).unwrap()
}

fn ids(app: &App) -> Vec<u64> {
    app.ordered_sessions().iter().map(|s| s.id).collect()
}

/// Owner, 2026-11-05: ":auto-ordering ... active agents are moved up the agent
/// list, and /clear agents are always moved to the bottom ... each session idle
/// or not should be grouped by type first and formost, then by activity".
/// The deck sorts when sessions start, end, change status or are cleared,
/// here and on another host, and never on other state updates.
#[test]
fn auto_ordering_sorts_on_session_events_only_across_hosts() {
    let used = |mut s: SessionInfo| {
        s.last_active = 1_700_000_000;
        s
    };
    let fedora = |id| crate::protocol::host_id(1, id);
    let dir = crate::testsupport::TempDir::new("auto-ordering-events");
    dir.write("config/plugins/auto-ordering.lua", include_str!("../../plugins/auto-ordering.lua"));
    std::fs::create_dir_all(dir.path().join("start")).unwrap();
    let (mut app, rx) = app_with(Vec::new(), vec![]);
    app.hosts = vec![host("fedora", "Fedora", true)];
    app.state.start_dir = dir.path().join("start").to_string_lossy().into_owned();
    app.state.sessions = vec![
        used(session(1, Tool::Codex, Status::Idle, true)),
        session(2, Tool::Claude, Status::Working, true),
        session(3, Tool::Codex, Status::Working, true),
        used(session(4, Tool::Claude, Status::Idle, true)),
        used(session(8, Tool::Claude, Status::Idle, true)),
        on("fedora", used(session(5, Tool::Claude, Status::Idle, true))),
        on("fedora", used(session(6, Tool::Codex, Status::Idle, true))),
        on("fedora", used(session(9, Tool::Codex, Status::Idle, true))),
        session(7, Tool::Claude, Status::Ended, false),
    ];
    app.order = vec![1, 2, 3, 4, 8, fedora(5), fedora(6), fedora(9), 7];
    app.current = Some(Sel::Session(4));
    app.load_lua(
        crate::lua::Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("start") },
        Arc::new(|_| {}),
    );
    // Codex came first here and Claude first on Fedora: each keeps its place.
    let sorted = vec![3, 1, 2, 4, 8, fedora(5), fedora(6), fedora(9)];
    assert_eq!(ids(&app), sorted);
    assert_eq!(orders(&rx), [sorted.clone()]);
    assert_eq!(app.current, Some(Sel::Session(4)), "the highlighted session stays highlighted");

    // A moved row, a title, recent output and a resumed chat sort nothing.
    update(&mut app, |state| {
        state.session_order = vec![3, 1, 4, 2, 8, fedora(5), fedora(6), fedora(9)];
        let s = find(state, 2);
        (s.title, s.last_active) = ("renamed".into(), 1_700_000_100);
        let s = find(state, 8);
        (s.conversation, s.last_active) = (Some("resumed".into()), 1_700_000_200);
    });
    assert_eq!(ids(&app), [3, 1, 4, 2, 8, fedora(5), fedora(6), fedora(9)]);
    assert!(orders(&rx).is_empty(), "no session event, no sorting");

    // A change of status sorts: working sessions first within their type.
    update(&mut app, |state| find(state, 1).status = Status::Working);
    assert_eq!(ids(&app), sorted);
    assert_eq!(orders(&rx), [sorted.clone()]);

    // /clear leaves the status idle; the cleared session goes last in its type.
    update(&mut app, |state| {
        let s = find(state, 4);
        (s.conversation, s.last_active) = (Some("fresh".into()), 0);
        let s = find(state, fedora(6));
        (s.conversation, s.last_active) = (None, 0);
    });
    let cleared = vec![3, 1, 2, 8, 4, fedora(5), fedora(9), fedora(6)];
    assert_eq!(ids(&app), cleared);
    assert_eq!(orders(&rx), [cleared]);

    // An ended session leaves the list, which stays sorted without it; it is
    // never sent again. A new session is placed by its activity.
    update(&mut app, |state| {
        let s = find(state, 2);
        (s.alive, s.status) = (false, Status::Ended);
    });
    assert_eq!(ids(&app), [3, 1, 8, 4, fedora(5), fedora(9), fedora(6)]);
    assert!(orders(&rx).is_empty());
    update(&mut app, |state| {
        let mut new = session(10, Tool::Claude, Status::Starting, true);
        new.created_at = 1;
        state.sessions.push(new);
    });
    assert_eq!(ids(&app), [3, 1, 10, 8, 4, fedora(5), fedora(9), fedora(6)]);
    assert_eq!(orders(&rx), [vec![3, 1, 10, 8, 4, fedora(5), fedora(9), fedora(6)]]);
    assert_eq!(app.current, Some(Sel::Session(4)));

    // The service's reply to each save shows nowhere: the list shows the order.
    app.notice = None;
    app.handle_server(ServerMsg::Notice { text: crate::protocol::ORDER_SAVED_NOTICE.into(), error: false });
    assert!(app.notice.is_none());
    assert!(!app.messages.iter().any(|m| m.text == crate::protocol::ORDER_SAVED_NOTICE));
}

/// Owner, 2026-11-12: "the auto ordering moves agents around live as they
/// work, it should order ones that are not working and move those to the
/// bottom, not move every working agent around non stop". A question or a
/// turn that a Stop hook sends on (shown finished for up to 15 seconds) moves
/// no working session; one that stays stopped goes below them.
#[test]
fn auto_ordering_moves_only_sessions_that_stopped_working() {
    use std::time::{Duration, Instant};
    /// The deck's timers run each simulated second from `from` through `to`.
    fn seconds(app: &mut App, from: Instant, to: std::ops::RangeInclusive<u64>) {
        for second in to {
            app.lua_tick(from + Duration::from_millis(second * 1000 + 500));
        }
    }
    let dir = crate::testsupport::TempDir::new("auto-ordering-working");
    dir.write("config/plugins/auto-ordering.lua", include_str!("../../plugins/auto-ordering.lua"));
    std::fs::create_dir_all(dir.path().join("start")).unwrap();
    let (mut app, rx) = app_with(Vec::new(), vec![]);
    app.state.start_dir = dir.path().join("start").to_string_lossy().into_owned();
    let mut idle = session(4, Tool::Claude, Status::Idle, true);
    idle.last_active = 1_700_000_000;
    app.state.sessions = vec![
        session(1, Tool::Claude, Status::Working, true),
        session(2, Tool::Claude, Status::Working, true),
        session(3, Tool::Claude, Status::Working, true),
        idle,
    ];
    app.order = vec![1, 2, 3, 4];
    app.load_lua(
        crate::lua::Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("start") },
        Arc::new(|_| {}),
    );
    assert_eq!(ids(&app), [1, 2, 3, 4]);

    update(&mut app, |state| find(state, 1).status = Status::NeedsYou);
    update(&mut app, |state| {
        find(state, 1).status = Status::Working;
        find(state, 2).status = Status::Finished;
    });
    let start = Instant::now();
    seconds(&mut app, start, 1..=15);
    update(&mut app, |state| find(state, 2).status = Status::Working);
    seconds(&mut app, start, 16..=60);
    assert_eq!(ids(&app), [1, 2, 3, 4], "working sessions keep their places");
    assert!(orders(&rx).is_empty());

    // Still stopped after the wait, it heads the idle sessions.
    update(&mut app, |state| find(state, 1).status = Status::Finished);
    let start = Instant::now();
    seconds(&mut app, start, 1..=15);
    assert_eq!(ids(&app), [1, 2, 3, 4]);
    seconds(&mut app, start, 16..=60);
    assert_eq!(ids(&app), [2, 3, 1, 4]);
    assert_eq!(orders(&rx), [vec![2, 3, 1, 4]]);

    // A cleared session waits for nothing: it goes last at once.
    update(&mut app, |state| {
        let s = find(state, 3);
        (s.status, s.conversation, s.last_active) = (Status::Idle, Some("fresh".into()), 0);
    });
    assert_eq!(ids(&app), [2, 1, 4, 3]);
}

#[test]
fn changing_coordinator_discards_old_identity_and_queued_events() {
    let dir = crate::testsupport::TempDir::new("tui-coordinator");
    let config = dir.path().join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "hosts": [{"name": "hub", "tag": 1, "addresses": ["127.0.0.1:8766"], "fingerprint": "1".repeat(64)}]
        }))
        .unwrap(),
    )
    .unwrap();
    let (mut app, old_commands) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    app.follow_config(config.clone());
    app.current = Some(Sel::Session(1));
    app.mode = Mode::Insert;
    app.command = "close 1".into();
    app.window_pending = Some((Instant::now(), KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)));
    app.closed.insert(2, true);
    app.order = vec![1];
    app.service_version = 14;
    app.state.job_capable = true;
    app.state.work_queue_capable = true;
    app.job_command("status").unwrap();
    app.work_queue_command("queue", "list").unwrap();
    let old_requests: Vec<_> = old_commands
        .try_iter()
        .filter_map(|msg| match msg {
            ClientMsg::Fleet { request, action, .. } => Some((request, action)),
            _ => None,
        })
        .collect();
    assert_eq!(old_requests.len(), 2);
    app.jobs.id = Some(1);
    app.jobs.text = "old coordinator log".into();
    let (events_tx, events) = mpsc::channel();
    let (conn, mut server) = UnixStream::pair().unwrap();
    let connection = std::sync::Mutex::new(Some(conn));
    let connect: Connector = Arc::new(move || Ok(connection.lock().unwrap().take().unwrap()));
    app.link = Some(Link { tx: events_tx, connect: connect.clone(), start: connect });
    std::fs::write(config.with_file_name("coordinator.json"), br#"{"host":"hub"}"#).unwrap();

    app.check_config();
    assert!(app.current.is_none() && app.state.sessions.is_empty(), "old registry is gone before reconnect");
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.command.is_empty() && app.window_pending.is_none() && app.closed.is_empty());
    assert!(app.jobs.id.is_none() && app.jobs.text.is_empty() && app.jobs.items.is_empty());
    for (request, action) in &old_requests {
        let data = serde_json::Value::Null;
        let errors = app.error_list().len();
        assert!(
            app.job_result(*request, &data, Some("old coordinator response")),
            "unmatched native replies are consumed before they can reach Lua"
        );
        assert_eq!(app.error_list().len(), errors, "old {action} replies cannot create new registry errors");
        assert!(!app.work_queue_result(*request, &data, None), "old {action} replies cannot enter a new registry");
    }
    assert!(app.next_fleet_request() > old_requests.iter().map(|(id, _)| *id).max().unwrap());
    app.handle(AppEv::Linked(0, Box::new(AppEv::Server(ServerMsg::Created { id: 1, request: None }))));
    app.handle(AppEv::Linked(0, Box::new(AppEv::Input(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))))));
    assert!(app.current.is_none(), "queued old events cannot reselect a reused identity");
    let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
    app.handle(event);
    let hello: ClientMsg = read_msg(&mut BufReader::new(&mut server)).unwrap().unwrap();
    assert!(matches!(hello, ClientMsg::Hello { .. }));
    // Input during the change is discarded even before the new state arrives.
    app.handle(AppEv::Input(Event::Paste("old typing".into())));
    app.handle(AppEv::Server(ServerMsg::Welcome { version: 14, pid: 2, start_dir: "/hub".into() }));
    app.handle(AppEv::Server(ServerMsg::State(DeckState {
        sessions: vec![session(1, Tool::Codex, Status::Idle, true), session(2, Tool::Codex, Status::Idle, true)],
        ..Default::default()
    })));
    assert_eq!(app.state.sessions.len(), 2, "old closed IDs do not hide new sessions");
    assert_eq!(app.mode, Mode::Normal, "new identity requires an explicit entry into Insert");
    assert!(!old_commands.try_iter().any(|msg| matches!(msg, ClientMsg::Input { .. })));
    // Unchanged declaration never starts a second connection.
    app.check_config();
    assert!(events.try_recv().is_err());
}
