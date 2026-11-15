use super::*;
use crate::tui::tests::{app_with, press as press_with, session};
use ratatui::{backend::TestBackend, Terminal};

fn press(app: &mut App, code: KeyCode) {
    press_with(app, code, KeyModifiers::NONE);
}
fn app() -> (App, Receiver<ClientMsg>) {
    let (mut app, rx) = app_with(
        (1..=100)
            .map(|id| session(id, Tool::Codex, if id == 1 { Status::NeedsYou } else { Status::Finished }, true))
            .collect(),
        vec![],
    );
    app.service_version = 17;
    app.state.attention_capable = true;
    rx.try_iter().for_each(drop);
    (app, rx)
}
fn requests(rx: &Receiver<ClientMsg>) -> Vec<(u64, String, Value)> {
    rx.try_iter()
        .map(|message| match message {
            ClientMsg::Fleet { request, action, args } => (request, action, args),
            ClientMsg::Focus { focused } => (0, "focus".into(), json!(focused)),
            other => panic!("unexpected mutation: {other:?}"),
        })
        .collect()
}
fn draw(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal
        .draw(|f| {
            crate::view::render(f, app);
        })
        .unwrap();
    terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect::<Vec<_>>().join("")
}
fn delivery(session: u64, id: &str, state: &str) -> Value {
    json!({"message":{"id":id,"session":session,"state":state,"created_at":1,"updated_at":2,"reason":"fixture"}})
}
fn reply(app: &mut App, request: u64, data: Value, error: Option<&str>) {
    app.handle_server(ServerMsg::FleetResult { request, data, error: error.map(str::to_owned) });
}

#[test]
fn older_peer_and_capability_fail_closed_including_late_reply_and_keep_jobs_queue_independent() {
    let (mut app, rx) = app();
    app.open_attention(Tab::Messages);
    let pending = requests(&rx).pop().unwrap();
    for (version, capable) in [(14, true), (15, false)] {
        app.service_version = version;
        app.state.attention_capable = capable;
        for command in [
            "attention recover",
            "send --idle 1 secret-body",
            "decide private-decision",
            "supervisor on",
            "supervisor off --fleet",
        ] {
            app.run_command(command);
            assert!(requests(&rx).is_empty());
            assert!(app.notice.as_ref().unwrap().0.contains("update needed"));
        }
    }
    app.state.attention.queued.insert(1, 2);
    reply(&mut app, pending.0, json!({"attention":crate::attention::Snapshot::default()}), None);
    assert_eq!(app.state.attention.queued.get(&1), Some(&2));
    app.service_version = 14;
    app.state.job_capable = true;
    app.state.work_queue_capable = true;
    app.run_command("job status");
    app.run_command("queue list");
    let sent = requests(&rx);
    assert!(sent.iter().any(|(_, a, _)| a == "job-status"));
    assert!(sent.iter().any(|(_, a, _)| a == "queue-list"));
}

#[test]
fn idle_send_captures_target_deduplicates_enter_and_late_reply_keeps_new_draft() {
    let (mut app, rx) = app();
    app.current = Some(Sel::Session(1));
    app.run_command("send --idle 1 first message");
    let first = requests(&rx).pop().unwrap();
    assert_eq!(first.1, "attention-send");
    assert_eq!(first.2["session"], 1);
    let key = first.2["key"].as_str().unwrap().to_owned();
    app.current = Some(Sel::Session(2));
    press(&mut app, KeyCode::Enter);
    assert!(requests(&rx).is_empty());
    press(&mut app, KeyCode::Esc);
    app.run_command("send --idle 2 second message");
    let second = requests(&rx).pop().unwrap();
    let secondkey = second.2["key"].as_str().unwrap().to_owned();
    reply(&mut app, first.0, delivery(1, &key, "queued"), None);
    assert_eq!(app.attention.active_draft.as_deref(), Some(secondkey.as_str()));
    assert_eq!(app.attention.drafts[&secondkey].text, "second message");
    assert!(app.attention.drafts[&key].accepted);
    app.mode = Mode::Command;
    app.command = "another unfinished command".into();
    reply(&mut app, second.0, delivery(2, &secondkey, "queued"), None);
    assert_eq!(app.command, "another unfinished command");
    assert_eq!(app.current, Some(Sel::Session(2)));
}

#[test]
fn failed_or_unknown_idle_reply_retains_private_draft_and_same_operation_key_for_explicit_retry() {
    let (mut app, rx) = app();
    app.run_command("send --idle 1 keep this text");
    let sent = requests(&rx).pop().unwrap();
    let key = sent.2["key"].as_str().unwrap().to_owned();
    reply(&mut app, sent.0, json!({}), Some("not admitted"));
    assert_eq!(app.attention.drafts[&key].text, "keep this text");
    assert!(!app.attention.drafts[&key].accepted);
    press(&mut app, KeyCode::Esc);
    app.run_command("send --idle 1");
    assert!(requests(&rx).is_empty());
    press(&mut app, KeyCode::Enter);
    let retry = requests(&rx).pop().unwrap();
    assert_eq!(retry.2["key"], key);
    app.poll_attention(Instant::now() + Duration::from_secs(16));
    assert!(requests(&rx).is_empty());
    assert!(app.attention.drafts[&key].error.is_some());
}

