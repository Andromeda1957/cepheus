use super::tests::{options, TestClient};
use super::*;
use crate::testsupport::{shutdown_service, TempDir};

pub(crate) fn draft_session(client: &mut TestClient) -> u64 {
    client.send(ClientMsg::Create {
        tool: Tool::Claude,
        model: None,
        effort: None,
        host: None,
        cwd: None,
        command: Vec::new(),
        name: None,
        request: None,
    });
    let id = client.wait("fixture session", |msg, _| match msg {
        ServerMsg::Created { id, .. } => Some(*id),
        _ => None,
    });
    client.wait_screen(id, "stand-in ready");
    client.send(ClientMsg::Input { id, data: "unsent draft".into() });
    client.wait_screen(id, "unsent draft");
    id
}

#[test]
fn cli_stop_preserves_live_pty_and_draft_even_with_yes() {
    let dir = TempDir::new("stop-cli");
    let opts = options(&dir, "echo stand-in ready; read x; echo accepted:$x; read y");
    let socket = opts.socket.clone();
    let service = thread::spawn(move || run(opts));
    let mut client = TestClient::connect(&socket);
    let id = draft_session(&mut client);
    let mut conn = crate::ctl::Conn::over(UnixStream::connect(&socket).unwrap()).unwrap();
    assert!(crate::ctl::stop_connection(&mut conn, true).is_err(), "--yes cannot authorize ending a session");
    client.send(ClientMsg::Shutdown);
    client.wait("public shutdown refusal", |m, _| matches!(m, ServerMsg::Notice { error: true, .. }).then_some(()));
    client.wait_screen(id, "unsent draft");
    client.send(ClientMsg::Input { id, data: "\r".into() });
    client.wait_screen(id, "accepted:unsent draft");
    shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

#[test]
fn public_stop_keeps_all_cold_recovery_custody_before_shutdown_mutations() {
    let mut lost = Vec::new();
    for kind in ["restore", "continue", "saved", "boot", "archive"] {
        let dir = TempDir::new("stop-cold-custody");
        let opts = options(&dir, "true");
        let mut entry = Entry::started(1, Tool::Claude, Some("cold-chat".into()), "unfinished task".into(), 1);
        entry.ended_at = Some(1);
        entry.resume = kind == "saved";
        entry.working = kind != "archive";
        entry.boot_pending = kind == "boot";
        entry.recovery = Some(recovery::ClientRecovery {
            draft: Some("unsent draft".into()),
            pending: vec!["owned request".into()],
            ..Default::default()
        });
        Registry { next_id: 2, sessions: vec![entry] }.save(&opts.state_dir.join("sessions.json")).unwrap();
        let (tx, _) = mpsc::channel();
        let mut svc = Service::new(opts, tx);
        let now = Instant::now();
        match kind {
            "restore" => svc.pending_restores.push(resume::PendingRestore { id: 1, due: now, attempts: 1 }),
            "continue" => svc.pending_continues.push(resume::PendingContinue { id: 1, since: now, ready_since: None }),
            _ => {}
        }
        assert!(!svc.sessions[&1].alive);
        let before = Registry::load(&svc.registry_path);
        svc.handle_client(0, ClientMsg::Shutdown);
        if kind == "archive" {
            assert!(svc.shutdown_at.is_some(), "ordinary ended history cannot hold an empty service open");
            assert_eq!(Registry::load(&svc.registry_path), before);
            continue;
        }
        if svc.shutdown_at.is_some()
            || svc.extensions.cancel.load(Ordering::SeqCst)
            || Registry::load(&svc.registry_path) != before
            || !svc.errors.borrow().iter().any(|e| e.text.contains("recovery"))
        {
            lost.push(kind);
        }
        assert_eq!(svc.pending_restores.len(), usize::from(kind == "restore"));
        assert_eq!(svc.pending_continues.len(), usize::from(kind == "continue"));
        if svc.shutdown_at.is_none() {
            svc.handle_client(0, ClientMsg::Close { id: 1 });
            svc.handle_client(0, ClientMsg::Shutdown);
            assert!(svc.shutdown_at.is_some(), "orphaned recovery timers cannot block Stop after explicit Close");
        }
    }
    assert!(lost.is_empty(), "public Stop did not preserve cold recovery custody: {lost:?}");
}

#[test]
fn cli_stop_refuses_cold_recovery_until_explicit_close_without_hanging() {
    let dir = TempDir::new("stop-cold-cli");
    let mut opts = options(&dir, "true");
    opts.config.recovery.startup_delay_secs = 3600;
    let mut entry = Entry::started(1, Tool::Claude, Some("cold-chat".into()), "unfinished task".into(), 1);
    entry.ended_at = Some(1);
    entry.resume = true;
    entry.working = true;
    entry.boot_pending = true;
    entry.recovery = Some(recovery::ClientRecovery { draft: Some("unsent draft".into()), ..Default::default() });
    let registry = opts.state_dir.join("sessions.json");
    Registry { next_id: 2, sessions: vec![entry] }.save(&registry).unwrap();
    let socket = opts.socket.clone();
    let service = thread::spawn(move || run(opts));
    let mut client = TestClient::connect(&socket);
    let mut conn = crate::ctl::Conn::over(UnixStream::connect(&socket).unwrap()).unwrap();
    let result = crate::ctl::stop_connection(&mut conn, true);
    if result.is_ok() {
        service.join().unwrap().unwrap();
        panic!("--yes discarded cold recovery while no PTY was alive");
    }
    assert!(format!("{:#}", result.unwrap_err()).contains("recovery"));
    let kept = Registry::load(&registry);
    assert!(kept.sessions[0].resume && kept.sessions[0].boot_pending);
    assert_eq!(kept.sessions[0].recovery.as_ref().unwrap().draft.as_deref(), Some("unsent draft"));
    client.send(ClientMsg::Close { id: 1 });
    // Use the same connection: Close and Shutdown are ordered by the service.
    client.send(ClientMsg::Shutdown);
    client.wait("empty Stop accepted after explicit Close", |message, _| {
        matches!(message, ServerMsg::Notice { text, error: false } if text == crate::protocol::STOP_ACCEPTED_NOTICE)
            .then_some(())
    });
    service.join().unwrap().unwrap();
    assert!(Registry::load(&registry).sessions.is_empty());
}
