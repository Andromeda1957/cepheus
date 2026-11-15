use super::tests::{logged, options, screen_of, wait_for_screen};
use super::*;
use crate::testsupport::TempDir;

#[test]
fn configured_agent_runs_broadcasts_and_reopens_the_persisted_conversation() {
    let dir = TempDir::new("provider-pty");
    let command = dir.executable(
        "fake-agent",
        r#"#!/usr/bin/env python3
import json, sys
if len(sys.argv) == 1:
    sys.exit(0)
print('\x1b[?2004hREADY ' + json.dumps(sys.argv[1:]), flush=True)
for line in sys.stdin:
    print('RECEIVED ' + line.strip(), flush=True)
"#,
    );
    let provider: crate::providers::Provider = serde_json::from_value(serde_json::json!({
        "id":"fixture-agent-with-long-provider", "label":"Fixture", "command":["python3", command],
        "model_flag":"--model", "new_session_args":["--session-id","{conversation}"],
        "resume_args":["--resume","{conversation}"]
    }))
    .unwrap();
    let mut opts = super::tests::options(&dir, "true");
    opts.config.providers = vec![provider];
    let (tx, rx) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let tool = Tool::Dynamic("fixture-agent-with-long-provider");
    let id = svc
        .create_with(tool, None, &StartModel { model: Some("small".into()), effort: None }, &Placement::default())
        .unwrap();
    let screen =
        |svc: &Service| screen::screen_text(&svc.sessions[&id].hosted.as_ref().unwrap().lock().term).join("\n");
    let wait = |svc: &Service, needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !screen(svc).contains(needle) {
            assert!(Instant::now() < deadline, "provider did not show {needle}: {}", screen(svc));
            thread::sleep(Duration::from_millis(10));
        }
    };
    wait(&svc, "READY");
    assert!(screen(&svc).contains("small"));
    let conversation = svc.sessions[&id].conversation.clone().unwrap();
    // A configured provider has no message interface of its own, so a
    // message is refused rather than typed into its terminal.
    let inputs = svc.sessions[&id].inputs;
    svc.broadcast_message(0, &[id], "hello provider");
    svc.submit_due(Instant::now() + Duration::from_secs(1));
    assert_eq!(svc.sessions[&id].inputs, inputs);
    assert!(!svc.messages.active(id));
    let plugin = "provider-fixture-agent-with-long-provider";
    assert!(svc.service_plugin_commands().iter().any(|registered| {
        registered.id == plugin && registered.enabled && registered.commands.iter().any(|c| c.name == "target")
    }));
    let (out, replies) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 7, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    let expected = crate::attention::ExpectedTarget {
        spawned_ms: svc.sessions[&id].spawned_ms,
        pid: svc.sessions[&id].hosted.as_ref().unwrap().pid,
        conversation: svc.sessions[&id].conversation.clone(),
    };
    let target = serde_json::json!({"id":id,"expected":expected,"operation":"send","text":"targeted provider"});
    svc.handle_plugin(7, 71, plugin, "target", &target);
    let mut refused = false;
    for reply in replies.try_iter() {
        if let Out::Msg(bytes) = reply {
            if let Some(ServerMsg::MessageSent { id: sent, request: 71, error, delivered }) =
                read_msg(&mut std::io::Cursor::new(bytes)).unwrap()
            {
                assert_eq!(sent, id);
                let error = error.expect("a provider without a message interface must refuse");
                assert!(error.contains("no supported direct message interface; nothing was sent"), "{error}");
                assert_eq!(delivered, None);
                refused = true;
            }
        }
    }
    assert!(refused, "the refusal is reported at once, without waiting on the terminal");
    assert_eq!(svc.sessions[&id].inputs, inputs);
    thread::sleep(Duration::from_millis(200));
    while let Ok(event) = rx.try_recv() {
        svc.handle(event);
    }
    assert!(!screen(&svc).contains("hello provider") && !screen(&svc).contains("targeted provider"));
    let mut stale = target.clone();
    stale["expected"]["spawned_ms"] = (expected.spawned_ms + 1).into();
    let inputs = svc.sessions[&id].inputs;
    svc.handle_plugin(7, 72, plugin, "target", &stale);
    assert_eq!(svc.sessions[&id].inputs, inputs);
    assert!(replies.try_iter().any(|reply| match reply {
        Out::Msg(bytes) => matches!(
            read_msg(&mut std::io::Cursor::new(bytes)).unwrap(),
            Some(ServerMsg::PluginResult { request: 72, error: Some(_), .. })
        ),
        _ => false,
    }));
    svc.opts.config.plugins.insert(plugin.into(), false);
    assert!(!svc.service_plugin_commands().iter().any(|registered| registered.id == plugin && registered.enabled));
    svc.opts.config.plugins.remove(plugin);
    let mut collision = svc.opts.config.providers[0].clone();
    collision.id = "accounts".into();
    svc.opts.config.providers.push(collision);
    let registrations = svc.service_plugin_commands();
    let accounts: Vec<_> = registrations.iter().filter(|registered| registered.id == "provider-accounts").collect();
    assert_eq!(accounts.len(), 1, "a configured provider must preserve the catalog plugin identity");
    assert_eq!(accounts[0].name, "Provider accounts");
    assert_eq!(accounts[0].commands.iter().filter(|command| command.name == "target").count(), 1);
    svc.opts.config.providers.pop();
    let state = svc.own_state();
    assert_eq!(state.providers[0].id, "fixture-agent-with-long-provider");
    let pid = svc.sessions[&id].hosted.as_ref().unwrap().pid;
    svc.opts.config.providers[0].cloud = true;
    assert!(!svc.service_plugin_commands().iter().any(|registered| registered.id == plugin && registered.enabled));
    let queued = Placement {
        launch_key: Some("new-cloud-task".into()),
        launch_fingerprint: Some("private-task".into()),
        prompt: Some(crate::keys::non_owner_message("PRIVATE_TASK")),
        ..Default::default()
    };
    let inputs = svc.sessions[&id].inputs;
    let turns = svc.sessions[&id].tracker.completed_turns;
    assert!(svc
        .create_with(tool, None, &StartModel::default(), &queued)
        .unwrap_err()
        .to_string()
        .contains("Cloud providers do not support"));
    assert_eq!(svc.sessions[&id].inputs, inputs);
    assert_eq!(svc.sessions[&id].tracker.completed_turns, turns);
    assert_eq!(svc.sessions[&id].hosted.as_ref().unwrap().pid, pid);
    assert!(svc.registry.sessions.iter().all(|entry| entry.launch_key.as_deref() != Some("new-cloud-task")));
    {
        let host = "@here";
        let routed = svc
            .route_client(
                0,
                ClientMsg::Create {
                    tool,
                    host: Some(host.into()),
                    model: None,
                    effort: None,
                    cwd: None,
                    command: Vec::new(),
                    name: None,
                    request: None,
                },
            )
            .expect("@here starts the native cloud bridge on this machine");
        assert!(matches!(routed, ClientMsg::Create { host: None, .. }));
        let reconnected = svc.create_with(tool, None, &StartModel::default(), &Placement::default()).unwrap();
        assert_eq!(reconnected, id, "desktop and phone reconnect to the same native cloud bridge");
        assert_eq!(svc.sessions.len(), 1);
    }
    assert!(
        svc.route_client(
            0,
            ClientMsg::Create {
                tool,
                host: Some("selected-peer".into()),
                model: None,
                effort: None,
                cwd: None,
                command: Vec::new(),
                name: None,
                request: None,
            }
        )
        .is_none(),
        "an unavailable explicit cloud source must not reconnect to the local bridge"
    );
    assert_eq!(svc.sessions.len(), 1);
    assert!(svc.change_directory(0, id, ".").unwrap_err().to_string().contains("cloud agents"));
    assert_eq!(svc.sessions[&id].hosted.as_ref().unwrap().pid, pid);
    assert_eq!(svc.sessions[&id].conversation.as_deref(), Some(conversation.as_str()));
    assert!(svc.sessions[&id].closing.is_none());
    assert!(svc.pending_directories.is_empty());
    svc.opts.config.providers[0].cloud = false;
    assert!(state_for(10, &state).sessions.is_empty());
    assert_eq!(state_for(PROTOCOL_VERSION, &state).sessions[0].tool, tool);
    svc.close(id);
    let deadline = Instant::now() + Duration::from_secs(10);
    while svc.sessions[&id].alive {
        assert!(Instant::now() < deadline, "provider never exited");
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) {
            svc.handle(event);
        }
    }
    let saved = Registry::load(&svc.registry_path);
    assert_eq!(saved.sessions[0].tool, tool);
    assert_eq!(saved.sessions[0].conversation.as_deref(), Some(conversation.as_str()));
    svc.reopen(id).unwrap();
    wait(&svc, "--resume");
    assert!(screen(&svc).contains(&conversation));
    svc.close(id);
    let deadline = Instant::now() + Duration::from_secs(10);
    while svc.sessions[&id].alive {
        assert!(Instant::now() < deadline, "reopened provider never exited");
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) {
            svc.handle(event);
        }
    }
}

