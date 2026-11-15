use super::*;
use crate::tui::tests::{app_with, press, session};
use ratatui::backend::TestBackend;

fn info(id: u64, status: &str) -> Info {
    Info {
        id,
        title: "Review build".into(),
        provider: Some(Tool::Codex),
        status: status.into(),
        created_at: 1,
        started_at: Some(2),
        finished_at: (status == "failed").then_some(3),
        exit_code: (status == "failed").then_some(23),
        signal: (status == "failed").then_some(15),
        reason: "fixture failure".into(),
        host: Some("worker".into()),
    }
}

fn app(id: u64) -> (App, Receiver<ClientMsg>) {
    let (mut app, rx) = app_with(vec![session(11, Tool::Claude, Status::Idle, true)], vec![]);
    app.state.job_capable = true;
    app.state.jobs = vec![info(id, "failed")];
    (app, rx)
}

fn requests(rx: &Receiver<ClientMsg>) -> Vec<(u64, String, Value)> {
    rx.try_iter()
        .filter_map(|m| match m {
            ClientMsg::Fleet { request, action, args } => Some((request, action, args)),
            _ => None,
        })
        .collect()
}

#[test]
fn jobs_route_opaque_ids_and_never_attach_or_receive_interactive_input() {
    let id = (7 << 48) + 55;
    let (mut app, rx) = app(id);
    app.select(Sel::Job(id));
    app.handle_input(Event::Paste("do not send".into()));
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    app.sync_view();
    let messages: Vec<_> = rx.try_iter().collect();
    assert_eq!(app.mode, Mode::Normal);
    assert!(messages.iter().any(|m| matches!(m, ClientMsg::Attach { id: None, .. })));
    assert!(!messages.iter().any(|m| matches!(m, ClientMsg::Input { .. } | ClientMsg::Attach { id: Some(_), .. })));
    assert!(app.rows().contains(&Sel::Session(11)));
    assert!(app.rows().contains(&Sel::Job(id)), "failed jobs remain selectable");
    app.run_command("job stop");
    let stop = requests(&rx).pop().unwrap();
    assert_eq!(stop.1, "job-stop");
    assert_eq!(stop.2, json!({"job":id}), "host metadata never bypasses service routing");
    app.handle_server(ServerMsg::FleetResult { request: stop.0, data: json!({"job":info(id,"failed")}), error: None });
    app.run_command("job dismiss");
    let dismiss = requests(&rx).pop().unwrap();
    assert_eq!(dismiss.1, "job-dismiss");
    assert_eq!(dismiss.2, json!({"job":id}));
    app.handle_server(ServerMsg::FleetResult {
        request: dismiss.0,
        data: json!({"job":info(id,"failed")}),
        error: None,
    });
    assert!(app.state.jobs.is_empty());
    assert_eq!(app.current, Some(Sel::Session(11)));
    assert_eq!(app.state.sessions.len(), 1);
}

#[test]
fn jobs_start_literal_argv_and_explicitly_gate_missing_service_support() {
    let (mut app, rx) = app(99);
    for (version, capable) in [(12, true), (13, true), (14, false)] {
        app.service_version = version;
        app.state.job_capable = capable;
        app.run_command("job start -- printf hello");
        assert!(requests(&rx).is_empty());
        assert!(app.notice.as_ref().unwrap().0.contains("update needed"));
    }
    app.service_version = 14;
    app.state.job_capable = true;
    app.run_command("job start --title 'Review fixture' --cwd '/srv/my repo' --host local --provider codex -- printf '%s' '$HOME' '*' ';' '$(touch x)'");
    let (_, action, args) = requests(&rx).pop().unwrap();
    assert_eq!(action, "job-start");
    assert_eq!(args["argv"], json!(["printf", "%s", "$HOME", "*", ";", "$(touch x)"]));
    assert_eq!(args["cwd"], "/srv/my repo");
    assert_eq!(args["placement"], "local");
    assert_eq!(args["provider"], "codex");
    assert_eq!(args["title"], "Review fixture");
}