#[test]
fn decision_and_supervisor_are_private_core_requests_with_truthful_pending_hosts() {
    let (mut app, rx) = app();
    app.current = Some(Sel::Session(1));
    app.state.sessions[0].cwd = Some("/srv/repo with spaces".into());
    app.run_command("decide exact text $(never shell) 'quoted'");
    let sent = requests(&rx).pop().unwrap();
    assert_eq!(sent.1, "attention-decide");
    assert_eq!(sent.2["repository"], "/srv/repo with spaces");
    assert_eq!(sent.2["text"], "exact text $(never shell) 'quoted'");
    app.run_command("supervisor off --fleet");
    let off = requests(&rx).pop().unwrap();
    assert_eq!(off.2, json!({"fleet":true}));
    reply(
        &mut app,
        off.0,
        json!({"supervisor":{"enabled":false,"revision":5,"rules":0,"pending":["offline-host"],"audit":[]}}),
        None,
    );
    app.open_attention(Tab::Supervisor);
    requests(&rx);
    let text = draw(&mut app);
    assert!(text.contains("PENDING OR OFFLINE") && text.contains("offline-host"), "{text}");
    assert!(requests(&rx).is_empty());
    app.run_command("supervisor rules [{\"id\":\"test\",\"tool\":\"Bash\",\"pattern\":{\"kind\":\"exact\",\"command\":\"printf test\"}}]");
    let rules = requests(&rx).pop().unwrap();
    assert_eq!(rules.1, "attention-supervisor-rules");
    assert!(rules.2["rules"].is_array());
}

#[test]
fn safe_buffers_and_private_response_identity_keep_untrusted_controls_out_of_terminal() {
    let (mut app, rx) = app();
    app.state.sessions[0].title = "title\u{1b}]52;c;test\u{7}".into();
    app.state.attention.messages = vec![
        serde_json::from_value(
            json!({"id":"a","session":1,"state":"held","created_at":1,"updated_at":1,"reason":"held"}),
        )
        .unwrap(),
        serde_json::from_value(
            json!({"id":"b","session":2,"state":"held","created_at":1,"updated_at":1,"reason":"held"}),
        )
        .unwrap(),
    ];
    app.open_attention(Tab::Messages);
    requests(&rx);
    app.attention_request("attention-message", json!({"id":"a"}));
    let a = requests(&rx).pop().unwrap();
    press(&mut app, KeyCode::Down);
    reply(
        &mut app,
        a.0,
        json!({"text":"wrong-row-body","message":{"id":"a","session":1,"state":"held","created_at":1,"updated_at":1,"reason":"held"}}),
        None,
    );
    assert!(app.attention.private.is_none());
    app.attention_request("attention-message", json!({"id":"b"}));
    let b = requests(&rx).pop().unwrap();
    reply(
        &mut app,
        b.0,
        json!({"text":"current\u{1b}[31m output\u{7}","message":{"id":"b","session":2,"state":"held","created_at":1,"updated_at":1,"reason":"held"}}),
        None,
    );
    reply(&mut app, a.0, json!({"text":"old private body"}), None);
    let buffer = draw(&mut app);
    assert!(buffer.contains("current"));
    assert!(!buffer.contains("old private body"));
    assert!(!buffer.chars().any(char::is_control));
    assert!(requests(&rx).is_empty());
}

#[test]
fn unresolved_drafts_are_immutable_and_bind_to_coordinator_namespace() {
    let (mut app, rx) = app();
    app.run_command("send --idle 1 old text");
    let original = requests(&rx).pop().unwrap();
    reply(&mut app, original.0, json!({}), Some("unknown"));
    press(&mut app, KeyCode::Char('!'));
    let edited = app.attention.active_draft.clone().unwrap();
    assert_eq!(app.attention.drafts[&edited].text, "old text");
    assert_eq!(edited, original.2["key"].as_str().unwrap());
    press(&mut app, KeyCode::Esc);
    app.run_command("send --idle 1");
    assert_eq!(app.attention.active_draft.as_ref(), Some(&edited));
    assert!(requests(&rx).is_empty());
    let (tx, _) = mpsc::channel();
    let never: Connector = Arc::new(|| panic!("coordinator fixture must not connect"));
    app.link = Some(Link { tx, connect: never.clone(), start: never });
    app.stopping_service = true;
    app.coordinator_changed();
    assert!(app.attention.active_draft.is_none());
    assert!(app.attention.private.is_none());
    let (tx, new_rx) = mpsc::channel();
    app.out = tx;
    app.service_version = 17;
    let mut state = DeckState::default();
    state.attention_capable = true;
    state.sessions = vec![session(1, Tool::Codex, Status::Idle, true)];
    app.handle_server(ServerMsg::State(state));
    app.run_command("send --idle 1");
    let current = app.attention.active_draft.clone().unwrap();
    assert_ne!(current, edited);
    assert!(app.attention.drafts[&current].text.is_empty());
    press(&mut app, KeyCode::Enter);
    assert!(requests(&new_rx).is_empty());
    app.attention.active_draft = Some(edited);
    app.submit_idle_draft();
    assert!(requests(&new_rx).is_empty());
    assert!(app.notice.as_ref().unwrap().0.contains("previous coordinator"));
}

#[test]
fn canceled_delivery_never_offers_cancel() {
    let (mut app, rx) = app();
    app.state.attention.messages = vec![serde_json::from_value(
        json!({"id":"canceled-op","session":1,"state":"canceled","created_at":1,"updated_at":2,"reason":"canceled"}),
    )
    .unwrap()];
    app.open_attention(Tab::Messages);
    requests(&rx);
    assert!(draw(&mut app).contains("canceled"));
    press(&mut app, KeyCode::Char('c'));
    assert!(requests(&rx).is_empty());
}