#[test]
fn stopped_codex_turns_retry_capacity_and_revoked_login_without_restarting() {
    for (error, goal_active) in [
        ("Selected model is at capacity. Please try a different model.", false),
        ("Fatal error: application network permission was revoked", true),
    ] {
        let dir = TempDir::new("svc-auto-continue");
        let script = format!(
            r#"printf '\033[?2004h'; printf '■ {error}\r\n\r\n› \033[2mAsk anything\033[0m\r\n\033[1A\033[3G'; stty -echo; while IFS= read -r x; do printf 'got %s\n' "$x"; done"#
        );
        let (tx, _rx) = mpsc::channel();
        let mut svc = Service::new(options(&dir, &script), tx);
        let codex = svc.create(Tool::Codex, None).unwrap();
        let claude = svc.create(Tool::Claude, None).unwrap();
        let pid = svc.sessions[&codex].hosted.as_ref().unwrap().pid;
        for id in [codex, claude] {
            wait_for_screen(&svc, id, "›");
        }
        // A fatal turn ends even when its persistent goal and background shell remain.
        let tracker = &mut svc.sessions.get_mut(&codex).unwrap().tracker;
        tracker.goal_active = goal_active;
        tracker.background = goal_active;
        tracker.tool_busy = Some(false);
        let mut start = Instant::now() + Duration::from_secs(1);
        if goal_active {
            let blocked = |svc: &mut Service, now: Instant| {
                svc.refresh_status(now);
                svc.refresh_status(now + AUTO_CONTINUE_FIRST_WAIT);
                assert!(svc.pending_submits.is_empty(), "no automatic input while recovery is blocked");
            };
            for busy in [None, Some(true)] {
                svc.sessions.get_mut(&codex).unwrap().tracker.tool_busy = busy;
                blocked(&mut svc, start);
                start += AUTO_CONTINUE_FIRST_WAIT * 2;
            }
            let tracker = &mut svc.sessions.get_mut(&codex).unwrap().tracker;
            tracker.tool_busy = Some(false);
            tracker.hook_asks("permission".into());
            blocked(&mut svc, start);
            start += AUTO_CONTINUE_FIRST_WAIT * 2;
            svc.sessions.get_mut(&codex).unwrap().tracker.user_input();
            let patterns = std::mem::take(&mut svc.opts.config.auto_continue);
            blocked(&mut svc, start);
            start += AUTO_CONTINUE_FIRST_WAIT * 2;
            svc.opts.config.auto_continue = patterns;
        }
        svc.refresh_status(start);
        assert!(svc.pending_submits.is_empty(), "the first continue waits");
        svc.refresh_status(start + AUTO_CONTINUE_FIRST_WAIT - Duration::from_secs(1));
        assert!(svc.pending_submits.is_empty(), "the first continue waits");
        svc.refresh_status(start + AUTO_CONTINUE_FIRST_WAIT);
        let pending: Vec<u64> = svc.pending_submits.iter().map(|p| p.id).collect();
        if pending != vec![codex] {
            for s in svc.sessions.values() {
                s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
            }
            panic!("only the stopped Codex session should recover from {error:?}: {pending:?}");
        }
        wait_for_screen(&svc, codex, "continue");
        svc.submit_due(start + AUTO_CONTINUE_FIRST_WAIT + SUBMIT_DELAY);
        wait_for_screen(&svc, codex, "got </cross-session-message>");
        let text = screen_of(&svc, codex);
        assert!(text.lines().any(|l| l.starts_with("got") && l.contains("continue")), "{text}");
        assert!(!screen_of(&svc, claude).contains("continue"), "Claude Code is never typed into");
        let pattern =
            if goal_active { "application network permission was revoked" } else { "Selected model is at capacity" };
        assert_eq!(logged(&svc), vec![format!("Codex: \"{pattern}\"; typed continue (1 of 8 in 30 minutes)")]);
        assert_eq!(svc.sessions[&codex].hosted.as_ref().unwrap().pid, pid, "the existing session process survives");
        // The turn went on: nothing more is typed.
        svc.refresh_status(start + AUTO_CONTINUE_FIRST_WAIT * 40);
        assert!(svc.pending_submits.is_empty());
        for s in svc.sessions.values() {
            s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
        }
    }
}