#[test]
fn job_chat_snapshots_replace_and_raw_byte_pages_ignore_stale_replies() {
    let (mut app, rx) = app(99);
    app.select(Sel::Job(99));
    let first = requests(&rx).pop().unwrap();
    let answer = |text: &str| json!([{"k":"assistant","text":text}]);
    app.handle_server(ServerMsg::FleetResult {
        request: first.0,
        data: json!({"job":99,"offset":10,"text":"first page","items":answer("partial"),"eof":false}),
        error: None,
    });
    app.refresh_job();
    let second = requests(&rx).pop().unwrap();
    app.handle_server(ServerMsg::FleetResult { request:second.0, data:json!({"job":99,"offset":10,"text":"first page","items":answer("complete\u{1b}]52;c;payload\u{7}\nlast line"),"items_truncated":true,"eof":false}), error:None });
    assert_eq!(app.jobs.items.len(), 1, "cumulative snapshots replace, not append");
    app.run_command("job next");
    let next = requests(&rx).pop().unwrap();
    assert_eq!(next.2["offset"], 10);
    app.run_command("job prev");
    let prev = requests(&rx).pop().unwrap();
    assert_eq!(prev.2["offset"], 0);
    app.handle_server(ServerMsg::FleetResult {
        request: next.0,
        data: json!({"job":99,"offset":20,"text":"stale next page","items":[],"eof":true}),
        error: None,
    });
    app.handle_server(ServerMsg::FleetResult { request:prev.0, data:json!({"job":99,"offset":10,"text":"safe\u{1b}[31m raw\u{7}\nlast line","items":answer("complete"),"items_truncated":true,"eof":false}), error:None });
    assert!(!app.jobs.text.contains("stale"));
    let mut term = Terminal::new(TestBackend::new(120, 28)).unwrap();
    term.draw(|f| {
        assert!(crate::view::render(f, &mut app).is_none());
    })
    .unwrap();
    let screen = term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
    assert!(!screen.contains('\u{1b}') && !screen.contains('\u{7}'));
    assert!(screen.contains("Exit code: 23") && screen.contains("Signal: 15"));
    assert!(screen.contains("fixture failure") && screen.contains("last line"));
    app.run_command("job chat");
    let chat = requests(&rx).pop().unwrap();
    app.handle_server(ServerMsg::FleetResult {
        request: chat.0,
        data: json!({"job":99,"offset":10,"text":"","items":answer("complete"),"items_truncated":true,"eof":false}),
        error: None,
    });
    term.draw(|f| {
        crate::view::render(f, &mut app);
    })
    .unwrap();
    let screen = term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
    assert!(screen.contains("Chat truncated") && screen.contains("complete"));
}

#[test]
fn a_stale_raw_page_timeout_cannot_stop_the_selected_pages_updates() {
    let (mut app, rx) = app(99);
    app.select(Sel::Job(99));
    let first = requests(&rx).pop().unwrap();
    app.handle_server(ServerMsg::FleetResult {
        request: first.0,
        data: json!({"job":99,"offset":10,"text":"first","items":[],"eof":false}),
        error: None,
    });
    app.run_command("job next");
    let stale = requests(&rx).pop().unwrap();
    assert_eq!(stale.2["offset"], 10);
    app.run_command("job prev");
    let current = requests(&rx).pop().unwrap();
    app.handle_server(ServerMsg::FleetResult {
        request: current.0,
        data: json!({"job":99,"offset":10,"text":"first refreshed","items":[],"eof":false}),
        error: None,
    });
    app.poll_job(Instant::now() + Duration::from_secs(16));
    assert!(app.jobs.problem.is_none(), "a departed page timeout must not poison the current page");
    assert!(requests(&rx).iter().any(|(_, action, args)| action == "job-log" && args["offset"] == 0));
}

#[test]
fn raw_next_uses_the_selected_pages_cursor_after_a_page_change() {
    let (mut app, rx) = app(99);
    app.select(Sel::Job(99));
    let first = requests(&rx).pop().unwrap();
    app.handle_server(ServerMsg::FleetResult {
        request: first.0,
        data: json!({"job":99,"offset":10,"text":"first","items":[],"eof":false}),
        error: None,
    });
    app.run_command("job next");
    let second = requests(&rx).pop().unwrap();
    app.handle_server(ServerMsg::FleetResult {
        request: second.0,
        data: json!({"job":99,"offset":20,"text":"second","items":[],"eof":false}),
        error: None,
    });
    app.run_command("job prev");
    let previous = requests(&rx).pop().unwrap();
    assert_eq!(previous.2["offset"], 0);
    app.run_command("job next");
    assert!(
        requests(&rx).is_empty(),
        "the departed page cursor must not skip a page before the selected response arrives"
    );
    app.handle_server(ServerMsg::FleetResult {
        request: previous.0,
        data: json!({"job":99,"offset":10,"text":"first","items":[],"eof":false}),
        error: None,
    });
    app.run_command("job next");
    assert_eq!(requests(&rx).pop().unwrap().2["offset"], 10);
}