#[test]
fn ambiguous_idle_errors_fence_changed_specs_and_reconcile_only_matching_known_receipts() {
    let (mut app, rx) = app();
    app.run_command("send --idle 1 immutable message");
    let original = requests(&rx).pop().unwrap();
    let key = original.2["key"].as_str().unwrap().to_owned();
    reply(&mut app, original.0, json!({}), Some("remote timed out after persistence"));
    press(&mut app, KeyCode::Char('!'));
    assert_eq!(app.attention.drafts[&key].text, "immutable message");
    press(&mut app, KeyCode::Esc);
    app.run_command("send --idle 1 replacement");
    assert!(requests(&rx).is_empty());
    app.run_command("send --idle 1 immutable message");
    let retry = requests(&rx).pop().unwrap();
    assert_eq!(retry.2, original.2);
    reply(&mut app, retry.0, delivery(2, "wrong-session", "queued"), None);
    assert!(!app.attention.drafts[&key].accepted);
    press(&mut app, KeyCode::Enter);
    let retry = requests(&rx).pop().unwrap();
    reply(&mut app, retry.0, delivery(1, "known-op", "unknown"), None);
    assert!(!app.attention.drafts[&key].accepted);
    press(&mut app, KeyCode::Enter);
    let retry = requests(&rx).pop().unwrap();
    reply(&mut app, retry.0, delivery(1, "known-op", "canceled"), None);
    assert!(app.attention.drafts[&key].accepted);
    press(&mut app, KeyCode::Esc);
    app.run_command("send --idle 1 replacement");
    assert_eq!(requests(&rx).pop().unwrap().2["text"], "replacement");
}

#[test]
fn immutable_decisions_reconcile_after_timeout_and_restart_without_automatic_replay() {
    let dir = crate::testsupport::TempDir::new("attention-recovery");
    let root = dir.path().join("attention-desktop");
    let (mut app, rx) = app();
    app.current = Some(Sel::Session(1));
    app.state.sessions[0].cwd = Some("/srv/repo".into());
    app.use_attention_store(root.clone());
    app.run_command("decide exact retained decision");
    let first = requests(&rx).pop().unwrap();
    reply(&mut app, first.0, json!({}), Some("persisted but reply lost"));
    app.run_command("decide changed decision");
    assert!(requests(&rx).is_empty());
    app.run_command("decide exact retained decision");
    let retry = requests(&rx).pop().unwrap();
    assert_eq!(retry.2, first.2);
    let (mut restored, rx) = self::app();
    restored.current = Some(Sel::Session(1));
    restored.state.sessions[0].cwd = Some("/srv/repo".into());
    restored.use_attention_store(root);
    assert!(requests(&rx).is_empty());
    restored.run_command("decide exact retained decision");
    let retry = requests(&rx).pop().unwrap();
    assert_eq!(retry.2, first.2);
    reply(&mut restored, retry.0, json!({"decision":{"id":"receipt","created_at":1,"recipients":[1,2]}}), None);
    restored.run_command("decide next deliberate decision");
    assert_ne!(requests(&rx).pop().unwrap().2["key"], first.2["key"]);
}

#[test]
fn private_idle_recovery_retains_exact_key_and_conversation_with_no_restart_replay_or_parallel_duplicate() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::testsupport::TempDir::new("attention-idle-recovery");
    let root = dir.path().join("attention-desktop");
    let (mut first, rx) = app();
    let (mut parallel, other) = app();
    first.use_attention_store(root.clone());
    parallel.use_attention_store(root.clone());
    first.run_command("send --idle 1 immutable private fixture");
    let sent = requests(&rx).pop().unwrap();
    parallel.run_command("send --idle 1 duplicate unknown fixture");
    assert!(requests(&other).is_empty());
    parallel.run_command("attention recover");
    parallel.run_command("send --idle 1");
    press(&mut parallel, KeyCode::Enter);
    let reconciled = requests(&other).pop().unwrap();
    assert_eq!(reconciled.2, sent.2);
    first.poll_attention(Instant::now() + Duration::from_secs(16));
    requests(&rx);
    let (mut recovered, rx) = app();
    recovered.use_attention_store(root.clone());
    assert!(requests(&rx).is_empty());
    recovered.run_command("send --idle 1");
    press(&mut recovered, KeyCode::Enter);
    let retry = requests(&rx).pop().unwrap();
    assert_eq!(retry.2, sent.2);
    assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o077, 0);
    for entry in std::fs::read_dir(root).unwrap() {
        assert_eq!(entry.unwrap().metadata().unwrap().permissions().mode() & 0o077, 0);
    }
    reply(&mut recovered, retry.0, json!({}), Some("still unknown"));
    recovered.state.sessions[0].conversation = Some("new-conversation".into());
    press(&mut recovered, KeyCode::Enter);
    assert_eq!(requests(&rx).pop().unwrap().2, sent.2);
}

#[test]
fn returning_to_the_same_coordinator_rebinds_only_its_original_recovery_operations() {
    let dir = crate::testsupport::TempDir::new("attention-origin-return");
    let root = dir.path().join("attention-desktop");
    let (mut app, rx) = app();
    app.use_attention_store(root.clone());
    app.run_command("send --idle 1 original origin");
    let original = requests(&rx).pop().unwrap();
    app.attention_namespace_changed();
    app.coordinator_target = Ok(Some(crate::config::HostConfig {
        role: Default::default(),
        name: "other".into(),
        label: None,
        tag: 7,
        addresses: vec![],
        ssh: None,
        fingerprint: "f".repeat(64),
    }));
    app.use_attention_store(root.clone());
    app.run_command("send --idle 1");
    let other = app.attention.active_draft.clone().unwrap();
    assert!(app.attention.drafts[&other].text.is_empty());
    app.attention_namespace_changed();
    app.coordinator_target = Ok(None);
    app.use_attention_store(root);
    app.run_command("send --idle 1");
    press(&mut app, KeyCode::Enter);
    let retry = requests(&rx).pop().unwrap();
    assert_eq!(retry.2, original.2);
}

#[test]
fn repeated_pending_decision_commands_share_one_request_and_valid_receipt_stays_known() {
    let (mut app, rx) = app();
    app.current = Some(Sel::Session(1));
    app.state.sessions[0].cwd = Some("/srv/repo".into());
    app.run_command("decide same pending decision");
    let first = requests(&rx).pop().unwrap();
    app.run_command("decide same pending decision");
    assert!(requests(&rx).is_empty());
    reply(&mut app, first.0, json!({"decision":{"id":"known","recipients":[1]}}), None);
    reply(&mut app, first.0, json!({}), Some("late duplicate error"));
    assert!(app.attention.decisions[first.2["key"].as_str().unwrap()].known);
    app.run_command("decide deliberate next decision");
    assert_ne!(requests(&rx).pop().unwrap().2["key"], first.2["key"]);
}

#[test]
fn invalid_private_recovery_files_hold_mutation_without_overwriting_existing_content() {
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{symlink, PermissionsExt},
    };
    let dir = crate::testsupport::TempDir::new("attention-invalid-recovery");
    let root = dir.path().join("attention-desktop");
    let (mut app, _) = app();
    app.use_attention_store(root.clone());
    let file = root.join(format!("{}.json", crate::remote::auth::sha256_hex(app.attention.origin.as_bytes())));
    let canary = dir.write("canary", "synthetic private canary");
    symlink(&canary, &file).unwrap();
    let (mut blocked, rx) = self::app();
    blocked.use_attention_store(root.clone());
    blocked.run_command("send --idle 1 must remain local");
    assert!(requests(&rx).is_empty());
    assert_eq!(std::fs::read_to_string(&canary).unwrap(), "synthetic private canary");
    std::fs::remove_file(&file).unwrap();
    let name = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (mut blocked, rx) = self::app();
    blocked.use_attention_store(root.clone());
    blocked.run_command("send --idle 1 must remain local");
    assert!(requests(&rx).is_empty());
    std::fs::remove_file(&file).unwrap();
    std::fs::write(&file, "malformed recovery").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (mut blocked, rx) = self::app();
    blocked.use_attention_store(root);
    blocked.run_command("send --idle 1 must remain local");
    assert!(requests(&rx).is_empty());
    assert_eq!(std::fs::read_to_string(file).unwrap(), "malformed recovery");
}

#[test]
fn stale_client_reconciles_original_key_after_another_client_resolves_and_replaces_target_ownership() {
    let dir = crate::testsupport::TempDir::new("attention-cross-client-receipts");
    let root = dir.path().join("attention-desktop");
    let (mut first, rx) = app();
    first.use_attention_store(root.clone());
    first.run_command("send --idle 1 original retained body");
    let original = requests(&rx).pop().unwrap();
    let key = original.2["key"].as_str().unwrap().to_owned();
    first.poll_attention(Instant::now() + Duration::from_secs(16));
    requests(&rx);
    let (mut second, other) = app();
    second.use_attention_store(root);
    second.run_command(&format!("attention retry {key}"));
    let reconciled = requests(&other).pop().unwrap();
    assert_eq!(reconciled.2, original.2);
    reply(&mut second, reconciled.0, delivery(1, "old-known", "queued"), None);
    second.run_command("send --idle 1 next deliberate body");
    let newer = requests(&other).pop().unwrap();
    assert_ne!(newer.2["key"], key);
    first.run_command("attention recover");
    requests(&rx);
    first.run_command(&format!("attention retry {key}"));
    let old = requests(&rx).pop().unwrap();
    assert_eq!(old.2, original.2);
    reply(&mut first, old.0, delivery(1, "old-known", "queued"), None);
    first.run_command(&format!("attention retry {}", newer.2["key"].as_str().unwrap()));
    let current = requests(&rx).pop().unwrap();
    assert_eq!(current.2, newer.2);
    reply(&mut first, current.0, delivery(1, "new-known", "queued"), None);
    first.run_command("send --idle 2 unrelated next operation");
    assert_eq!(requests(&rx).pop().unwrap().2["session"], 2);
}

#[test]
fn locally_held_decision_retry_journals_before_transport_and_ambiguous_receipt_preserves_fence() {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let dir = crate::testsupport::TempDir::new("attention-local-decision-retry");
    let root = dir.path().join("attention-desktop");
    let (mut app, rx) = app();
    app.current = Some(Sel::Session(1));
    app.state.sessions[0].cwd = Some("/srv/repo".into());
    app.use_attention_store(root.clone());
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("recovery.lock"))
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    app.run_command("decide original held decision");
    assert!(requests(&rx).is_empty());
    let key = app.attention.decisions.keys().next().unwrap().clone();
    assert!(app.attention.decisions[&key].local_only);
    let original_args = app.attention.decisions[&key].args.clone();
    let foreign = switch_attention_origin(&mut app, Some("other"));
    app.run_command(&format!("attention retry {key}"));
    assert!(requests(&foreign).is_empty());
    let rx = switch_attention_origin(&mut app, None);
    assert!(requests(&rx).is_empty());
    assert_eq!(app.attention.decisions[&key].namespace, app.attention.namespace);
    assert_eq!(app.attention.decisions[&key].args, original_args);
    assert!(app.attention.decisions[&key].local_only);
    drop(lock);
    app.run_command(&format!("attention retry {key}"));
    let sent = requests(&rx).pop().unwrap();
    assert!(!app.attention.decisions[&key].local_only);
    let (mut restored, other) = self::app();
    restored.use_attention_store(root);
    assert!(requests(&other).is_empty());
    assert_eq!(restored.attention.decisions[&key].args, sent.2);
    reply(&mut app, sent.0, json!({}), Some("persisted but acknowledgment lost"));
    app.run_command("decide changed text must remain held");
    assert!(requests(&rx).is_empty());
    app.run_command(&format!("attention retry {key}"));
    assert_eq!(requests(&rx).pop().unwrap().2, sent.2);
}