#[test]
fn a_hundred_retained_jobs_keep_session_numbers_stable() {
    let selected = (8 << 48) + 55;
    let (mut app, rx) = app(selected);
    app.state.sessions.push(session(12, Tool::Codex, Status::Working, true));
    let mut remote = session(13, Tool::Codex, Status::Idle, true);
    remote.host = Some("worker".into());
    app.state.sessions.push(remote);
    // An order written when Jobs had a sidebar group still draws none.
    app.sidebar_order = Some(vec!["sessions".into(), "jobs".into()]);
    app.state.jobs.extend((1..100).map(|id| {
        let mut job = info(id, "working");
        job.created_at = 2;
        job
    }));
    app.select(Sel::Job(selected));
    let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
    term.draw(|f| {
        crate::view::render(f, &mut app);
    })
    .unwrap();
    assert!(
        !app.row_hits.iter().any(|(_, _, row)| matches!(row, Sel::Job(_))),
        "jobs keep their own pane and take no sidebar rows"
    );
    assert_eq!(app.position(11), Some(1));
    app.fix_selection(false);
    assert_eq!(app.current, Some(Sel::Job(selected)), "service updates preserve the explicit job view");
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert!(matches!(app.current, Some(Sel::Job(_))), "the Jobs pane still browses retained results");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Job(selected)));
    press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Session(11)));
    assert!(requests(&rx).iter().all(|(_, action, _)| action == "job-log"));
    for (key, expected) in [
        (KeyCode::Up, 13),
        (KeyCode::Down, 11),
        (KeyCode::Down, 12),
        (KeyCode::Down, 13),
        (KeyCode::Down, 11),
        (KeyCode::Char('k'), 13),
        (KeyCode::Char('j'), 11),
    ] {
        press(&mut app, key, KeyModifiers::NONE);
        assert_eq!(app.current, Some(Sel::Session(expected)), "{key:?} must follow visible sessions");
        assert_eq!(app.mode, Mode::Normal);
    }
    for (kind, expected) in [(MouseEventKind::ScrollUp, 13), (MouseEventKind::ScrollDown, 11)] {
        app.handle_input(Event::Mouse(MouseEvent { kind, column: 0, row: 1, modifiers: KeyModifiers::NONE }));
        assert_eq!(app.current, Some(Sel::Session(expected)), "sidebar scrolling must follow visible sessions");
    }
    assert!(requests(&rx).is_empty(), "session navigation must never open a hidden job");
    for (kind, expected) in [(MouseEventKind::ScrollUp, 13), (MouseEventKind::ScrollDown, 12)] {
        app.select(Sel::Job(selected));
        requests(&rx);
        app.handle_input(Event::Mouse(MouseEvent { kind, column: 0, row: 1, modifiers: KeyModifiers::NONE }));
        assert_eq!(app.current, Some(Sel::Session(expected)), "sidebar scrolling must leave the Jobs pane");
        assert_eq!(app.mode, Mode::Normal);
        assert!(requests(&rx).is_empty(), "sidebar scrolling must never request a hidden job");
    }
}

#[test]
fn reconnect_refreshes_job_reads_without_replaying_uncertain_controls() {
    let (mut app, rx) = app(99);
    app.select(Sel::Job(99));
    let old = requests(&rx).pop().unwrap().0;
    app.run_command("job stop");
    let control = requests(&rx).pop().unwrap().0;
    app.jobs.problem = Some("old connection failed".into());
    let (events, _received) = mpsc::channel();
    app.link = Some(Link {
        tx: events,
        connect: Arc::new(|| Err(io::Error::other("fixture connector unused"))),
        start: Arc::new(|| Err(io::Error::other("fixture starter unused"))),
    });
    let (client, server) = UnixStream::pair().unwrap();
    app.reconnected(client);
    assert!(!app.jobs.pending.contains_key(&old), "log reads from the old connection must be invalidated");
    assert!(app.jobs.pending.contains_key(&control), "uncertain controls remain status-checkable and are not replayed");
    assert!(app.jobs.problem.is_none());
    assert_eq!(app.jobs.pending.values().filter(|(p, _)| matches!(p, Pending::Log { id: 99, offset: 0 })).count(), 1);
    assert_eq!(app.jobs.pending.values().filter(|(p, _)| matches!(p, Pending::Control { .. })).count(), 1);
    server.shutdown(std::net::Shutdown::Both).unwrap();
}

#[test]
fn stop_remains_available_for_failed_jobs_without_an_exit_code_including_signal_only_receipts() {
    for (status, exit, signal, eligible) in [
        ("starting", None, None, true),
        ("working", None, None, true),
        ("failed", None, None, true),
        ("failed", None, Some(15), true),
        ("failed", Some(23), None, false),
        ("failed", Some(137), Some(9), false),
        ("finished", Some(0), None, false),
    ] {
        let id = (7 << 48) + 55;
        let (mut app, rx) = app(id);
        let job = &mut app.state.jobs[0];
        job.status = status.into();
        job.exit_code = exit;
        job.signal = signal;
        let receipt = job.clone();
        app.select(Sel::Job(id));
        requests(&rx);
        let mut term = Terminal::new(TestBackend::new(120, 28)).unwrap();
        term.draw(|f| {
            crate::view::render(f, &mut app);
        })
        .unwrap();
        let screen = term.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>();
        assert_eq!(screen.contains(":job stop"), eligible, "{status} exit={exit:?} signal={signal:?}");
        for _ in 0..2 {
            app.run_command("job stop");
            let sent = requests(&rx);
            // Explicit manual commands retain the backend's idempotent API,
            // including a repeated request after a signal-only receipt.
            assert_eq!(sent.len(), 1, "{status} exit={exit:?} signal={signal:?}");
            assert_eq!(sent[0].1, "job-stop");
            assert_eq!(sent[0].2, json!({"job":id}));
            app.handle_server(ServerMsg::FleetResult { request: sent[0].0, data: json!({"job":receipt}), error: None });
            assert_eq!(app.state.jobs[0].exit_code, exit);
            assert_eq!(app.state.jobs[0].signal, signal);
            assert_eq!(app.current, Some(Sel::Job(id)));
            assert_eq!(app.mode, Mode::Normal);
        }
    }
}