#[test]
fn decision_wrapper_headroom_is_checked_in_utf8_bytes_without_changing_an_uncertain_original() {
    let (mut app, rx) = app();
    app.current = Some(Sel::Session(1));
    app.state.sessions[0].cwd = Some("/srv/repo".into());
    let oversized = "é".repeat(7951);
    app.run_command(&format!("decide {oversized}"));
    assert!(requests(&rx).is_empty());
    assert!(app.attention.decisions.is_empty());
    let exact = "é".repeat(7950);
    assert_eq!(exact.len(), 15900);
    app.run_command(&format!("decide {exact}"));
    let sent = requests(&rx).pop().unwrap();
    assert_eq!(sent.2["text"], exact);
    reply(&mut app, sent.0, json!({}), Some("reply unknown after durable admission"));
    app.run_command(&format!("decide {oversized}"));
    assert!(requests(&rx).is_empty());
    app.run_command(&format!("decide {exact}"));
    assert_eq!(requests(&rx).pop().unwrap().2, sent.2);
}

#[test]
fn unattempted_restart_draft_fences_captured_pid_but_attempted_key_reconciliation_survives_reopen() {
    let dir = crate::testsupport::TempDir::new("attention-captured-pid");
    let root = dir.path().join("attention-desktop");
    let (mut original, rx) = app();
    original.state.sessions[0].pid = Some(50001);
    original.use_attention_store(root.clone());
    original.run_command("send --idle 1");
    original.handle_input(Event::Paste("unsubmitted captured-process draft".into()));
    press(&mut original, KeyCode::Esc);
    assert!(requests(&rx).is_empty());
    let (mut restored, rx) = app();
    restored.state.sessions[0].pid = Some(50002);
    restored.use_attention_store(root);
    restored.run_command("send --idle 1");
    let key = restored.attention.active_draft.clone().unwrap();
    assert_eq!(restored.attention.drafts[&key].pid, Some(50001));
    press(&mut restored, KeyCode::Enter);
    assert!(requests(&rx).is_empty());
    assert!(!restored.attention.drafts[&key].attempted);
    restored.state.sessions[0].pid = None;
    press(&mut restored, KeyCode::Enter);
    assert!(requests(&rx).is_empty());
    restored.state.sessions[0].pid = Some(50001);
    press(&mut restored, KeyCode::Enter);
    let sent = requests(&rx).pop().unwrap();
    reply(&mut restored, sent.0, json!({}), Some("admission acknowledgment unknown"));
    restored.state.sessions[0].pid = Some(50003);
    restored.state.sessions[0].conversation = Some("replacement-conversation".into());
    restored.state.sessions[0].alive = false;
    press(&mut restored, KeyCode::Enter);
    assert_eq!(requests(&rx).pop().unwrap().2, sent.2);
}

#[test]
fn duplicate_message_uuids_keep_origin_selection_fetch_cancel_and_private_reply_identity() {
    let (mut app, rx) = app();
    let record = |host: &str, session| {
        json!({"id":"same-uuid","host":host,"session":session,
        "state":"held","created_at":1,"updated_at":2,"reason":"fixture"})
    };
    app.state.attention.messages =
        vec![serde_json::from_value(record("alpha", 1)).unwrap(), serde_json::from_value(record("beta", 2)).unwrap()];
    app.open_attention(Tab::Messages);
    requests(&rx);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    let fetch = requests(&rx).pop().unwrap();
    assert_eq!(fetch.2, json!({"id":"same-uuid","host":"beta"}));
    let mut state = app.state.clone();
    state.attention.messages.reverse();
    app.handle_server(ServerMsg::State(state));
    assert_eq!(app.attention_selection().unwrap().1, Some(message_key(Some("beta"), "same-uuid")));
    reply(&mut app, fetch.0, json!({"message":record("alpha", 1),"text":"wrong executor body"}), None);
    assert!(app.attention.private.is_none());
    assert_eq!(app.state.attention.messages.len(), 2);
    press(&mut app, KeyCode::Enter);
    let fetch = requests(&rx).pop().unwrap();
    reply(&mut app, fetch.0, json!({"message":record("beta", 2),"text":"beta private body"}), None);
    assert_eq!(app.attention.private.as_ref().unwrap().1, "beta private body");
    press(&mut app, KeyCode::Char('c'));
    assert_eq!(requests(&rx).pop().unwrap().2, json!({"id":"same-uuid","host":"beta"}));
    let mut state = app.state.clone();
    state.attention.messages.retain(|m| m.host.as_deref() != Some("beta"));
    app.handle_server(ServerMsg::State(state));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('c'));
    assert!(requests(&rx).is_empty());
    assert_eq!(app.attention.reader.as_ref().unwrap().identity, message_key(Some("beta"), "same-uuid"));
    assert_eq!(app.attention.private.as_ref().unwrap().1, "beta private body");
    press(&mut app, KeyCode::Esc);
    assert!(app.attention.private.is_none());
    assert!(matches!(app.dialog, Some(Dialog::Attention { tab: Tab::Messages, sel: usize::MAX })));
}

#[test]
fn remote_idle_retry_retains_captured_origin_after_restart_and_missing_origin_never_guesses() {
    let dir = crate::testsupport::TempDir::new("attention-pinned-origin");
    let root = dir.path().join("attention-desktop");
    let remote = crate::protocol::host_id(7, 1);
    let (mut first, rx) = app();
    first.state.sessions.truncate(1);
    first.state.sessions[0].id = remote;
    first.state.sessions[0].host = Some("alpha".into());
    first.current = Some(Sel::Session(remote));
    first.use_attention_store(root.clone());
    first.run_command("send --idle");
    first.handle_input(Event::Paste("retained remote body".into()));
    press(&mut first, KeyCode::Enter);
    let original = requests(&rx).pop().unwrap();
    assert_eq!(original.2["host"], "alpha");
    assert_eq!(original.2["session"], remote);
    let key = original.2["key"].as_str().unwrap().to_owned();
    let (mut restored, rx) = app();
    restored.state.sessions.truncate(1);
    restored.state.sessions[0].id = remote;
    restored.state.sessions[0].host = Some("beta".into());
    restored.use_attention_store(root);
    assert!(requests(&rx).is_empty());
    restored.run_command(&format!("attention retry {key}"));
    let retry = requests(&rx).pop().unwrap();
    assert_eq!(retry.2, original.2);
    let mut wrong = delivery(remote, "remote-receipt", "queued");
    wrong["message"]["host"] = json!("beta");
    reply(&mut restored, retry.0, wrong, None);
    assert!(!restored.attention.drafts[&key].accepted);
    restored.attention.drafts.get_mut(&key).unwrap().host = None;
    restored.run_command(&format!("attention retry {key}"));
    assert!(requests(&rx).is_empty());
    assert_eq!(restored.attention.drafts[&key].text, "retained remote body");
}

#[test]
fn unavailable_late_reply_never_accepts_or_replays_and_releases_only_its_inflight_marker() {
    for (version, capable) in [(14, true), (15, false), (16, true)] {
        let (mut app, rx) = app();
        app.run_command("send --idle 1 original uncertain operation");
        let original = requests(&rx).pop().unwrap();
        let key = original.2["key"].as_str().unwrap().to_owned();
        app.service_version = version;
        app.state.attention_capable = capable;
        reply(&mut app, original.0, delivery(1, "untrusted-late-receipt", "queued"), None);
        assert!(requests(&rx).is_empty());
        assert!(app.state.attention.messages.is_empty());
        assert!(!app.attention.drafts[&key].accepted);
        assert!(app.attention.drafts[&key].request.is_none());
        assert_eq!(app.attention.drafts[&key].text, "original uncertain operation");
        app.service_version = 17;
        app.state.attention_capable = true;
        press(&mut app, KeyCode::Enter);
        let retry = requests(&rx).pop().unwrap();
        assert_eq!(retry.2, original.2);
        reply(&mut app, retry.0, delivery(1, "reconciled-receipt", "queued"), None);
        assert!(app.attention.drafts[&key].accepted);
    }
}

#[test]
fn native_rules_require_protocol16_without_changing_terminal_rules_or_hook_reply_authority() {
    let (mut app, rx) = app();
    let ordinary = json!({"rules":[
        {"id":"terminal-exact","tool":"Bash","pattern":{"kind":"exact","command":"printf fixture"}},
        {"id":"terminal-glob","tool":"Bash","pattern":{"kind":"argv_glob","tokens":["printf","*"]}}
    ]});
    let mut mixed = ordinary.clone();
    mixed["rules"].as_array_mut().unwrap().push(json!({"id":"native-read","tool":"Read",
        "pattern":{"kind":"native_exact","input":{"file_path":"/repo/docs/guide.md"}}}));
    for version in [14, 15] {
        app.service_version = version;
        let serial = app.fleet_serial;
        assert!(!app.attention_request("attention-supervisor-rules", mixed.clone()));
        assert!(requests(&rx).is_empty());
        assert!(app.attention.pending.is_empty());
        assert_eq!(app.fleet_serial, serial);
        let notice = app.notice.as_ref().unwrap();
        assert!(notice.1 && notice.0.contains("16") && notice.0.contains("update needed"));
    }
    app.service_version = 15;
    assert!(app.attention_request("attention-supervisor-rules", ordinary.clone()));
    assert_eq!(requests(&rx).pop().unwrap().2, ordinary);
    assert!(app.attention_request("attention-supervisor-rules", json!({})));
    assert_eq!(requests(&rx).pop().unwrap().2, json!({}));
    app.service_version = 16;
    assert!(app.attention_request("attention-supervisor-rules", mixed.clone()));
    assert_eq!(requests(&rx).pop().unwrap().2, mixed);
    app.handle_server(ServerMsg::PermissionDecision { request: "synthetic-hook-only".into(), allow: true });
    assert!(requests(&rx).is_empty());
    assert_eq!(app.session(1).unwrap().status, Status::NeedsYou);
    assert!(!app.state.attention.supervisor.enabled);
}

#[test]
fn private_reader_scrolls_long_body_with_hundred_messages_without_moving_its_host_identity() {
    for (width, height) in [(80, 24), (140, 36), (190, 50)] {
        let (mut app, rx) = app();
        app.state.attention.messages = (0..100)
            .map(|i| {
                serde_json::from_value(json!({
                    "id":format!("message-{i}"),"host":if i == 49 {"alpha"} else {"beta"},"session":1,
                    "state":"held","created_at":1,"updated_at":2,"reason":"fixture"
                }))
                .unwrap()
            })
            .collect();
        app.open_attention(Tab::Messages);
        requests(&rx);
        app.dialog = Some(Dialog::Attention { tab: Tab::Messages, sel: 49 });
        let render = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| {
                    crate::view::render(f, app);
                })
                .unwrap();
            terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>()
        };
        render(&mut app);
        let list_offset = app.dialog_viewport.offset;
        press(&mut app, KeyCode::Enter);
        let fetch = requests(&rx).pop().unwrap();
        assert_eq!(fetch.2, json!({"id":"message-49","host":"alpha"}));
        let body = format!(
            "BODY-BEGIN\n{}\n{}\nBODY-END",
            "wrapped-unbroken-".repeat(30),
            (0..150).map(|i| format!("private-line-{i}")).collect::<Vec<_>>().join("\n")
        );
        let metadata = app.state.attention.messages[49].clone();
        reply(&mut app, fetch.0, json!({"message":metadata,"text":body}), None);
        let initial = render(&mut app);
        assert!(initial.contains("BODY-BEGIN"));
        assert!(!initial.contains("BODY-END"));
        assert_eq!(app.attention.reader.as_ref().unwrap().identity, message_key(Some("alpha"), "message-49"));
        press(&mut app, KeyCode::PageDown);
        assert!(app.dialog_viewport.offset > 0);
        press(&mut app, KeyCode::End);
        assert!(render(&mut app).contains("BODY-END"));
        let end = app.dialog_viewport.offset;
        app.handle_input(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.dialog_viewport.offset < end);
        assert_eq!(app.attention_selection().unwrap().1, Some(message_key(Some("alpha"), "message-49")));
        let mut state = app.state.clone();
        state.attention.messages.reverse();
        app.handle_server(ServerMsg::State(state));
        assert_eq!(app.dialog_viewport.offset, end - 1);
        assert_eq!(app.attention.reader.as_ref().unwrap().identity, message_key(Some("alpha"), "message-49"));
        press(&mut app, KeyCode::Home);
        assert!(render(&mut app).contains("BODY-BEGIN"));
        press(&mut app, KeyCode::Esc);
        assert!(app.attention.reader.is_none() && app.attention.private.is_none());
        assert!(matches!(app.dialog, Some(Dialog::Attention { tab: Tab::Messages, sel: 50 })));
        assert_eq!(app.dialog_viewport.offset, list_offset);
        assert!(requests(&rx).is_empty());
        assert_eq!(app.session(1).unwrap().status, Status::NeedsYou);
    }
}

fn switch_attention_origin(app: &mut App, peer: Option<&str>) -> Receiver<ClientMsg> {
    switch_attention_target(
        app,
        Ok(peer.map(|name| crate::config::HostConfig {
            role: Default::default(),
            name: name.into(),
            label: None,
            tag: 7,
            addresses: vec![],
            ssh: None,
            fingerprint: "f".repeat(64),
        })),
    )
}

fn switch_attention_target(
    app: &mut App,
    target: Result<Option<crate::config::HostConfig>, String>,
) -> Receiver<ClientMsg> {
    let sessions = app.state.sessions.clone();
    app.coordinator_target = target;
    let (tx, _) = mpsc::channel();
    let never: Connector = Arc::new(|| panic!("private origin fixture must never connect"));
    app.link = Some(Link { tx, connect: never.clone(), start: never });
    app.stopping_service = true;
    app.coordinator_changed();
    assert!(app.attention.active_draft.is_none());
    assert!(app.dialog.is_none());
    let (tx, rx) = mpsc::channel();
    app.out = tx;
    app.service_version = 17;
    let state = DeckState { sessions, attention_capable: true, ..Default::default() };
    app.handle_server(ServerMsg::State(state));
    rx
}

#[test]
fn unsent_composer_is_saved_under_original_origin_before_switch_and_requires_explicit_reopen() {
    let dir = crate::testsupport::TempDir::new("attention-unsent-origin");
    let root = dir.path().join("private");
    let (mut app, rx) = app();
    app.use_attention_store(root.clone());
    let origin = app.attention.origin.clone();
    app.run_command("send --idle 1");
    app.handle_input(Event::Paste("never submitted original composer".into()));
    let key = app.attention.active_draft.clone().unwrap();
    let captured_generation = app.attention.drafts[&key].spawned_ms;
    assert!(!app.attention.drafts[&key].attempted);
    assert!(requests(&rx).is_empty());
    let other = switch_attention_origin(&mut app, Some("other"));
    assert!(requests(&other).is_empty());
    app.run_command("send --idle 1");
    let other_key = app.attention.active_draft.clone().unwrap();
    assert_ne!(other_key, key);
    assert!(app.attention.drafts[&other_key].text.is_empty());
    app.handle_input(Event::Paste("other origin composer".into()));
    assert!(requests(&other).is_empty());
    let returned = switch_attention_origin(&mut app, None);
    assert_eq!(app.attention.origin, origin);
    assert!(requests(&returned).is_empty());
    app.run_command("send --idle 1");
    assert_eq!(app.attention.active_draft.as_deref(), Some(key.as_str()));
    assert_eq!(app.attention.drafts[&key].text, "never submitted original composer");
    assert_eq!(app.attention.drafts[&key].spawned_ms, captured_generation);
    assert!(!app.attention.drafts[&key].attempted);
    assert!(requests(&returned).is_empty());
    let (mut restarted, restarted_rx) = self::app();
    restarted.use_attention_store(root);
    assert!(requests(&restarted_rx).is_empty());
    assert!(restarted.attention.active_draft.is_none());
    restarted.run_command("send --idle 1");
    assert_eq!(restarted.attention.active_draft.as_deref(), Some(key.as_str()));
    assert_eq!(restarted.attention.drafts[&key].text, "never submitted original composer");
    assert!(requests(&restarted_rx).is_empty());
}

#[test]
fn failed_origin_save_preserves_unsent_ram_draft_without_rebinding_it_to_another_coordinator() {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    for stale_disk in [false, true] {
        let dir = crate::testsupport::TempDir::new("attention-unsent-save-held");
        let root = dir.path().join("private");
        let (mut app, original_rx) = app();
        app.use_attention_store(root.clone());
        app.run_command("send --idle 1");
        app.handle_input(Event::Paste("RAM retained".into()));
        if stale_disk {
            press(&mut app, KeyCode::Esc);
            app.run_command("send --idle 1");
        }
        press(&mut app, KeyCode::Char(' '));
        app.handle_input(Event::Paste("composer".into()));
        let key = app.attention.active_draft.clone().unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(root.join("recovery.lock"))
            .unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        app.current = Some(Sel::Session(1));
        app.state.sessions[0].cwd = Some("/srv/repo".into());
        app.run_command("decide immutable locally held decision");
        assert!(requests(&original_rx).is_empty());
        let decision_key = app.attention.decisions.keys().next().unwrap().clone();
        let decision_args = app.attention.decisions[&decision_key].args.clone();
        assert!(app.attention.decisions[&decision_key].local_only);
        assert!(!app.attention.decisions[&decision_key].known);
        let invalid = switch_attention_target(&mut app, Err("synthetic invalid coordinator selection".into()));
        assert!(app.attention.store_failed);
        assert!(requests(&invalid).is_empty());
        let other = switch_attention_origin(&mut app, Some("other"));
        app.attention.active_draft = Some(key.clone());
        app.submit_idle_draft();
        assert!(requests(&other).is_empty());
        assert!(app.notice.as_ref().unwrap().0.contains("previous coordinator"));
        app.run_command(&format!("attention retry {decision_key}"));
        assert!(requests(&other).is_empty());
        assert_ne!(app.attention.decisions[&decision_key].namespace, app.attention.namespace);
        drop(lock);
        let returned = switch_attention_origin(&mut app, None);
        app.run_command("send --idle 1");
        assert_eq!(app.attention.active_draft.as_deref(), Some(key.as_str()));
        assert_eq!(app.attention.drafts[&key].text, "RAM retained composer");
        assert!(!app.attention.drafts[&key].attempted);
        assert!(requests(&returned).is_empty());
        assert_eq!(app.attention.decisions[&decision_key].namespace, app.attention.namespace);
        assert_eq!(app.attention.decisions[&decision_key].args, decision_args);
        assert!(app.attention.decisions[&decision_key].local_only);
        assert!(!app.attention.decisions[&decision_key].known);
        app.run_command(&format!("attention retry {decision_key}"));
        let explicit = requests(&returned).pop().unwrap();
        assert_eq!(explicit.1, "attention-decide");
        assert_eq!(explicit.2, decision_args);
        assert!(!app.attention.decisions[&decision_key].local_only);
        assert!(!app.attention.drafts[&key].attempted);
    }
}

#[test]
fn idle_send_requires17_and_frozen_expected_target_survives_pre_admission_loss_reopen_and_restart() {
    let dir = crate::testsupport::TempDir::new("attention-frozen-target");
    let root = dir.path().join("private");
    let (mut app, rx) = app();
    app.use_attention_store(root.clone());
    app.run_command("send --idle 1");
    app.handle_input(Event::Paste("immutable identity body".into()));
    let key = app.attention.active_draft.clone().unwrap();
    let captured = app.session(1).unwrap().clone();
    for version in [15, 16] {
        app.service_version = version;
        let serial = app.fleet_serial;
        press(&mut app, KeyCode::Enter);
        assert!(requests(&rx).is_empty());
        assert_eq!(app.fleet_serial, serial);
        assert!(!app.attention.drafts[&key].attempted);
        assert!(app.notice.as_ref().unwrap().0.contains("17"));
    }
    app.service_version = 17;
    press(&mut app, KeyCode::Enter);
    let first = requests(&rx).pop().unwrap();
    assert_eq!(
        first.2["expected_target"],
        json!({"spawned_ms":captured.spawned_ms,
        "pid":captured.pid,"conversation":captured.conversation})
    );
    // No admission receipt exists: the service must compare this ORIGINAL tuple to its reopened target.
    reply(&mut app, first.0, json!({}), Some("transport lost before admission"));
    app.state.sessions[0].spawned_ms += 1;
    app.state.sessions[0].pid = Some(55555);
    app.state.sessions[0].conversation = Some("replacement-known-conversation".into());
    press(&mut app, KeyCode::Enter);
    let retry = requests(&rx).pop().unwrap();
    assert_eq!(retry.2, first.2);
    reply(&mut app, retry.0, json!({}), Some("captured target differs; no admission"));
    assert!(!app.attention.drafts[&key].accepted);
    assert_eq!(app.attention.drafts[&key].text, "immutable identity body");
    let (mut restarted, rx) = self::app();
    restarted.state.sessions[0].spawned_ms += 10;
    restarted.use_attention_store(root);
    assert!(requests(&rx).is_empty());
    restarted.run_command(&format!("attention retry {key}"));
    let original_receipt_lookup = requests(&rx).pop().unwrap();
    assert_eq!(original_receipt_lookup.2, first.2);
    reply(&mut restarted, original_receipt_lookup.0, delivery(1, "already-admitted-original", "held"), None);
    assert!(restarted.attention.drafts[&key].accepted);
}

#[test]
fn legacy_generation_zero_drafts_hold_without_inference_or_replay() {
    let dir = crate::testsupport::TempDir::new("attention-legacy-target");
    let root = dir.path().join("private");
    let (mut original, _) = app();
    original.use_attention_store(root.clone());
    original.run_command("send --idle 1");
    original.handle_input(Event::Paste("legacy retained body".into()));
    let key = original.attention.active_draft.clone().unwrap();
    press(&mut original, KeyCode::Esc);
    let file = root.join(format!("{}.json", crate::remote::auth::sha256_hex(original.attention.origin.as_bytes())));
    let mut state: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    state["drafts"][&key].as_object_mut().unwrap().remove("spawned_ms");
    std::fs::write(&file, serde_json::to_vec(&state).unwrap()).unwrap();
    let (mut restored, rx) = app();
    restored.use_attention_store(root);
    restored.run_command("send --idle 1");
    assert_eq!(restored.attention.active_draft.as_deref(), Some(key.as_str()));
    press(&mut restored, KeyCode::Enter);
    assert!(requests(&rx).is_empty());
    assert_eq!(restored.attention.drafts[&key].spawned_ms, 0);
    assert_eq!(restored.attention.drafts[&key].text, "legacy retained body");
    assert!(!restored.attention.drafts[&key].attempted);
    assert!(restored.notice.as_ref().unwrap().0.contains("legacy"));
}
