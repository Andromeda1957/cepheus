use super::*;
use crate::protocol::write_msg;
use crate::testsupport::TempDir;
use serde_json::json;

#[test]
fn busy_frame_writer_sleeps_until_completion_without_delaying_other_clients() {
    let dir = TempDir::new("frame-backpressure");
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx.clone());
    let (socket, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    accept_client(socket, 1, tx.clone());
    let first = events.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(first, Ev::ClientNew { id: 1, .. }));
    svc.handle(first);
    while let Ok(event) = events.try_recv() {
        svc.handle(event);
    }

    let (session_events, _ignored) = mpsc::channel();
    let argv = vec!["sh".into(), "-c".into(), "exit 0".into()];
    let mut hosted = crate::session::Hosted::spawn(
        crate::session::SpawnSpec {
            id: 1,
            argv: &argv,
            cwd: dir.path(),
            env_set: &[],
            env_remove: &[],
            cols: 80,
            rows: 24,
            scrollback: 100,
        },
        session_events,
    )
    .unwrap();
    hosted.pause().unwrap();
    let mut session = fake_session(1, Tool::Shell);
    session.hosted = Some(hosted);
    svc.sessions.insert(1, session);
    let busy = svc.clients[&1].frame_busy.clone();
    let client = svc.clients.get_mut(&1).unwrap();
    client.attached = Some(1);
    busy.store(true, Ordering::SeqCst);
    svc.last_pump = Instant::now() - FRAME_INTERVAL;

    assert_eq!(svc.next_timeout(), IDLE_WAIT, "a blocked writer must not force frame-rate polling");

    let (out, fast_frames) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 2, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    let fast = svc.clients.get_mut(&2).unwrap();
    fast.attached = Some(crate::protocol::host_id(2, 1));
    fast.pending_frame = Some(b"newest remote frame".to_vec());
    assert!(svc.next_timeout() <= FRAME_INTERVAL, "an independent ready remote frame must stay prompt");
    svc.pump_frames();
    assert!(matches!(fast_frames.try_recv(), Ok(Out::Frame(buf)) if buf == b"newest remote frame"));
    assert!(busy.load(Ordering::SeqCst), "the slow client remains independently blocked");

    let initial = encode(&ServerMsg::Notice { text: "first frame".into(), error: false });
    svc.clients[&1].out.send(Out::Frame(initial)).unwrap();
    loop {
        let wake = events.recv_timeout(Duration::from_secs(2)).expect("frame writer completion must wake the service");
        let ready = matches!(wake, Ev::FrameReady);
        svc.handle(wake);
        if ready {
            break;
        }
    }
    assert!(!busy.load(Ordering::SeqCst));
    assert!(svc.next_timeout() <= FRAME_INTERVAL, "completion promptly schedules the newest local frame");
    svc.pump_frames();
    assert!(matches!(read_msg::<_, ServerMsg>(&mut peer).unwrap(), Some(ServerMsg::Notice { .. })));
    assert!(matches!(read_msg::<_, ServerMsg>(&mut peer).unwrap(), Some(ServerMsg::Frame { id: 1, .. })));
}

#[test]
fn newer_remote_frame_supersedes_a_queued_older_frame() {
    let dir = TempDir::new("remote-frame-order");
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let (out, frames) = mpsc::channel();
    let busy = Arc::new(AtomicBool::new(false));
    svc.handle(Ev::ClientNew { id: 1, out, frame_busy: busy.clone() });
    let client = svc.clients.get_mut(&1).unwrap();
    client.attached = Some(crate::protocol::host_id(2, 1));
    client.pending_frame = Some(b"old".to_vec());

    svc.on_host(crate::hosts::link::HostEv::UpFrame { cid: 1, tag: 2, buf: b"new".to_vec() });
    assert!(matches!(frames.try_recv(), Ok(Out::Frame(buf)) if buf == b"new"));
    busy.store(false, Ordering::SeqCst);
    svc.pump_host_frames();
    assert!(frames.try_recv().is_err(), "the older queued frame must not overwrite the newer one");
}

pub(crate) fn options(dir: &TempDir, script: &str) -> ServiceOptions {
    let mut config = Config {
        claude_command: vec!["sh".into(), "-c".into(), script.into()],
        codex_command: vec!["sh".into(), "-c".into(), script.into()],
        notifications: false,
        claude_hooks: false,
        codex_notify: false,
        palette_file: Some(dir.path().join("no-kitty.conf")),
        // Never the owner's runserver on port 8000, which is the owner's alone.
        dev_server_command: vec!["sh".into(), "-c".into(), "echo serving; exec sleep 30".into()],
        ..Config::default()
    };
    // Session behavior tests must not depend on the build host's live pressure.
    // Admission tests replace these fixture-owned readings explicitly.
    let probe = dir.path().join("proc");
    fs::create_dir_all(probe.join("pressure")).unwrap();
    fs::write(probe.join("meminfo"), "MemTotal: 16777216 kB\nMemAvailable: 12582912 kB\n").unwrap();
    fs::write(probe.join("pressure/memory"), "full avg10=0\n").unwrap();
    fs::write(probe.join("pressure/cpu"), "some avg60=0\n").unwrap();
    fs::write(probe.join("loadavg"), "0 0 0 1/1 1\n").unwrap();
    config.scheduler.probe.root = Some(probe);
    ServiceOptions {
        socket: dir.path().join("run/deck.sock"),
        state_dir: dir.path().join("state"),
        config,
        claude_dir: dir.path().join("claude"),
        codex_dir: dir.path().join("codex"),
        antigravity_dir: dir.path().join("agy"),
        grok_home: dir.path().join("grok"),
        opencode_db: dir.path().join("opencode.db"),
        usage_file: dir.path().join("state/claude-usage.json"),
        exe: PathBuf::from("/opt/agent deck/cepheus"),
        start_dir: dir.path().to_path_buf(),
        monitor: false,
        signals: false,
        update: false,
        handover: None,
        config_file: None,
    }
}

#[test]
fn hub_enter_policy_runs_once_per_minute_and_reports_changed_failures() {
    let dir = TempDir::new("hub-enter-upkeep");
    let mut opts = options(&dir, "true");
    let mirror = dir.path().join("windows/claude");
    opts.config.plugin_settings.insert(
        "login-sync".into(),
        json!({"credential_mirrors":[{
            "claude_dir":mirror,"codex_dir":dir.path().join("windows/codex")
        }]}),
    );
    let paths =
        crate::hosts::logins::LoginPaths { claude_dir: opts.claude_dir.clone(), codex_dir: opts.codex_dir.clone() };
    let (tx, _) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let now = Instant::now();
    svc.upkeep_claude_policy(now);
    assert_eq!(svc.errors.borrow().len(), 1);
    assert!(!paths.claude_dir.exists(), "missing mirrors preflight before any default-home write");
    svc.upkeep_claude_policy(now + Duration::from_secs(60));
    assert_eq!(svc.errors.borrow().len(), 1, "identical failures are reported once");
    fs::create_dir_all(&mirror).unwrap();
    svc.upkeep_claude_policy(now + Duration::from_secs(119));
    assert!(!paths.claude_dir.exists(), "a second peer or event cannot accelerate local upkeep");
    svc.upkeep_claude_policy(now + Duration::from_secs(120));
    assert!(crate::providers::claude_keybindings::compliant(&paths, &svc.opts.config).unwrap());
    let file = paths.claude_dir.join("keybindings.json");
    let stamp = crate::util::file_stamp(&file);
    svc.upkeep_claude_policy(now + Duration::from_secs(121));
    assert_eq!(crate::util::file_stamp(&file), stamp);
    fs::write(&file, r#"{"bindings":[]}"#).unwrap();
    svc.upkeep_claude_policy(now + Duration::from_secs(179));
    assert!(!crate::providers::claude_keybindings::compliant_target(&paths.claude_dir).unwrap());
    svc.upkeep_claude_policy(now + Duration::from_secs(180));
    assert!(crate::providers::claude_keybindings::compliant_target(&paths.claude_dir).unwrap());
    fs::write(&file, "invalid JSON").unwrap();
    svc.upkeep_claude_policy(now + Duration::from_secs(240));
    assert_eq!(svc.errors.borrow().len(), 2, "a failure after recovery is visible again");
    svc.upkeep_claude_policy(now + Duration::from_secs(300));
    assert_eq!(svc.errors.borrow().len(), 2);
    assert_eq!(fs::read_to_string(file).unwrap(), "invalid JSON");
}

/// The transcript Claude writes once a conversation begins, in the test's
/// Claude folder ([`options`]): without one, an ended Claude session had no
/// conversation and leaves the list.
fn claude_transcript(dir: &TempDir, conversation: &str) {
    let log = claude::transcript_path(&dir.path().join("claude"), &dir.path().to_string_lossy(), conversation);
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, "{\"type\":\"user\"}\n").unwrap();
}

pub(super) fn fake_session(id: u64, tool: Tool) -> Sess {
    Sess {
        account_root: None,
        account: None,
        id,
        tool,
        hosted: None,
        alive: true,
        exit_code: None,
        conversation: None,
        term_title: None,
        facts: SessionFacts::default(),
        tracker: Tracker::new(0, true),
        created_at: 0,
        spawned_ms: 0,
        closing: None,
        remove_when_ended: false,
        title: tool.label().into(),
        name: None,
        retry: AutoContinue::default(),
        screen_read: None,
        update_notice: false,
        left: Vec::new(),
        chat: ChatWatch::default(),
        inputs: 0,
        recovery: recovery::ClientRecovery::default(),
        owner_draft: false,
        cwd: None,
        command: Vec::new(),
        job: None,
    }
}

fn failed_codex_screen(draft: &str) -> Vec<String> {
    format!("■ Automatic reconnect could not restore this session. Your draft is still editable.\n■ Automatic reconnect timed out while resuming the conversation.\n• Reconnect failed — check the endpoint, then relaunch (1m 2s)\n\n› {draft}\n\n  GPT-6.1-Sol xhigh · Resume task · weekly 26% left\n  ^c quit")
        .lines().map(str::to_string).collect()
}

#[test]
fn codex_client_recovery_keeps_collapsed_pastes_and_acknowledges_duplicate_messages_once() {
    let dir = TempDir::new("codex-recovery-input");
    let full = "original pasted content ".repeat(2000);
    let mut captured = recovery::ClientRecovery::default();
    captured.typed(&crate::keys::encode_message(&full), Some(String::new()));
    captured.typed("\r", None);
    captured.typed(&crate::keys::encode_message(&full), None);
    captured.typed("\r", None);
    assert_eq!(captured.pending, vec![full.clone(), full.clone()], "capture precedes Codex collapsing the paste");
    let meta = json!({"type":"session_meta","payload":{"cli_version":"0.166.0","instructions":"x".repeat(64 * 1024)}});
    let ack = json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":full}]}}});
    let path = dir.write("rollout.jsonl", format!("{meta}\n{ack}\n"));
    captured.rollout = Some(path);
    captured.reconcile().unwrap();
    assert_eq!(captured.pending, vec![full]);
    captured.reconcile().unwrap();
    assert_eq!(captured.pending.len(), 1, "an already-read user record cannot acknowledge another send");

    // A photo-only phone send is an absolute path paste. Codex turns that
    // path into a LocalImage plus a marker, while real slash commands stay local.
    let photo = "/private/uploads/example  photo.jpg";
    let mut image = recovery::ClientRecovery::default();
    image.typed(&crate::keys::encode_message(photo), Some(String::new()));
    image.typed("\r", None);
    assert_eq!(image.pending, vec![photo]);
    let receipt = json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"local_image","path":photo},{"type":"text","text":"[Image #1]","text_elements":[{"byte_range":{"start":0,"end":10},"placeholder":"[Image #1]"}]}]}}});
    image.rollout = Some(dir.write("photo.jsonl", format!("{meta}\n{receipt}\n")));
    image.reconcile().unwrap();
    assert!(image.pending.is_empty(), "the native LocalImage receipt acknowledges the original path once");
    image.typed(&crate::keys::encode_message("/clear"), None);
    image.typed("\r", None);
    assert!(image.pending.is_empty(), "a real slash command has no user-message receipt");

    let mut caption = recovery::ClientRecovery::default();
    caption.typed(&crate::keys::encode_message(photo), Some(String::new()));
    caption.typed(" caption", None);
    caption.typed("\r", None);
    let receipt = json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"local_image","path":photo},{"type":"text","text":"[Image #1] caption","text_elements":[{"byte_range":{"start":0,"end":10},"placeholder":"[Image #1]"}]}]}}});
    caption.rollout = Some(dir.write("caption.jsonl", format!("{meta}\n{receipt}\n")));
    assert!(caption.reconcile().is_err(), "an unknown image transformation cannot authorize replay");
    assert!(caption.blocked.is_some());
    assert_eq!(caption.pending, vec![format!("{photo} caption")], "the original payload stays in custody");
}

#[test]
fn codex_client_failure_detection_excludes_quoted_and_transient_errors_and_keeps_multiline_drafts() {
    use crate::providers::recovery::*;
    let screen = failed_codex_screen("read this\n\n  /private/uploads/a.jpg\n  and this");
    assert!(codex_reconnect_failed(&screen));
    assert_eq!(codex_draft(&screen, Some((10, 7))), Some("read this\n\n/private/uploads/a.jpg\nand this".into()));
    let mut quote = screen.clone();
    *quote.last_mut().unwrap() = "  ← for agents · ? for shortcuts".into();
    assert!(!codex_reconnect_failed(&quote), "old or quoted errors lack the failed-client shortcut footer");
    let mut transient = failed_codex_screen("");
    transient[0] = "• Reconnecting… 1/5".into();
    transient[1].clear();
    transient[2].clear();
    assert!(!codex_reconnect_failed(&transient));
    let collapsed = failed_codex_screen("[Pasted Content 50000 chars]");
    assert_eq!(codex_draft(&collapsed, Some((20, 4))), None);
    assert_eq!(codex_draft_nonempty(&collapsed, Some((20, 4))), Some(true));
    assert_eq!(codex_draft(&failed_codex_screen("Ask Codex to do anything"), Some((2, 4))), Some(String::new()));
    let dir = TempDir::new("codex-image-draft");
    let image = dir.write("photo.jpg", "fixture");
    assert!(codex_image_draft_recoverable(&failed_codex_screen("[Image #1]"), Some((12, 4)), image.to_str().unwrap()));
    assert!(!codex_image_draft_recoverable(
        &failed_codex_screen("[Image #1] caption"),
        Some((20, 4)),
        &format!("{} caption", image.display())
    ));
}

#[test]
fn a_fresh_codex_client_binds_its_first_input_receipt_before_any_further_typing() {
    let dir = TempDir::new("codex-new-input-receipt");
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let mut session = fake_session(1, Tool::Codex);
    session.recovery.draft = Some(String::new());
    svc.sessions.insert(1, session);
    svc.capture_client_input(1, &crate::keys::encode_message("first request"));
    svc.capture_client_input(1, "\r");
    assert_eq!(svc.sessions[&1].recovery.pending, vec!["first request"]);
    let id = "019a0000-0000-7000-8000-000000000001";
    let meta = json!({"type":"session_meta","payload":{"cli_version":"0.166.0"}});
    let user = json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"first request"}]}}});
    dir.write(&format!("codex/sessions/2026/11/05/rollout-initial-{id}.jsonl"), format!("{meta}\n{user}\n"));
    svc.sessions.get_mut(&1).unwrap().conversation = Some(id.into());
    svc.bind_client_rollouts();
    assert!(
        svc.sessions[&1].recovery.pending.is_empty(),
        "the accepted first request must never replay after discovery"
    );
}

#[test]
fn failed_codex_clients_relaunch_automatically_with_the_same_identity_and_full_input() {
    let dir = TempDir::new("codex-client-recovery");
    let script = dir.write("codex-client.py", r#"
import os, pathlib, sys, termios, tty
key = os.environ['CEPHEUS_SESSION']
marker = pathlib.Path('started-' + key)
tries = int(marker.read_text()) if marker.exists() else 0
marker.write_text(str(tries + 1))
tty.setraw(0)
if tries >= (2 if key == '1' else 1):
    print('\x1b[?2004h\x1b[2J\x1b[H\r\n› \x1b[2mAsk Codex to do anything\x1b[0m\r\n\r\n  GPT-6.1-Sol xhigh · Resume task · weekly 26% left\r\n\x1b[2;3H', end='', flush=True)
    with open('received-' + key, 'ab', buffering=0) as out:
        while True:
            out.write(os.read(0, 65536))
else:
    draft = '[Pasted Content 50000 chars]' if key == '3' else ('[Image #1] caption' if key == '5' else ('held caption' if key == '6' else ('editable draft\r\n\r\n  /private/uploads/a.jpg' if key == '1' and tries == 0 else 'Ask Codex to do anything')))
    print('\x1b[?2004h\x1b[2J\x1b[H■ Automatic reconnect could not restore this session. Your draft is still editable.\r\n■ Automatic reconnect timed out while resuming the conversation.\r\n• Reconnect failed — check the endpoint, then relaunch (1m 2s)\r\n\r\n› ' + draft + '\r\n\r\n  GPT-6.1-Sol xhigh · Resume task · weekly 26% left\r\n  ^c quit' + ('\x1b[5;15H' if key == '6' else '\x1b[5;3H'), end='', flush=True)
    while True:
        os.read(0, 65536)
"#);
    let mut opts = options(&dir, "true");
    opts.config.codex_command = vec!["python3".into(), "-u".into(), script.to_string_lossy().into_owned()];
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let conversation = "019a0000-0000-7000-8000-000000000001";
    for _ in 0..6 {
        svc.create(Tool::Codex, Some(conversation.into())).unwrap();
    }
    for id in 1..=6 {
        wait_for_screen(&svc, id, "^c quit");
    }
    let original_pids: Vec<_> = (1..=5).map(|id| svc.sessions[&id].hosted.as_ref().unwrap().pid).collect();
    svc.sessions.get_mut(&1).unwrap().recovery.draft = Some("editable draft\n/private/uploads/a.jpg".into());
    svc.sessions.get_mut(&1).unwrap().recovery.pending =
        vec!["first queued message".into(), "second queued message".into()];
    svc.sessions.get_mut(&2).unwrap().facts.busy = Some(true);
    svc.sessions.get_mut(&4).unwrap().recovery.attempts_ms = vec![util::now_ms(); 4];
    svc.sessions.get_mut(&4).unwrap().facts.busy = Some(true);
    svc.sessions.get_mut(&5).unwrap().recovery.draft = Some("/private/uploads/a.jpg caption".into());
    svc.refresh_status(Instant::now());
    svc.paste_file(6, "held.jpg", "Zml4dHVyZQ==").unwrap();
    svc.client_input(6, "\x1b").unwrap();
    assert!(svc.sessions[&5].recovery.blocked.is_some(), "an image caption first preserves its client");
    // Session 3 was inherited with collapsed text the deck never captured.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut buffered = false;
    loop {
        while let Ok(event) = events.try_recv() {
            svc.handle(event);
        }
        svc.refresh_status(Instant::now());
        if !buffered
            && svc.sessions[&1].recovery.phase == Some(recovery::Phase::Replaying)
            && svc.pending_submits.iter().any(|p| p.id == 1)
        {
            let generation = svc.sessions[&1].inputs;
            svc.handle_client(0, ClientMsg::Input { id: 1, data: " added".into() });
            assert_eq!(svc.sessions[&1].inputs, generation, "buffered edits must not cancel a replay's delayed Enter");
            buffered = true;
        }
        svc.continues_due(Instant::now());
        svc.submit_due(Instant::now());
        let one = dir.path().join("received-1");
        let two = dir.path().join("received-2");
        if fs::read_to_string(&one).is_ok_and(|text| text.contains("editable draft"))
            && fs::read_to_string(&two).is_ok_and(|text| text.contains(&svc.opts.config.resume_message))
            && svc.sessions[&6].recovery.phase == Some(recovery::Phase::Starting)
            && dir.path().join("received-6").is_file()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "recovery did not finish: {:?}",
            svc.sessions.iter().map(|(id, s)| (*id, &s.recovery, s.tracker.status)).collect::<Vec<_>>()
        );
        thread::sleep(Duration::from_millis(20));
    }
    for id in 1..=2 {
        assert!(svc.sessions[&id].alive);
        assert_eq!(svc.sessions[&id].conversation.as_deref(), Some(conversation));
        assert_ne!(svc.sessions[&id].hosted.as_ref().unwrap().pid, original_pids[id as usize - 1]);
    }
    assert_eq!(
        svc.sessions[&3].hosted.as_ref().unwrap().pid,
        original_pids[2],
        "unrecoverable collapsed input preserves its client"
    );
    assert!(svc.sessions[&3].recovery.blocked.is_some());
    assert_eq!(svc.sessions[&4].hosted.as_ref().unwrap().pid, original_pids[3]);
    assert_eq!(svc.sessions[&4].recovery.phase, None, "retry exhaustion must not block future deck updates");
    assert_eq!(svc.sessions[&4].tracker.status, Status::NeedsYou, "failed transport wins over stale Thinking state");
    let got = fs::read_to_string(dir.path().join("received-1")).unwrap();
    assert_eq!(
        got,
        "\x1b[200~first queued message\x1b[201~\r\x1b[200~second queued message\x1b[201~\r\x1b[200~editable draft\n/private/uploads/a.jpg added\x1b[201~"
    );
    assert!(buffered);
    assert!(svc.sessions[&6].recovery.paused, "owner Stop survives the actual failed-client relaunch");
    assert!(svc.sessions[&6].recovery.replay.is_empty(), "held input waits for fresh receipts before building replay");
    assert!(fs::read(dir.path().join("received-6")).unwrap().is_empty());
    assert_eq!(svc.sessions[&6].recovery.draft.as_deref(), Some("held caption"));
    assert_eq!(svc.sessions[&6].recovery.draft_files.len(), 1);
    assert_eq!(svc.sessions[&5].hosted.as_ref().unwrap().pid, original_pids[4], "the image caption's client is kept");
    assert!(svc.sessions[&5].recovery.blocked.is_some());
    assert!(!dir.path().join("received-5").exists());
    for id in 1..=6 {
        svc.close(id);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while svc.sessions.values().any(|s| s.alive) {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(50)) {
            svc.handle(event);
        }
        assert!(Instant::now() < deadline);
    }
}

#[test]
fn a_claude_title_is_never_its_agent_name() {
    // Owner, 2026-10-26: "the claude sessions dont get a proper title". Claude
    // sets the terminal title to the agent's name, so after its folder-derived
    // names were skipped every hosted Claude read "implementer" (owner
    // screenshot, 2026-10-26).
    let mut s = fake_session(1, Tool::Claude);
    s.term_title = Some("implementer".into());
    s.facts = SessionFacts {
        agent: Some("implementer".into()),
        first_prompt: Some("build the deck".into()),
        ..SessionFacts::default()
    };
    assert_eq!(s.display_title(&[], &mut Titles::off()), "build the deck");
    s.facts.ai_title = Some("Deck restart key".into());
    assert_eq!(
        s.display_title(&[], &mut Titles::off()),
        "Deck restart key",
        "Claude's own title comes before the first prompt"
    );
    s.facts.ai_title = None;
    s.term_title = Some("Port the parser".into());
    assert_eq!(
        s.display_title(&[], &mut Titles::off()),
        "Port the parser",
        "a terminal title that is not the agent's name stays"
    );
    // Before its transcript names the agent (no prompt yet), the agents the
    // repository defines do; a title kept as "implementer" goes too (owner
    // screenshot, 2026-10-26: "the titles are still a bit funky").
    let dir = TempDir::new("svc-agent-names");
    dir.write("repo/.claude/agents/implementer.md", "---\nname: implementer\n---\n");
    dir.write("repo/.claude/agents/notes.txt", "not an agent");
    dir.write("home/agents/director.md", "---\nname: director\n---\n");
    let agents = claude::agent_names(&[dir.path().join("repo/.claude"), dir.path().join("home")]);
    assert_eq!(agents, ["director", "implementer"]);
    let mut fresh = fake_session(2, Tool::Claude);
    fresh.term_title = Some("implementer".into());
    fresh.title = "implementer".into();
    assert_eq!(fresh.display_title(&agents, &mut Titles::off()), "Claude");
    fresh.facts.first_prompt = Some("port the parser to rust".into());
    assert_eq!(fresh.display_title(&agents, &mut Titles::off()), "port the parser to rust");
}

#[test]
fn followup_messages_preserve_the_task_title() {
    // Owner, 2026-10-26: "titles dont update on clear or when given a new task".
    let mut s = fake_session(1, Tool::Claude);
    s.term_title = Some("implementer".into());
    s.facts = SessionFacts {
        agent: Some("implementer".into()),
        first_prompt: Some("build the deck".into()),
        latest_prompt: Some("make the restart key".into()),
        ..SessionFacts::default()
    };
    assert_eq!(s.display_title(&[], &mut Titles::off()), "build the deck");
    // Codex sets its terminal title itself; the task comes first, and a
    // thread name the monitor still reports (not outdated) before that.
    let mut c = fake_session(2, Tool::Codex);
    c.term_title = Some("project".into());
    c.facts = SessionFacts {
        first_prompt: Some("finish the plan".into()),
        latest_prompt: Some("merge everything to master".into()),
        ..SessionFacts::default()
    };
    assert_eq!(c.display_title(&[], &mut Titles::off()), "finish the plan");
    c.facts.meta_title = Some("Merge everything to master".into());
    assert_eq!(c.display_title(&[], &mut Titles::off()), "Merge everything to master");
}

#[test]
fn a_long_title_from_the_chat_shows_as_its_summary_and_a_given_name_as_it_is() {
    // Owner, 2026-11-01: "titles should be a 2-4 word summary".
    let dir = TempDir::new("svc-short-titles");
    let summarizer = vec!["sh".into(), "-c".into(), "cat >/dev/null; echo 'Fix broken Android deck app.'".into()];
    let mut titles = Titles::new(Some(summarizer), Some(dir.path().to_path_buf()));
    let mut s = fake_session(1, Tool::Claude);
    s.facts.latest_prompt =
        Some("the android deck app looks really messed up, it was supposed to be based off the claude app".into());
    let long = s.facts.latest_prompt.clone().unwrap();
    assert_eq!(s.display_title(&[], &mut titles), long, "the prompt shows until its summary comes");
    let started = std::time::Instant::now();
    while !titles.collect() && started.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(s.display_title(&[], &mut titles), "Fix broken Android deck");
    s.facts.latest_prompt = Some("install font tools".into());
    assert_eq!(s.display_title(&[], &mut titles), "install font tools", "four words or fewer stay");
    s.name = Some("My own long name for this session".into());
    assert_eq!(s.display_title(&[], &mut titles), "My own long name for this session");
}

#[test]
fn parent_session_variables_never_reach_hosted_programs() {
    // Names a Claude Code session exports to its tools (seen 2026-10-25), and
    // Codex's equivalents: a hosted Claude that inherited the child-session
    // marker turned transcript saving off.
    let parent = [
        "CLAUDECODE",
        "CLAUDE_CODE_BRIDGE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_CODE_SESSION_ATTENDED",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_EFFORT",
        "CLAUDE_PID",
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "CODEX_SANDBOX_NETWORK_DISABLED",
        "KITTY_WINDOW_ID",
        "CEPHEUS_SESSION",
    ];
    let config = [
        "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "ANTHROPIC_MODEL",
        "PATH",
        "HOME",
    ];
    for name in parent {
        assert!(inherited_session_var(name), "{name} must be removed");
    }
    for name in config {
        assert!(!inherited_session_var(name), "{name} is the owner's configuration and must stay");
    }
    let names = parent.iter().chain(config.iter()).map(|s| s.to_string());
    let removed = child_env_remove(names);
    for name in parent {
        assert!(removed.iter().any(|r| r == name), "{name} missing from {removed:?}");
    }
    assert!(config.iter().all(|c| !removed.iter().any(|r| r == c)), "{removed:?}");
    assert!(removed.iter().any(|r| r == "TMUX"), "fixed names are removed even when absent from the scan");
}

#[test]
fn conversation_ids_are_restricted_to_safe_words() {
    assert!(valid_conversation_id("019a1b2c-3d4e-5f60-7182-93a4b5c6d7e8"));
    assert!(valid_conversation_id("abc_123"));
    assert!(!valid_conversation_id(""));
    assert!(!valid_conversation_id("--dangerously-skip-permissions"));
    assert!(!valid_conversation_id("a b"));
    assert!(!valid_conversation_id("a;rm"));
    assert!(!valid_conversation_id(&"a".repeat(81)));
}

#[test]
fn commands_carry_hooks_without_settings_files() {
    let dir = TempDir::new("svc-cmd");
    let mut opts = options(&dir, "true");
    opts.config.claude_command = vec!["claude".into()];
    opts.config.codex_command = vec!["codex".into(), "--yolo".into()];
    opts.config.claude_hooks = true;
    opts.config.codex_notify = true;
    opts.config.antigravity_command = vec!["agy".into(), "--dangerously-skip-permissions".into()];
    let (tx, _rx) = mpsc::channel();
    let svc = Service::new(opts, tx);

    let launch = svc.command_for(Tool::Claude, None).unwrap();
    let (argv, conv) = (launch.argv, launch.conversation.unwrap());
    assert_eq!(&argv[..3], &["claude".to_string(), "--session-id".into(), conv.clone()]);
    assert_eq!(argv[3], "--settings");
    let settings: Value = serde_json::from_str(&argv[4]).unwrap();
    assert_eq!(settings["statusLine"]["command"], "'/opt/agent deck/cepheus' statusline");
    let hook = "'/opt/agent deck/cepheus' hook claude";
    assert_eq!(settings["hooks"]["Notification"][0]["hooks"][0]["command"], hook);
    assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0]["command"], hook, "the end of each turn is reported");
    // Owner messages arrive through the plugin, on every launch.
    assert_eq!(argv[5], "--plugin-dir");
    let plugin = Path::new(&argv[6]);
    assert_eq!(plugin.parent().unwrap(), svc.opts.state_dir.join("claude-plugins"));
    assert!(plugin.join(".claude-plugin/plugin.json").is_file());
    assert_eq!(argv.len(), 7);

    let launch = svc.command_for(Tool::Claude, Some("abc")).unwrap();
    assert_eq!(&launch.argv[1..3], &["--resume".to_string(), "abc".into()]);
    assert_eq!(&launch.argv[launch.argv.len() - 2..], &argv[5..]);
    assert_eq!(launch.conversation.as_deref(), Some("abc"));

    // `--no-daemon` runs Codex embedded as the notify override needs, without
    // the warning a bare `-c` prints on every start (owner screenshot,
    // 2026-10-26: "random warnings").
    let launch = svc.command_for(Tool::Codex, Some("019a-thread")).unwrap();
    assert_eq!(
        launch.argv,
        vec![
            "codex",
            "--no-daemon",
            "-c",
            r#"notify=["/opt/agent deck/cepheus","hook","codex"]"#,
            "--yolo",
            "resume",
            "019a-thread"
        ]
    );
    assert_eq!(launch.conversation.as_deref(), Some("019a-thread"));
    assert!(!launch.attached);
    let launch = svc.command_for(Tool::Codex, None).unwrap();
    assert_eq!(launch.argv.len(), 5);
    assert_eq!(launch.conversation, None, "a new Codex thread id is learned later");

    // Codex's shared server still holds the thread: attach to it, as the
    // owner's own Codex does, instead of failing with "This conversation is
    // open in another app" (owner screenshot, 2026-10-26).
    let lock = dir.write("codex/thread-writer-locks/019a-held.lock", "");
    let held = fs::File::open(&lock).unwrap();
    held.try_lock().unwrap();
    let launch = svc.command_for(Tool::Codex, Some("019a-held")).unwrap();
    assert_eq!(launch.argv, vec!["codex", "--yolo", "resume", "019a-held"]);
    assert!(launch.attached, "no notify hook reaches an attached Codex");
    drop(held);
    assert!(!svc.command_for(Tool::Codex, Some("019a-held")).unwrap().attached, "released: embedded again");

    let launch = svc.command_for(Tool::Antigravity, None).unwrap();
    assert_eq!(launch.argv, vec!["agy", "--dangerously-skip-permissions"]);
    assert_eq!(launch.conversation, None);
    let launch = svc.command_for(Tool::Antigravity, Some("20a8a689-1111")).unwrap();
    assert_eq!(launch.argv, vec!["agy", "--dangerously-skip-permissions", "--conversation", "20a8a689-1111"]);
    assert_eq!(launch.conversation.as_deref(), Some("20a8a689-1111"));
    assert!(!dir.path().join(".claude").exists());
}

#[test]
fn grok_and_opencode_start_in_a_conversation_the_deck_chose() {
    // Owner, 2026-10-26: "make it support grok, and opencode models as well".
    // Each takes the new conversation's id, so a reopen resumes it.
    let dir = TempDir::new("svc-other-commands");
    let mut opts = options(&dir, "true");
    opts.config.grok_command = vec!["grok".into(), "--always-approve".into()];
    let (tx, _rx) = mpsc::channel();
    let svc = Service::new(opts, tx);
    let launch = svc.command_for(Tool::Grok, None).unwrap();
    let conv = launch.conversation.clone().unwrap();
    assert_eq!(launch.argv, vec!["grok".to_string(), "--always-approve".into(), "--session-id".into(), conv.clone()]);
    assert_eq!((conv.len(), conv.matches('-').count()), (36, 4), "a UUID: {conv}");
    let launch = svc.command_for(Tool::Grok, Some(&conv)).unwrap();
    assert_eq!(launch.argv, vec!["grok".to_string(), "--always-approve".into(), "--resume".into(), conv.clone()]);
    assert_eq!(launch.conversation.as_deref(), Some(conv.as_str()));
    let launch = svc.command_for(Tool::OpenCode, None).unwrap();
    let conv = launch.conversation.clone().unwrap();
    assert!(crate::other::is_opencode_id(&conv), "{conv}");
    assert_eq!(launch.argv, vec!["opencode".to_string(), "--session".into(), conv.clone()]);
    assert!(!launch.attached);
    let launch = svc.command_for(Tool::OpenCode, Some("ses_ead42ac29ffei5Xcr4QiFwE9uu")).unwrap();
    assert_eq!(launch.argv, vec!["opencode", "--session", "ses_ead42ac29ffei5Xcr4QiFwE9uu"]);
    assert_eq!(launch.conversation.as_deref(), Some("ses_ead42ac29ffei5Xcr4QiFwE9uu"));
}

#[test]
fn an_older_deck_is_never_sent_a_session_it_cannot_read() {
    // A deck open since before an update in place cannot read a Grok
    // session: one in its state would end its connection.
    let dir = TempDir::new("svc-older-deck");
    let mut opts = options(&dir, "echo ready; read x");
    opts.config.grok_command = vec!["sh".into(), "-c".into(), "echo grok ready; read x".into()];
    let socket = opts.socket.clone();
    let service = thread::spawn(move || run(opts));
    let mut c = TestClient::connect(&socket);
    c.send(ClientMsg::Create {
        tool: Tool::Grok,
        model: None,
        effort: None,
        host: None,
        cwd: None,
        command: Vec::new(),
        name: None,
        request: None,
    });
    let grok = c.wait("grok created", |m, _| match m {
        ServerMsg::Created { id, .. } => Some(*id),
        _ => None,
    });
    c.wait_state("grok listed", |s| s.sessions.iter().any(|x| x.id == grok && x.tool == Tool::Grok));
    let mut old = TestClient::connect_as(&socket, 3);
    let told = old.wait("the older deck is told", |m, _| match m {
        ServerMsg::Notice { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert!(told.contains("reopen cepheus"), "{told}");
    c.send(ClientMsg::Create {
        tool: Tool::Claude,
        model: None,
        effort: None,
        host: None,
        cwd: None,
        command: Vec::new(),
        name: None,
        request: None,
    });
    let claude = c.wait("claude created", |m, _| match m {
        ServerMsg::Created { id, .. } => Some(*id),
        _ => None,
    });
    old.wait_state("the older deck sees the Claude session", |s| s.sessions.iter().any(|x| x.id == claude));
    assert!(old.state.sessions.iter().all(|x| x.tool != Tool::Grok), "{:?}", old.state.sessions);
    c.wait_state("the current deck sees both", |s| s.sessions.len() == 2);
    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

#[test]
fn the_resume_list_holds_grok_too_but_not_for_a_deck_older_than_grok() {
    // Owner, 2026-10-31: "resume a existing session either claude/codex/agy
    // etc where you pick from a list of sessions". Alt+r listed only Claude,
    // Codex and Antigravity.
    let dir = TempDir::new("svc-resume-list");
    let mut opts = options(&dir, "echo ready; read x");
    opts.config.grok_command = vec!["sh".into(), "-c".into(), "echo grok ready; read x".into()];
    let start = opts.start_dir.to_string_lossy().into_owned();
    let claude = format!("claude/projects/{}/c0ffee00-0000-4000-8000-000000000001.jsonl", claude::project_slug(&start));
    dir.write(&claude, "{\"type\": \"user\", \"message\": {\"content\": \"Tidy the parser\"}}\n");
    let grok_id = "01a16bf6-a28a-7ce3-b29d-e722992ee7d8";
    let folder: String = start.chars().map(|c| if c == '/' { "%2F".to_string() } else { c.to_string() }).collect();
    dir.write(
        &format!("grok/sessions/{folder}/prompt_history.jsonl"),
        format!("{}\n", serde_json::json!({"timestamp": "2026-10-24T03:31:30Z", "session_id": grok_id, "prompt": "Fix the build", "is_bash": false})),
    );
    let socket = opts.socket.clone();
    let service = thread::spawn(move || run(opts));
    let list = |c: &mut TestClient| {
        c.send(ClientMsg::ListConversations { request: None, query: String::new() });
        c.wait("the conversations", |m, _| match m {
            ServerMsg::Conversations { items, .. } => {
                Some(items.iter().map(|i| (i.tool, i.title.clone())).collect::<Vec<_>>())
            }
            _ => None,
        })
    };
    let mut c = TestClient::connect(&socket);
    let mut got = list(&mut c);
    got.sort_by_key(|(tool, _)| tool.label());
    assert_eq!(got, vec![(Tool::Claude, "Tidy the parser".to_string()), (Tool::Grok, "Fix the build".to_string())]);
    // A deck from before Grok cannot read a Grok conversation; one in the
    // list would end its connection.
    let mut old = TestClient::connect_as(&socket, 3);
    assert_eq!(list(&mut old), vec![(Tool::Claude, "Tidy the parser".to_string())]);
    // Resuming one starts Grok on it, and resuming it again shows that session.
    c.send(ClientMsg::Resume { tool: Tool::Grok, conversation: grok_id.into(), host: None });
    let id = c.wait("resumed", |m, _| match m {
        ServerMsg::Created { id, .. } => Some(*id),
        _ => None,
    });
    c.wait_state("listed with its conversation", |s| {
        s.sessions.iter().any(|x| x.id == id && x.tool == Tool::Grok && x.conversation.as_deref() == Some(grok_id))
    });
    c.send(ClientMsg::Resume { tool: Tool::Grok, conversation: grok_id.into(), host: None });
    let again = c.wait("resumed again", |m, _| match m {
        ServerMsg::Created { id, .. } => Some(*id),
        _ => None,
    });
    assert_eq!(again, id);
    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

#[test]
fn hosted_codex_joins_the_shared_server_whose_thread_tools_reach_other_sessions() {
    // Owner, 2026-10-26: "before it can read TUI messages and send messaes to
    // other sessions when lauched directly in kitty" and "its a build in tool
    // call in codex that it ships with". Codex's thread tools (list_threads,
    // read_thread, send_message_to_thread) come from its shared background
    // server; a hosted Codex run embedded for the notify hook had none.
    let dir = TempDir::new("svc-codex-shared");
    let mut opts = options(&dir, "true");
    dir.write("config/profile.json", r#"{"codex_command":["codex","--yolo"]}"#);
    let (defaults, error) = Config::load_from(&dir.path().join("config/config.json"));
    assert!(error.is_none(), "{error:?}");
    opts.config.codex_command = defaults.codex_command.clone();
    opts.config.codex_notify = defaults.codex_notify;
    let (tx, _rx) = mpsc::channel();
    let svc = Service::new(opts, tx);
    assert_eq!(svc.command_for(Tool::Codex, None).unwrap().argv, vec!["codex", "--yolo"]);
    let resumed = svc.command_for(Tool::Codex, Some("019a-conversation")).unwrap();
    assert_eq!(resumed.argv, vec!["codex", "--yolo", "resume", "019a-conversation"]);
    assert!(!svc.new_tracker(Tool::Codex, false, 0).finish_hook, "turn ends come from the conversation log");
}

#[test]
fn a_claude_turn_that_leaves_background_work_running_has_not_finished() {
    // Owner, 2026-10-26: "sessions like this showed finished even though your
    // waiting for a task to complete". Claude's Stop hook lists the background
    // tasks its turn left running; Claude starts a new turn when they end, and
    // that turn's end finishes the session.
    let dir = TempDir::new("svc-background");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let mut s = fake_session(1, Tool::Claude);
    s.conversation = Some("conv-1".into());
    s.tracker.finish_hook = true;
    s.tracker.tool_busy = Some(true);
    svc.sessions.insert(1, s);
    let status = |svc: &mut Service, now_ms: u64| {
        let t = &mut svc.sessions.get_mut(&1).unwrap().tracker;
        let notify = t.update(true, false, now_ms);
        (t.status, notify)
    };
    assert_eq!(status(&mut svc, 1_000), (Status::Working, None));
    let stop = |tasks: serde_json::Value| json!({"hook_event_name": "Stop", "session_id": "conv-1", "background_tasks": tasks, "session_crons": []});
    let tests = json!({"id": "b1", "type": "shell", "status": "running", "description": "Run the tests", "command": "cargo test"});
    svc.handle_hook(1, Tool::Claude, &stop(json!([tests])));
    // Claude says it is idle while it waits.
    svc.sessions.get_mut(&1).unwrap().tracker.tool_busy = Some(false);
    assert_eq!(status(&mut svc, 60_000), (Status::Working, None), "waiting on its tests: no finished notice");
    assert_eq!(status(&mut svc, 600_000), (Status::Working, None));
    // Claude's own housekeeping in the background is not the session's work.
    let dream = json!({"id": "d1", "type": "dream", "status": "running", "description": "Consolidate memory"});
    svc.handle_hook(1, Tool::Claude, &stop(json!([dream])));
    assert_eq!(status(&mut svc, 660_000), (Status::Finished, None), "announced once its Stop hooks are over");
    let t = &mut svc.sessions.get_mut(&1).unwrap().tracker;
    t.observe_screen(&crate::status::ScreenSignals::default(), 660_250);
    t.log_busy(Some(false), 660_250);
    assert_eq!(status(&mut svc, 660_500), (Status::Finished, Some(crate::status::Notify::Finished)));
}

#[test]
fn a_claude_session_waiting_on_background_work_says_what_it_waits_on() {
    // Owner, 2026-11-01, of the phone: "it still shows thinking when your
    // waiting on a shell". The session's detail names the work its last turn
    // left running until Claude takes up a turn again.
    let dir = TempDir::new("svc-waiting-on");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let mut s = fake_session(1, Tool::Claude);
    s.conversation = Some("conv-1".into());
    s.tracker.finish_hook = true;
    svc.sessions.insert(1, s);
    let stop = |tasks: serde_json::Value| json!({"hook_event_name": "Stop", "session_id": "conv-1", "background_tasks": tasks, "session_crons": []});
    let task = |kind: &str, status: &str| json!({"id": "t", "type": kind, "status": status, "description": "work"});
    let shown = |svc: &mut Service, busy: Option<bool>, now_ms: u64| {
        let t = &mut svc.sessions.get_mut(&1).unwrap().tracker;
        t.tool_busy = busy;
        t.update(true, false, now_ms);
        let info = svc.build_state().sessions.remove(0);
        (info.status, info.detail)
    };
    let working = |detail: &str| (Status::Working, detail.to_string());

    svc.handle_hook(1, Tool::Claude, &stop(json!([task("shell", "running")])));
    assert_eq!(shown(&mut svc, Some(false), 1_000), working("waiting on a shell"));
    // Claude woke for the shell's end and works again: no longer waiting.
    assert_eq!(shown(&mut svc, Some(true), 2_000), working(""));

    let shells = json!([task("shell", "running"), task("shell", "pending"), task("dream", "running")]);
    svc.handle_hook(1, Tool::Claude, &stop(shells));
    assert_eq!(shown(&mut svc, Some(false), 3_000), working("waiting on 2 shells"));

    let mixed = json!([task("subagent", "running"), task("shell", "running"), task("shell", "completed")]);
    svc.handle_hook(1, Tool::Claude, &stop(mixed));
    assert_eq!(shown(&mut svc, Some(false), 4_000), working("waiting on 2 background tasks"));

    svc.handle_hook(1, Tool::Claude, &stop(json!([task("subagent", "running")])));
    assert_eq!(shown(&mut svc, Some(false), 5_000), working("waiting on a sub-agent"));

    // Only Claude's own housekeeping left: the turn is over and says nothing.
    svc.handle_hook(1, Tool::Claude, &stop(json!([task("dream", "running")])));
    assert_eq!(shown(&mut svc, Some(false), 6_000), (Status::Finished, String::new()));
}

#[test]
fn a_claude_rows_sub_agents_are_what_claudes_hooks_report() {
    // Owner, 2026-11-08: the row showed ⑂14 while Claude said "Waiting for 17
    // background agents to finish".
    let dir = TempDir::new("svc-claude-agents");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let mut s = fake_session(1, Tool::Claude);
    s.conversation = Some("conv-1".into());
    s.tracker.finish_hook = true;
    s.tracker.tool_busy = Some(true);
    svc.sessions.insert(1, s);
    let agent = |id: &str| json!({"id": id, "type": "subagent", "status": "running", "description": format!("task {id}"), "agent_type": "general-purpose"});
    let shown = |svc: &Service| svc.build_state().sessions.remove(0).active_subagents();
    svc.handle_hook(
        1,
        Tool::Claude,
        &json!({"hook_event_name": "SubagentStart", "session_id": "conv-1", "agent_id": "a1", "agent_type": "Explore"}),
    );
    assert_eq!(shown(&svc), 1);
    // A sub-agent's turn ending is not the session's.
    svc.handle_hook(
        1,
        Tool::Claude,
        &json!({"hook_event_name": "SubagentStop", "session_id": "conv-1", "agent_id": "a9", "agent_type": "Explore",
                "agent_transcript_path": "/p/a9.jsonl", "stop_hook_active": false,
                "background_tasks": [agent("a1"), agent("a2")], "session_crons": []}),
    );
    assert_eq!(shown(&svc), 2);
    let tracker = &svc.sessions[&1].tracker;
    assert_eq!((tracker.pending_finish, tracker.tool_busy), (false, Some(true)));
    // Another conversation's hook says nothing of this one.
    svc.handle_hook(
        1,
        Tool::Claude,
        &json!({"hook_event_name": "Stop", "session_id": "conv-2", "background_tasks": [], "session_crons": []}),
    );
    assert_eq!(shown(&svc), 2);

    // The monitor's facts never replace them, nor bring back a guess.
    let report = |subagents: Vec<crate::protocol::Subagent>| Report {
        facts: [(1, SessionFacts { known_conversation: Some("conv-1".into()), subagents, ..SessionFacts::default() })]
            .into_iter()
            .collect(),
        ..Report::default()
    };
    let guess = crate::protocol::Subagent { name: "guess".into(), active: true, ..Default::default() };
    svc.apply_report(report(vec![guess.clone(); 14]));
    assert_eq!(shown(&svc), 2);
    let mut names: Vec<String> = svc.build_state().sessions[0].subagents.iter().map(|a| a.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["task a1", "task a2"]);

    // The turn ends with one still running, then with none.
    let stop = |tasks: serde_json::Value| json!({"hook_event_name": "Stop", "session_id": "conv-1", "background_tasks": tasks, "session_crons": []});
    svc.handle_hook(1, Tool::Claude, &stop(json!([agent("a2")])));
    assert_eq!(shown(&svc), 1);
    svc.handle_hook(1, Tool::Claude, &stop(json!([])));
    svc.apply_report(report(vec![guess]));
    assert_eq!(shown(&svc), 0);
    assert_eq!(svc.build_state().sessions[0].subagents.len(), 2, "both listed as recent");
}

#[test]
fn an_active_codex_goal_keeps_working_between_turns() {
    let dir = TempDir::new("svc-active-goal");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let mut session = fake_session(1, Tool::Codex);
    session.conversation = Some("goal-thread".into());
    svc.sessions.insert(1, session);
    let report = |active: bool, busy: bool| Report {
        facts: HashMap::from([(
            1,
            SessionFacts {
                known_conversation: Some("goal-thread".into()),
                conversation: Some("goal-thread".into()),
                busy: Some(busy),
                goal_active: Some(active),
                ..SessionFacts::default()
            },
        )]),
        ..Report::default()
    };
    for hooked in [false, true] {
        svc.sessions.get_mut(&1).unwrap().tracker.finish_hook = hooked;
        svc.apply_report(report(true, true));
        svc.refresh_status(Instant::now());
        assert_eq!(svc.build_state().sessions[0].status, Status::Working);
        if hooked {
            svc.handle_hook(1, Tool::Codex, &json!({"type": "agent-turn-complete", "thread-id": "goal-thread"}));
        }
        svc.apply_report(report(true, false));
        svc.refresh_status(Instant::now());
        assert_eq!(
            svc.build_state().sessions[0].status,
            Status::Working,
            "goal continues after turn end; hook={hooked}"
        );
        let tracker = &mut svc.sessions.get_mut(&1).unwrap().tracker;
        assert_eq!(tracker.update(true, false, util::now_ms() + 60_000), None, "no false finish alert");
        tracker.hook_needs = Some("question".into());
        svc.refresh_status(Instant::now());
        assert_eq!(svc.build_state().sessions[0].status, Status::NeedsYou, "a goal can still need the owner");
        svc.sessions.get_mut(&1).unwrap().tracker.user_input();
        svc.refresh_status(Instant::now());
        assert_eq!(svc.build_state().sessions[0].status, Status::Working);
        let mut unavailable = report(false, false);
        unavailable.facts.get_mut(&1).unwrap().goal_active = None;
        svc.apply_report(unavailable);
        svc.refresh_status(Instant::now());
        assert_eq!(svc.build_state().sessions[0].status, Status::Working, "a temporary read failure cannot end a goal");
        svc.apply_report(report(false, false));
        svc.refresh_status(Instant::now());
        assert_eq!(svc.build_state().sessions[0].status, Status::Finished, "inactive goal permits a real finish");
    }
    svc.apply_report(report(true, false));
    svc.refresh_status(Instant::now());
    assert_eq!(svc.build_state().sessions[0].status, Status::Working);
    svc.sessions.get_mut(&1).unwrap().start_new_chat();
    svc.apply_report(report(true, false));
    svc.refresh_status(Instant::now());
    assert_ne!(
        svc.build_state().sessions[0].status,
        Status::Working,
        "a new chat cannot inherit the old goal, even from a stale report"
    );
}

#[test]
fn a_finished_check_stays_on_the_session_on_screen_until_the_owner_submits_or_clears() {
    // Owner, 2026-10-27, a screenshot of a session whose turn had just ended
    // reading "· idle 21s" while the owner looked at it: "when its done make
    // it a green checkmark until i enter a prompt or clear etc".
    let dir = TempDir::new("svc-finished-mark");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, r"printf '\033[?2004hready'; stty -echo; exec cat >/dev/null"), tx);
    let (out, _deck) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 1, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(1, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Tui });
    let id = svc.create(Tool::Claude, None).unwrap();
    wait_for_screen(&svc, id, "ready");
    svc.sessions.get_mut(&id).unwrap().conversation = Some("conv-a".into());
    let finish = |svc: &mut Service| {
        svc.sessions.get_mut(&id).unwrap().tracker.turn_complete();
        svc.refresh_status(Instant::now());
        assert_eq!(svc.sessions[&id].tracker.status, Status::Finished);
    };
    let status = |svc: &mut Service| {
        svc.refresh_status(Instant::now());
        svc.sessions[&id].tracker.status
    };

    finish(&mut svc);
    svc.handle_client(1, ClientMsg::Attach { id: Some(id), cols: 80, rows: 24 });
    svc.handle_client(1, ClientMsg::Focus { focused: true });
    assert_eq!(status(&mut svc), Status::Finished, "opening the session keeps the check");
    svc.handle_client(1, ClientMsg::Input { id, data: "half a prompt".into() });
    svc.handle_client(1, ClientMsg::Input { id, data: "\x1b\r".into() });
    assert_eq!(status(&mut svc), Status::Finished, "typing, and Alt+Enter's new line, submit nothing");
    svc.handle_client(1, ClientMsg::Input { id, data: "\r".into() });
    assert_eq!(status(&mut svc), Status::Idle, "Enter submits the prompt");

    // A turn that ends while the owner watches gets the check too.
    finish(&mut svc);
    svc.handle_client(1, ClientMsg::Broadcast { running_only: false, ids: vec![id], text: "next task".into() });
    assert_eq!(status(&mut svc), Status::Finished, "a message's Enter follows it");
    svc.submit_due(Instant::now() + SUBMIT_DELAY);
    assert_eq!(status(&mut svc), Status::Idle, "a message sent from the list or the phone is submitted");

    // `/clear` typed outside the deck: the session moves to a new chat.
    finish(&mut svc);
    svc.sessions.get_mut(&id).unwrap().enter_conversation(Some("conv-b".into()));
    assert_eq!(status(&mut svc), Status::Idle, "a new chat drops the old chat's check");
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
}

#[test]
fn hooks_mark_needs_and_learn_codex_threads() {
    let dir = TempDir::new("svc-hook");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    svc.sessions.insert(1, fake_session(1, Tool::Claude));
    svc.sessions.insert(2, fake_session(2, Tool::Codex));
    svc.registry.sessions.push(Entry::started(2, Tool::Codex, None, "Codex".into(), 0));

    svc.handle_hook(
        1,
        Tool::Claude,
        &json!({"notification_type": "idle_prompt", "message": "Claude is waiting for your input"}),
    );
    assert_eq!(svc.sessions[&1].tracker.hook_needs, None, "an idle reminder is not a question");
    svc.handle_hook(1, Tool::Codex, &json!({"type": "agent-turn-complete"}));
    assert!(!svc.sessions[&1].tracker.pending_finish, "a hook for the wrong tool is ignored");
    svc.handle_hook(
        1,
        Tool::Claude,
        &json!({"notification_type": "permission_prompt", "message": "Claude needs your permission to use Bash"}),
    );
    assert_eq!(svc.sessions[&1].tracker.hook_needs.as_deref(), Some("Claude needs your permission to use Bash"));

    // Claude's Stop hook ends the turn of the session's own conversation only.
    svc.sessions.get_mut(&1).unwrap().conversation = Some("conv-1".into());
    svc.handle_hook(1, Tool::Claude, &json!({"hook_event_name": "Stop", "session_id": "conv-other"}));
    assert!(!svc.sessions[&1].tracker.pending_finish, "another conversation's turn");
    svc.handle_hook(1, Tool::Claude, &json!({"hook_event_name": "Stop", "session_id": "conv-1"}));
    assert!(svc.sessions[&1].tracker.pending_finish);
    svc.sessions.get_mut(&1).unwrap().tracker.pending_finish = false;
    assert_eq!(svc.sessions[&1].tracker.hook_needs.as_deref(), Some("Claude needs your permission to use Bash"));

    let meta = |id: &str, source: &str| {
        json!({"type": "session_meta", "payload": {"id": id, "cwd": "/srv", "thread_source": source}}).to_string()
            + "\n"
    };
    dir.write("codex/sessions/2026/10/25/rollout-x-019a-thread.jsonl", meta("019a-thread", "user"));
    dir.write("codex/sessions/2026/10/25/rollout-x-019a-helper.jsonl", meta("019a-helper", "subagent"));
    let turn_of = |thread: &str| json!({"type": "agent-turn-complete", "thread-id": thread, "turn-id": "t"});

    // Codex's temporary naming thread (no rollout) and its sub-agents finish
    // turns mid-way through the session's own turn.
    for other in ["019a-namer", "019a-helper", "--evil"] {
        svc.handle_hook(2, Tool::Codex, &turn_of(other));
        assert_eq!(svc.sessions[&2].conversation, None, "{other} is not the session's thread");
        assert!(!svc.sessions[&2].tracker.pending_finish, "{other} finishing does not finish the session");
    }
    svc.handle_hook(2, Tool::Codex, &turn_of("019a-thread"));
    assert_eq!(svc.sessions[&2].conversation.as_deref(), Some("019a-thread"));
    assert_eq!(svc.registry.sessions[0].conversation.as_deref(), Some("019a-thread"));
    assert!(svc.sessions[&2].tracker.pending_finish);
    svc.sessions.get_mut(&2).unwrap().tracker.pending_finish = false;
    svc.handle_hook(2, Tool::Codex, &turn_of("019a-namer"));
    assert_eq!(svc.sessions[&2].conversation.as_deref(), Some("019a-thread"));
    assert!(!svc.sessions[&2].tracker.pending_finish);
    svc.handle_hook(2, Tool::Codex, &json!({"type": "agent-turn-complete"}));
    assert!(svc.sessions[&2].tracker.pending_finish, "an older Codex sends no thread id");
    assert_eq!(
        svc.hosted_refs.lock().unwrap().iter().find(|r| r.id == 2).and_then(|r| r.conversation.clone()).as_deref(),
        Some("019a-thread")
    );
}

#[test]
fn stale_monitor_reports_never_undo_a_newer_conversation() {
    let dir = TempDir::new("svc-stale-report");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let mut s = fake_session(2, Tool::Codex);
    s.conversation = Some("019a-old".into());
    svc.sessions.insert(2, s);
    svc.registry.sessions.push(Entry::started(2, Tool::Codex, Some("019a-old".into()), "Codex".into(), 0));
    let meta = json!({"type": "session_meta", "payload": {"id": "019a-new", "cwd": "/srv", "thread_source": "user"}});
    dir.write("codex/sessions/2026/10/25/rollout-x-019a-new.jsonl", meta.to_string() + "\n");
    let report_on = |known: &str, title: &str| {
        let mut report = Report::default();
        let facts = SessionFacts {
            known_conversation: Some(known.into()),
            conversation: Some(known.into()),
            meta_title: Some(title.into()),
            ..SessionFacts::default()
        };
        report.facts.insert(2, facts);
        report
    };
    // The monitor snapshots the session in 019a-old; before its report
    // arrives, the session moves to a new thread (`/new`) and finishes a turn.
    let stale = report_on("019a-old", "Old work");
    svc.handle_hook(2, Tool::Codex, &json!({"type": "agent-turn-complete", "thread-id": "019a-new"}));
    svc.apply_report(stale);
    assert_eq!(svc.sessions[&2].conversation.as_deref(), Some("019a-new"));
    assert_eq!(svc.registry.sessions[0].conversation.as_deref(), Some("019a-new"));
    assert_ne!(svc.sessions[&2].display_title(&[], &mut Titles::off()), "Old work");
    assert_eq!(
        svc.hosted_refs.lock().unwrap().iter().find(|r| r.id == 2).and_then(|r| r.conversation.clone()).as_deref(),
        Some("019a-new")
    );
    // A report from a snapshot taken after the change applies.
    svc.apply_report(report_on("019a-new", "New work"));
    assert_eq!(svc.sessions[&2].display_title(&[], &mut Titles::off()), "New work");
}

#[test]
fn a_new_codex_chat_drops_the_title_and_worktree_of_the_one_before() {
    // Owner, 2026-10-26, a screenshot of a Codex session cleared to its
    // welcome card still listed as "Resume session" in a sample worktree:
    // "/ clear doesnt reset the title and worktree given".
    let dir = TempDir::new("svc-codex-clear");
    // Codex 0.164's welcome card, a chat, then `/status` filling the screen
    // with its own card, then `/clear`.
    let card = r"╭──────╮\n│ >_ OpenAI Codex (v0.164.0) │\n│ │\n│ model:     gpt-5.5 │\n│ directory: ~/projects/example/app │\n╰──────╯\n";
    let status = r"\033[2J\033[H│ >_ OpenAI Codex (v0.164.0) │\n│ │\n│ Model:      gpt-5.5 │\n│ Directory:  ~/projects/example/app │\n\n› ";
    let script = format!(
        "printf '{card}\\n› Resume session\\n\\n• Resumed in the sample worktree.\\n\\n› '; read x; \
         printf '{status}'; read y; \
         printf '\\033[2J\\033[3J\\033[H{card}  To get started, describe a task\\n\\n› '; read z"
    );
    let script = script.as_str();
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, script), tx);
    let id = svc.create(Tool::Codex, None).unwrap();
    wait_for_screen(&svc, id, "Resumed in");
    let worktree =
        crate::protocol::Worktree { path: "/srv/w".into(), branch: Some("sample-s4".into()), task: None, exists: true };
    {
        let s = svc.sessions.get_mut(&id).unwrap();
        s.conversation = Some("019a-old".into());
        s.facts = SessionFacts {
            known_conversation: Some("019a-old".into()),
            conversation: Some("019a-old".into()),
            first_prompt: Some("Resume session".into()),
            worktree: Some(worktree),
            ..SessionFacts::default()
        };
    }
    svc.registry.sessions.iter_mut().find(|e| e.id == id).unwrap().conversation = Some("019a-old".into());
    svc.update_refs();
    let start = Instant::now() + Duration::from_secs(1);
    svc.refresh_status(start);
    assert_eq!(svc.sessions[&id].title, "Resume session");
    svc.handle_client(0, ClientMsg::Input { id, data: "/status\r".into() });
    wait_for_screen(&svc, id, "Directory:");
    svc.refresh_status(start + Duration::from_secs(1));
    svc.refresh_status(start + Duration::from_secs(3));
    assert_eq!(svc.sessions[&id].title, "Resume session", "the /status card is not a new chat");
    assert_eq!(svc.sessions[&id].conversation.as_deref(), Some("019a-old"));
    // `/clear`: Codex clears the terminal and shows its card again.
    svc.handle_client(0, ClientMsg::Input { id, data: "/clear\r".into() });
    wait_for_screen(&svc, id, "To get started");
    svc.refresh_status(start + Duration::from_secs(4));
    svc.refresh_status(start + Duration::from_secs(6));
    let s = &svc.sessions[&id];
    assert_eq!(s.conversation, None, "the new chat is not the old conversation");
    assert_eq!(s.title, "Codex");
    assert_eq!(s.facts.worktree, None);
    let entry = svc.registry.sessions.iter().find(|e| e.id == id).unwrap();
    assert_eq!(entry.conversation, None, "reopening it must not resume the old conversation");
    assert_eq!(entry.title, "Codex");
    assert_eq!(svc.hosted_refs.lock().unwrap().iter().find(|r| r.id == id).unwrap().conversation, None);
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
}

#[test]
fn clear_typed_in_a_resumed_codex_starts_a_new_chat() {
    // Owner screenshot, 2026-10-26: a Codex the deck had reopened was cleared
    // and given "merge everything to master", and stayed listed as "Finish
    // Sample session1 plan.md". A resumed Codex 0.164 shows only its start
    // screen, never the chat, so the screen never showed one before `/clear`.
    let dir = TempDir::new("svc-codex-resumed-clear");
    let splash = r"  >_ OpenAI Codex (v0.164.0)\n     ~/projects/example/app\n  permissions: YOLO mode\n\n";
    // The start screen; `/clear` typed and submitted; the start screen again.
    // Ctrl+L redraws it too, which is not a new chat.
    let script = format!(
        "printf '{splash}› '; read x; printf '\\033[2J\\033[3J\\033[H{splash}› '; read y; \
         printf '\\033[2J\\033[3J\\033[H{splash}› '; read z"
    );
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, &script), tx);
    let id = svc.create(Tool::Codex, None).unwrap();
    wait_for_screen(&svc, id, "permissions:");
    {
        let s = svc.sessions.get_mut(&id).unwrap();
        s.conversation = Some("019a-old".into());
        s.facts = SessionFacts {
            known_conversation: Some("019a-old".into()),
            conversation: Some("019a-old".into()),
            meta_title: Some("Finish Sample session1 plan.md".into()),
            ..SessionFacts::default()
        };
    }
    svc.registry.sessions.iter_mut().find(|e| e.id == id).unwrap().conversation = Some("019a-old".into());
    svc.update_refs();
    let start = Instant::now() + Duration::from_secs(1);
    svc.refresh_status(start);
    assert_eq!(svc.sessions[&id].title, "Finish Sample session1 plan.md");
    // Typed as the owner does, one key at a time, then Enter.
    for key in "/clear".chars() {
        svc.handle_client(0, ClientMsg::Input { id, data: key.to_string() });
    }
    wait_for_screen(&svc, id, "› /clear");
    svc.handle_client(0, ClientMsg::Input { id, data: "\r".into() });
    let cleared_at = util::now_ms();
    wait_until(|| {
        let guard = svc.sessions[&id].hosted.as_ref().unwrap().lock();
        let lines = screen::screen_text(&guard.term);
        !lines.iter().any(|line| line.contains("/clear"))
            && status::codex_new_chat(&lines, screen::text_cursor(&guard.term)) == Some(true)
    });
    svc.refresh_status(start + Duration::from_secs(1));
    let s = &svc.sessions[&id];
    assert_eq!(s.conversation, None, "the new chat is not the old conversation");
    assert_eq!(s.title, "Codex");
    assert!(s.spawned_ms + 1000 >= cleared_at, "its thread is looked for from the /clear");
    assert!(!s.owner_draft, "an explicit confirmed /clear releases the unknown draft latch");
    assert_eq!(svc.registry.sessions.iter().find(|e| e.id == id).unwrap().conversation, None);
    // Ctrl+L: the start screen again with no command submitted.
    svc.sessions.get_mut(&id).unwrap().conversation = Some("019a-new".into());
    svc.handle_client(0, ClientMsg::Input { id, data: "\x0c".into() });
    svc.handle_client(0, ClientMsg::Input { id, data: "x\r".into() });
    svc.refresh_status(start + Duration::from_secs(2));
    svc.refresh_status(start + Duration::from_secs(9));
    assert_eq!(svc.sessions[&id].conversation.as_deref(), Some("019a-new"), "no command, no new chat");
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
}

#[test]
fn a_refused_clear_leaves_the_chat() {
    // Codex refuses `/clear` while a turn runs; its screen keeps the chat.
    let dir = TempDir::new("svc-codex-refused-clear");
    let script = "printf '› fix it\\n\\n• Working (3s • esc to interrupt)\\n\\n› '; read x; \
                  printf '\\n• /clear is disabled while a task is in progress.\\n\\n› '; read y";
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, script), tx);
    let id = svc.create(Tool::Codex, None).unwrap();
    wait_for_screen(&svc, id, "Working");
    svc.sessions.get_mut(&id).unwrap().conversation = Some("019a-old".into());
    let start = Instant::now() + Duration::from_secs(1);
    svc.refresh_status(start);
    svc.handle_client(0, ClientMsg::Input { id, data: "/clear\r".into() });
    wait_for_screen(&svc, id, "disabled");
    for secs in [1, 3, 8] {
        svc.refresh_status(start + Duration::from_secs(secs));
    }
    assert_eq!(svc.sessions[&id].conversation.as_deref(), Some("019a-old"));
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
}

#[test]
fn a_session_in_another_conversation_drops_the_old_title() {
    // Claude Code's `/clear` starts a conversation of its own, which the
    // monitor follows; the old conversation's title must not stay.
    let dir = TempDir::new("svc-claude-clear");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let mut s = fake_session(4, Tool::Claude);
    s.conversation = Some("conv-old".into());
    s.title = "Old work".into();
    svc.sessions.insert(4, s);
    let mut report = Report::default();
    report.facts.insert(
        4,
        SessionFacts {
            known_conversation: Some("conv-old".into()),
            conversation: Some("conv-new".into()),
            ..SessionFacts::default()
        },
    );
    svc.apply_report(report);
    svc.refresh_status(Instant::now());
    assert_eq!(svc.sessions[&4].conversation.as_deref(), Some("conv-new"));
    assert_eq!(svc.sessions[&4].title, "Claude");
}

#[test]
fn claude_clear_drops_the_previous_conversations_turn_state() {
    for hooks in [false, true] {
        for activity in ["background", "screen", "question", "finish"] {
            let dir = TempDir::new("svc-claude-clear-status");
            let (tx, _rx) = mpsc::channel();
            let mut svc = Service::new(options(&dir, "true"), tx);
            let mut s = fake_session(4, Tool::Claude);
            s.conversation = Some("conv-old".into());
            s.tracker.finish_hook = hooks;
            svc.sessions.insert(4, s);
            match activity {
                "background" => svc.handle_hook(
                    4,
                    Tool::Claude,
                    &json!({"hook_event_name": "Stop", "session_id": "conv-old", "background_tasks": [
                        {"type": "shell", "status": "running"}
                    ]}),
                ),
                "screen" => svc.sessions.get_mut(&4).unwrap().tracker.observe_screen(
                    &status::screen_signals(&["Thinking (esc to interrupt)".into()]),
                    util::now_ms(),
                ),
                "question" => svc.handle_hook(
                    4,
                    Tool::Claude,
                    &json!({"hook_event_name": "Notification", "notification_type": "permission_prompt", "message": "Allow?"}),
                ),
                "finish" => svc.handle_hook(
                    4,
                    Tool::Claude,
                    &json!({"hook_event_name": "Stop", "session_id": "conv-old"}),
                ),
                _ => unreachable!(),
            }
            let report = |conversation: &str, busy: bool| Report {
                facts: HashMap::from([(
                    4,
                    SessionFacts {
                        known_conversation: Some("conv-old".into()),
                        conversation: Some(conversation.into()),
                        busy: Some(busy),
                        ..SessionFacts::default()
                    },
                )]),
                ..Report::default()
            };
            if activity != "finish" {
                svc.apply_report(report("conv-old", false));
                svc.refresh_status(Instant::now());
                let expected = if activity == "question" { Status::NeedsYou } else { Status::Working };
                assert_eq!(svc.build_state().sessions[0].status, expected, "{activity}: same conversation");
            }

            // Claude updates its session id after /clear, without a turn-end hook.
            svc.apply_report(report("conv-new", false));
            svc.refresh_status(Instant::now());
            assert_eq!(svc.build_state().sessions[0].status, Status::Idle, "{activity}, hooks={hooks}");
            assert!(!svc.sessions[&4].tracker.unseen, "clearing does not finish an old turn");
            assert_eq!(svc.sessions[&4].tracker.finish_hook, hooks);

            // An old Stop arriving late cannot restore the old background work.
            svc.handle_hook(
                4,
                Tool::Claude,
                &json!({"hook_event_name": "Stop", "session_id": "conv-old", "background_tasks": [
                    {"type": "shell", "status": "running"}
                ]}),
            );
            svc.refresh_status(Instant::now());
            assert_eq!(svc.build_state().sessions[0].status, Status::Idle);

            // Real work in the new chat is still observed immediately.
            let mut next = report("conv-new", true);
            next.facts.get_mut(&4).unwrap().known_conversation = Some("conv-new".into());
            svc.apply_report(next);
            svc.refresh_status(Instant::now());
            assert_eq!(svc.build_state().sessions[0].status, Status::Working);
        }
    }
}

#[test]
fn monitor_reports_update_sessions() {
    let dir = TempDir::new("svc-report");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    svc.sessions.insert(3, fake_session(3, Tool::Claude));
    svc.registry.sessions.push(Entry::started(3, Tool::Claude, None, "Claude".into(), 0));
    let mut report = Report::default();
    report.facts.insert(
        3,
        SessionFacts {
            conversation: Some("conv-3".into()),
            meta_title: Some("Deck work".into()),
            busy: Some(true),
            ..SessionFacts::default()
        },
    );
    report.facts.insert(99, SessionFacts::default());
    report.usage.claude = vec![crate::protocol::UsageWindow {
        label: "5h".into(),
        used_percent: Some(5.0),
        resets_at: None,
        updated_at: 0,
    }];
    svc.apply_report(report);
    let s = &svc.sessions[&3];
    assert_eq!(s.conversation.as_deref(), Some("conv-3"));
    assert_eq!(s.tracker.tool_busy, Some(true));
    assert_eq!(s.display_title(&[], &mut Titles::off()), "Deck work");
    assert_eq!(svc.registry.sessions[0].conversation.as_deref(), Some("conv-3"));
    let state = svc.build_state();
    assert_eq!(state.usage.claude.len(), 1);
    assert_eq!(state.sessions[0].conversation.as_deref(), Some("conv-3"));
}

#[test]
fn registry_sessions_load_as_ended_and_resumable() {
    let dir = TempDir::new("svc-load");
    let reg =
        Registry { next_id: 5, sessions: vec![Entry::started(4, Tool::Codex, Some("t".into()), "Old work".into(), 9)] };
    reg.save(&dir.path().join("state/sessions.json")).unwrap();
    let (tx, _rx) = mpsc::channel();
    let svc = Service::new(options(&dir, "true"), tx);
    let state = svc.build_state();
    assert_eq!(state.sessions.len(), 1);
    let s = &state.sessions[0];
    assert_eq!(
        (s.id, s.alive, s.status, s.title.as_str(), s.detail.as_str()),
        (4, false, Status::Ended, "Old work", "not running")
    );
    assert!(svc.registry.sessions[0].ended_at.is_some(), "a session left running by a stopped service is marked ended");
}

/// A control connection whose messages arrive through a reader thread.
pub(crate) struct TestClient {
    writer: UnixStream,
    rx: Receiver<ServerMsg>,
    pub(crate) state: DeckState,
}

impl TestClient {
    pub(crate) fn connect(socket: &Path) -> TestClient {
        Self::connect_as(socket, PROTOCOL_VERSION)
    }

    /// A client speaking protocol `version`, as a deck built then would.
    fn connect_as(socket: &Path, version: u32) -> TestClient {
        Self::connect_with(socket, version, ClientRole::Control)
    }

    fn connect_with(socket: &Path, version: u32, role: ClientRole) -> TestClient {
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(s) => break s,
                Err(e) => {
                    assert!(Instant::now() < deadline, "service never listened: {e}");
                    thread::sleep(Duration::from_millis(20));
                }
            }
        };
        let (tx, rx) = mpsc::channel();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        thread::spawn(move || {
            while let Ok(Some(msg)) = read_msg::<_, ServerMsg>(&mut reader) {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        let mut client = TestClient { writer: stream, rx, state: DeckState::default() };
        client.send(ClientMsg::Hello { version, role });
        client.wait("welcome", |m, _| matches!(m, ServerMsg::Welcome { .. }).then_some(()));
        client
    }

    pub(crate) fn send(&mut self, msg: ClientMsg) {
        write_msg(&mut self.writer, &msg).unwrap();
    }

    pub(crate) fn wait<T>(&mut self, what: &str, mut want: impl FnMut(&ServerMsg, &DeckState) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = match self.rx.recv_timeout(left) {
                Ok(msg) => msg,
                Err(_) => panic!("timed out waiting for {what}; last state {:?}", self.state),
            };
            // A protocol-20 client gets the whole state once, then patches of it.
            match &msg {
                ServerMsg::State(state) => self.state = state.clone(),
                ServerMsg::StatePatch(patch) => self.state.apply(patch.clone()),
                _ => {}
            }
            if let Some(v) = want(&msg, &self.state) {
                return v;
            }
        }
    }

    pub(crate) fn wait_state(&mut self, what: &str, want: impl Fn(&DeckState) -> bool) {
        if want(&self.state) {
            return;
        }
        self.wait(what, |m, state| {
            (matches!(m, ServerMsg::State(_) | ServerMsg::StatePatch(_)) && want(state)).then_some(())
        });
    }

    pub(crate) fn screen(&mut self, id: u64) -> Vec<String> {
        self.send(ClientMsg::Screen { id, tail: None });
        self.wait("screen", |m, _| match m {
            ServerMsg::ScreenText { id: got, lines, .. } if *got == id => Some(lines.clone()),
            _ => None,
        })
    }

    pub(crate) fn wait_screen(&mut self, id: u64, needle: &str) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let lines = self.screen(id);
            if lines.iter().any(|l| l.contains(needle)) {
                return lines;
            }
            assert!(Instant::now() < deadline, "screen of {id} never showed {needle:?}: {lines:?}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn session(&self, id: u64) -> Option<&SessionInfo> {
        self.state.sessions.iter().find(|s| s.id == id)
    }
}

#[test]
fn hosts_sessions_end_to_end() {
    let dir = TempDir::new("svc");
    let cwd = fs::canonicalize(dir.path()).unwrap();
    let script = format!(
        r#"echo "ready $CEPHEUS_SESSION $TERM args $0 $1"; [ "$(pwd -P)" = {} ] && echo cwd-ok; [ -z "$CLAUDECODE" ] && echo env-ok; read x; echo "got $x"; read y"#,
        util::shell_quote(&cwd.to_string_lossy())
    );
    let opts = options(&dir, &script);
    let socket = opts.socket.clone();
    let registry_path = opts.state_dir.join("sessions.json");
    let service = thread::spawn(move || run(opts));
    let mut c = TestClient::connect(&socket);
    assert_eq!(fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);

    // A second service cannot start beside the first.
    let err = run(options(&dir, "true")).unwrap_err();
    assert!(format!("{err:#}").contains("another cepheus service"), "{err:#}");

    // Create a Claude session: it starts in the start directory with a fresh conversation id.
    c.send(ClientMsg::Create {
        tool: Tool::Claude,
        model: None,
        effort: None,
        host: None,
        cwd: None,
        command: Vec::new(),
        name: None,
        request: None,
    });
    let id = c.wait("created", |m, _| match m {
        ServerMsg::Created { id, .. } => Some(*id),
        _ => None,
    });
    assert_eq!(id, 1);
    let lines = c.wait_screen(id, "env-ok");
    let joined = lines.join("\n");
    assert!(joined.contains("ready 1 xterm-256color args --session-id "), "{joined}");
    assert!(joined.contains("cwd-ok"), "{joined}");
    c.wait_state("session listed", |s| s.sessions.iter().any(|x| x.id == id && x.alive));
    let conversation = c.session(id).unwrap().conversation.clone().unwrap();
    assert_eq!(conversation.len(), 36);

    // Typed input reaches the program, which begins its conversation.
    c.send(ClientMsg::Input { id, data: "hello deck\r".into() });
    c.wait_screen(id, "got hello deck");
    claude_transcript(&dir, &conversation);

    // Attaching streams frames sized to the client's pane.
    c.send(ClientMsg::Attach { id: Some(id), cols: 60, rows: 12 });
    let frame = c.wait("frame", |m, _| match m {
        ServerMsg::Frame { id: got, frame } if *got == id && frame.cols == 60 => Some(frame.clone()),
        _ => None,
    });
    assert_eq!((frame.cols, frame.rows), (60, 12));
    assert!(frame.lines.iter().flatten().any(|r| r.t.contains("got")), "{frame:?}");
    // Detach and widen again so later screens do not wrap.
    c.send(ClientMsg::Attach { id: None, cols: 120, rows: 30 });

    // Forgetting a live session is refused.
    c.send(ClientMsg::Forget { id });
    let text = c.wait("notice", |m, _| match m {
        ServerMsg::Notice { text, error: true } => Some(text.clone()),
        _ => None,
    });
    assert!(text.contains("close the session"), "{text}");

    // A program that exits by itself once its conversation began leaves its
    // session listed as ended; the registry records the end.
    c.send(ClientMsg::Input { id, data: "bye\r".into() });
    c.wait_state("session ended", |s| s.sessions.iter().any(|x| x.id == id && !x.alive && x.status == Status::Ended));
    let reg = Registry::load(&registry_path);
    assert_eq!(reg.sessions.len(), 1);
    assert_eq!(reg.sessions[0].conversation.as_deref(), Some(conversation.as_str()));
    assert!(reg.sessions[0].ended_at.is_some());

    // Reopening resumes the same conversation in the same slot.
    c.send(ClientMsg::Reopen { id });
    c.wait("reopened", |m, _| matches!(m, ServerMsg::Created { id: got, .. } if *got == id).then_some(()));
    let lines = c.wait_screen(id, &format!("args --resume {conversation}"));
    assert!(lines.join("\n").contains("ready 1"), "{lines:?}");

    // Resuming a conversation that is already open selects it instead of starting another.
    c.send(ClientMsg::Resume { tool: Tool::Claude, conversation: conversation.clone(), host: None });
    c.wait("same session", |m, _| matches!(m, ServerMsg::Created { id: got, .. } if *got == id).then_some(()));

    // Resuming an unknown Codex thread starts a new session with `resume <id>`.
    c.send(ClientMsg::Resume { tool: Tool::Codex, conversation: "019a-thread".into(), host: None });
    let codex_id = c.wait("codex created", |m, _| match m {
        ServerMsg::Created { id, .. } => Some(*id),
        _ => None,
    });
    assert_eq!(codex_id, 2);
    c.wait_screen(codex_id, "args resume 019a-thread");

    // Unsafe ids never reach a command line.
    c.send(ClientMsg::Resume { tool: Tool::Claude, conversation: "--dangerously-skip-permissions".into(), host: None });
    let text = c.wait("rejected", |m, _| match m {
        ServerMsg::Notice { text, error: true } => Some(text.clone()),
        _ => None,
    });
    assert!(text.contains("invalid conversation id"), "{text}");

    // Closing ends the program and takes the session off the deck and out of
    // the registry. Owner, 2026-10-26: "close closes it, alt x and i should
    // not see that session anymore".
    c.wait_state("codex listed", |s| s.sessions.iter().any(|x| x.id == codex_id && x.alive));
    c.send(ClientMsg::Close { id: codex_id });
    c.wait_state("codex removed", |s| s.sessions.iter().all(|x| x.id != codex_id));
    assert!(Registry::load(&registry_path).sessions.iter().all(|e| e.id != codex_id));

    // Explicitly close the remaining fixture session, then stop its empty service.
    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
    assert!(!socket.exists());
    let reg = Registry::load(&registry_path);
    assert!(reg.sessions.is_empty(), "explicit Close removes the remaining fixture session");
    assert_eq!(reg.next_id, 3);
}

#[test]
fn broadcast_types_and_submits_one_message_in_each_ready_session() {
    let dir = TempDir::new("svc-broadcast");
    // Bracketed paste on (as Claude Code and Codex do once their input box is
    // up) unless the session was told to stay "starting".
    let script = r#"[ "$CEPHEUS_SESSION" = 4 ] || printf '\033[?2004h'; echo ready; read x; echo "got $x"; read y"#;
    let opts = options(&dir, script);
    let socket = opts.socket.clone();
    let service = thread::spawn(move || run(opts));
    let mut c = TestClient::connect(&socket);
    let mut ids = Vec::new();
    for tool in [Tool::Claude, Tool::Codex, Tool::Claude, Tool::Codex] {
        c.send(ClientMsg::Create {
            tool,
            model: None,
            effort: None,
            host: None,
            cwd: None,
            command: Vec::new(),
            name: None,
            request: None,
        });
        let id = c.wait("created", |m, _| match m {
            ServerMsg::Created { id, .. } => Some(*id),
            _ => None,
        });
        c.wait_screen(id, "ready");
        ids.push(id);
    }
    assert_eq!(ids, vec![1, 2, 3, 4]);
    // Session 3 is showing a permission question: Enter there would answer it.
    let payload =
        json!({"notification_type": "permission_prompt", "message": "Claude needs your permission to use Bash"});
    c.send(ClientMsg::Hook { session: 3, tool: Tool::Claude, payload });
    c.wait_state("question shown", |s| s.sessions.iter().any(|x| x.id == 3 && x.status == Status::NeedsYou));

    c.send(ClientMsg::Broadcast {
        running_only: false,
        ids: vec![1, 2, 3, 4, 99],
        text: "hello all\x1b[201~\r\x03".into(),
    });
    let notice = c.wait("broadcast notice", |m, _| match m {
        ServerMsg::Notice { text, error: false } => Some(text.clone()),
        _ => None,
    });
    // Named as the list names them: these have no title yet.
    assert_eq!(
        notice,
        "Sent to 2 sessions (Claude, Codex); skipped Claude (waiting on a question), Codex (still starting)"
    );
    for id in [1, 2] {
        let lines = c.wait_screen(id, "got hello all");
        let joined = lines.join("\n");
        // The tty echo shows the one paste, its markers intact and no injected keys.
        assert!(joined.contains("^[[200~hello all[201~^[[201~"), "{joined}");
        assert!(!joined.contains("^C"), "{joined}");
    }
    for id in [3, 4] {
        let joined = c.screen(id).join("\n");
        assert!(!joined.contains("hello all"), "session {id} was skipped: {joined}");
    }
    c.send(ClientMsg::Broadcast { running_only: false, ids: vec![1], text: "  ".into() });
    let text = c.wait("empty refused", |m, _| match m {
        ServerMsg::Notice { text, error: true } => Some(text.clone()),
        _ => None,
    });
    assert!(text.contains("Type a message"), "{text}");
    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

// A PTY stand-in for Claude's asynchronous image-paste handler. `!` is a
// test barrier: every earlier input byte has been consumed when it answers.
// `~` completes image processing. An Enter during Pasting is lost, as in
// Claude Code 2.1.316; Enter after it finishes submits the original paste.
fn image_paste_service(dir: &TempDir) -> (Service, u64) {
    let program = r#"
import os, sys, tty
from pathlib import Path
tty.setraw(0)
def show(s):
    os.write(1, s.encode())
show('\x1b[?2004hready')
pasted = b''
while not pasted.endswith(b'\x1b[201~'):
    pasted += os.read(0, 1)
Path('pasted').write_bytes(pasted)
show('\x1b[36;1HPasting\u2026')
early = 0
while True:
    key = os.read(0, 1)
    if key == b'\r':
        early += 1
    elif key == b'!':
        Path('early').write_text(str(early))
        show('\x1b[1;1Hchecked')
    elif key == b'~':
        break
show('\x1b[36;1H\x1b[2Kpaste ready')
while True:
    key = os.read(0, 1)
    if key == b'\r':
        Path('submitted').write_bytes(pasted)
        with Path('enters').open('ab') as entered:
            entered.write(key)
        show('\x1b[1;1Hsubmitted')
    elif key == b'!':
        show('\x1b[2;1Hchecked again')
"#;
    let mut opts = options(dir, "true");
    opts.config.claude_command = vec!["python3".into(), "-u".into(), "-c".into(), program.into()];
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let id = svc.create(Tool::Claude, None).unwrap();
    wait_for_screen(&svc, id, "ready");
    (svc, id)
}

#[test]
fn image_pastes_submit_once_after_processing_without_a_second_message() {
    for message in [
        "Please inspect this\n\n/synthetic/shot.jpg",
        "Compare these\n\n/synthetic/first.png\n/synthetic/second.png",
        "/synthetic/screenshot.webp",
    ] {
        let dir = TempDir::new("svc-image-paste");
        let (mut svc, id) = image_paste_service(&dir);
        svc.broadcast_message(0, &[id], message);
        wait_for_screen(&svc, id, "Pasting…");
        let due = Instant::now() + SUBMIT_DELAY;
        svc.submit_due(due);
        svc.sessions[&id].hosted.as_ref().unwrap().write(b"!".to_vec());
        wait_for_screen(&svc, id, "checked");
        let early = std::fs::read_to_string(dir.path().join("early")).unwrap();
        // Release the asynchronous paste, without sending another prompt.
        svc.sessions[&id].hosted.as_ref().unwrap().write(b"~".to_vec());
        wait_for_screen(&svc, id, "paste ready");
        svc.submit_due(due + Duration::from_secs(1));
        // Always stop our process, including for the unfixed red case.
        if early != "0" {
            svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
            assert_eq!(early, "0", "Enter was swallowed while processing {message:?}");
        }
        wait_for_screen(&svc, id, "submitted");
        svc.submit_due(due + Duration::from_secs(2));
        svc.sessions[&id].hosted.as_ref().unwrap().write(b"!".to_vec());
        wait_for_screen(&svc, id, "checked again");
        let submitted = std::fs::read(dir.path().join("submitted")).unwrap();
        let enters = std::fs::read(dir.path().join("enters")).unwrap();
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
        assert_eq!(submitted, crate::keys::encode_message(message).into_bytes());
        assert_eq!(enters, b"\r", "the same message is submitted exactly once");
    }
}

#[test]
fn image_pastes_cancel_enter_after_timeout_or_intervening_input() {
    for changed_input in [false, true] {
        let dir = TempDir::new("svc-image-paste-cancel");
        let (mut svc, id) = image_paste_service(&dir);
        svc.broadcast_message(0, &[id], "Inspect\n\n/synthetic/shot.png");
        wait_for_screen(&svc, id, "Pasting…");
        let mut due = Instant::now() + SUBMIT_DELAY;
        if changed_input {
            svc.handle_client(0, ClientMsg::Input { id, data: "edited".into() });
        } else {
            due += Duration::from_secs(60);
        }
        svc.submit_due(due);
        svc.sessions[&id].hosted.as_ref().unwrap().write(b"!".to_vec());
        wait_for_screen(&svc, id, "checked");
        let early = std::fs::read_to_string(dir.path().join("early")).unwrap();
        svc.sessions[&id].hosted.as_ref().unwrap().write(b"~".to_vec());
        wait_for_screen(&svc, id, "paste ready");
        svc.submit_due(due + Duration::from_secs(1));
        svc.sessions[&id].hosted.as_ref().unwrap().write(b"!".to_vec());
        wait_for_screen(&svc, id, "checked again");
        let submitted = dir.path().join("submitted").exists();
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
        assert_eq!(early, "0", "no early Enter, changed_input={changed_input}");
        assert!(!submitted, "no delayed Enter after cancellation, changed_input={changed_input}");
    }
}

// A raw PTY program that keeps every byte it reads in `keys`, in order. `!`
// is a test barrier: once it answers `checked N`, every earlier byte is kept.
// It shows a Claude Code permission question when it reads `?`, and with
// `ask` before anything else.
pub(super) fn key_recorder_service(dir: &TempDir, tool: Tool, ask: bool) -> (Service, u64) {
    let question = r"\x1b[30;1H Bash command\r\n\r\n   rm -rf build\r\n\r\n Do you want to proceed?\r\n ❯ 1. Yes\r\n   2. No\r\n\r\n Esc to cancel";
    let first = if ask {
        question
    } else if tool == Tool::Codex {
        r"\r\n\r\n› \x1b[2mAsk Codex to do anything\x1b[0m\r\n\r\n  GPT-6.1-Sol xhigh · Test conversation\r\n  ? for shortcuts\x1b[3;3H"
    } else {
        ""
    };
    let program = format!(
        r#"
import os, tty
from pathlib import Path
tty.setraw(0)
os.write(1, '\x1b[?2004hready{first}'.encode())
got = b''
while True:
    got += os.read(0, 64)
    Path('keys').write_bytes(got)
    # This one test opts in to the redraw a real Codex makes after its paste.
    if Path('render-continue').exists() and got.endswith(b'\x1b[200~continue\x1b[201~'):
        os.write(1, '\x1b[2J\x1b[Hready\r\n\r\n› continue\r\n\r\n\x1b[2m  GPT-6.1-Sol xhigh · Test conversation\x1b[0m\r\n\x1b[3A\x1b[11G'.encode())
    if got.endswith(b'?'):
        os.write(1, '{question}'.encode())
    if got.endswith(b'!'):
        os.write(1, ('\x1b[2;1Hchecked %d' % got.count(b'!')).encode())
"#
    );
    let mut opts = options(dir, "true");
    let command = vec!["python3".into(), "-u".into(), "-c".into(), program];
    opts.config.claude_command = command.clone();
    opts.config.codex_command = command;
    if tool == Tool::Claude {
        crate::providers::claude_keybindings::apply(
            &crate::hosts::logins::LoginPaths {
                claude_dir: opts.claude_dir.clone(),
                codex_dir: opts.codex_dir.clone(),
            },
            &opts.config,
        )
        .unwrap();
    }
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let id = svc.create(tool, None).unwrap();
    wait_for_screen(&svc, id, if ask { "Do you want to proceed?" } else { "ready" });
    (svc, id)
}

/// The keys the recorder kept once barrier number `n` arrived.
pub(super) fn recorded_keys(svc: &Service, dir: &TempDir, id: u64, n: usize) -> Vec<u8> {
    svc.sessions[&id].hosted.as_ref().unwrap().write(b"!".to_vec());
    wait_for_screen(svc, id, &format!("checked {n}"));
    std::fs::read(dir.path().join("keys")).unwrap()
}

#[test]
fn codex_client_stop_preserves_full_input_and_files_in_every_recovery_phase() {
    for phase in [recovery::Phase::Closing, recovery::Phase::Starting, recovery::Phase::Replaying] {
        for key in ["\x1b", "\x03"] {
            let dir = TempDir::new("recovery-stop-custody");
            let (mut svc, id) = key_recorder_service(&dir, Tool::Codex, false);
            let s = svc.sessions.get_mut(&id).unwrap();
            s.recovery.phase = Some(phase);
            s.recovery.phase_at_ms = util::now_ms();
            s.recovery.draft = Some("held caption".into());
            s.recovery.interrupted = true;
            s.recovery.continue_after_replay = true;
            let generation = s.inputs;
            svc.paste_file(id, "first.jpg", "Zml4dHVyZQ==").unwrap();
            svc.paste_file(id, "second.jpg", "c2Vjb25k").unwrap();
            assert_eq!(svc.sessions[&id].inputs, generation, "buffered files cannot invalidate delayed Enter");
            assert!(svc.client_input(id, "\x1b[A").is_err(), "arrows are editing, not Stop");
            assert!(!svc.sessions[&id].recovery.paused);
            let s = svc.sessions.get_mut(&id).unwrap();
            s.recovery.pending = vec!["queued message".into(); 64];
            if key == "\x03" {
                s.recovery.draft = Some("x".repeat(2 * 1024 * 1024));
            }
            let saved = s.recovery.clone();
            svc.pending_submits.push(PendingSubmit::new(Instant::now(), id, 0, generation));
            svc.pending_continues.push(resume::PendingContinue { id, since: Instant::now(), ready_since: None });
            svc.handle_client(0, ClientMsg::Input { id, data: key.into() });
            let stopped = &svc.sessions[&id].recovery;
            assert!(stopped.paused, "Stop works even beyond the byte or message bound");
            assert_eq!(
                (&stopped.draft, &stopped.pending, &stopped.draft_files),
                (&saved.draft, &saved.pending, &saved.draft_files)
            );
            assert!(stopped.draft_files.iter().all(|p| Path::new(p).is_file()));
            assert!(!stopped.interrupted && !stopped.continue_after_replay);
            assert!(svc.pending_submits.is_empty() && svc.pending_continues.is_empty());
            let now = util::now_ms();
            svc.recover_clients(now);
            svc.recover_clients(now + 2000);
            svc.recover_clients(now + 200_000);
            svc.continues_due(Instant::now() + Duration::from_secs(90));
            svc.submit_due(Instant::now() + Duration::from_secs(90));
            let s = svc.sessions.get_mut(&id).unwrap();
            s.tracker.status = Status::Working;
            s.facts.busy = Some(true);
            s.facts.goal_active = Some(true);
            assert_eq!(
                s.retry_after_error(
                    &[],
                    None,
                    &["capacity".into()],
                    provider_automation::RetryContext {
                        now: Instant::now() + Duration::from_secs(900),
                        now_ms: now,
                        usage: &Usage::default(),
                        threshold: 90.0,
                        input_ready: true,
                    },
                ),
                None
            );
            svc.track_working();
            assert!(!svc.registry.sessions.iter().find(|e| e.id == id).unwrap().working);
            assert_eq!(recorded_keys(&svc, &dir, id, 1), b"!", "no stopped paste, Enter or continue reaches the PTY");
            assert!(svc.sessions[&id].recovery.paused);
            svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
        }
    }
}

#[test]
fn codex_client_stop_before_enter_resumes_the_existing_paste_exactly_once() {
    let dir = TempDir::new("recovery-past-retry");
    let (mut svc, id) = key_recorder_service(&dir, Tool::Codex, false);
    svc.sessions.get_mut(&id).unwrap().retry.sent.push(Instant::now());
    for key in ["\x1b", "\x03"] {
        svc.client_input(id, key).unwrap();
        assert!(!svc.sessions[&id].recovery.paused, "historical retries do not intercept native cancellation");
    }
    assert_eq!(recorded_keys(&svc, &dir, id, 1), b"\x1b\x03!");
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    for kind in 0..4 {
        let dir = TempDir::new("recovery-stop-in-flight");
        let (mut svc, id) = key_recorder_service(&dir, Tool::Codex, false);
        let now = Instant::now();
        let s = svc.sessions.get_mut(&id).unwrap();
        s.recovery.phase = if kind == 3 {
            None
        } else {
            Some(if kind == 2 { recovery::Phase::Starting } else { recovery::Phase::Replaying })
        };
        s.recovery.phase_at_ms = util::now_ms();
        s.recovery.draft = Some(if kind == 3 { "" } else { "held caption" }.into());
        let mut expected = String::new();
        if kind >= 2 {
            for (name, bytes) in [("first.jpg", "Zml4dHVyZQ=="), ("second.jpg", "c2Vjb25k")] {
                svc.paste_file(id, name, bytes).unwrap();
            }
            for file in &svc.sessions[&id].recovery.draft_files {
                expected.push_str(&crate::keys::encode_message(file));
            }
            if kind == 3 {
                svc.client_input(id, "\x03").unwrap();
                assert!(
                    svc.sessions[&id].recovery.paused && svc.sessions[&id].recovery.draft_restored,
                    "Stop preserves the healthy native image composer"
                );
                expected.push_str("held caption");
                svc.client_input(id, "held caption\r").unwrap();
            } else {
                let files = svc.sessions[&id].recovery.draft_files.clone();
                let s = svc.sessions.get_mut(&id).unwrap();
                s.recovery.phase = Some(recovery::Phase::Replaying);
                s.tracker.status = Status::Working;
                svc.broadcast_message(0, &[id], "/clear");
                assert_eq!(svc.sessions[&id].recovery.draft_files, files, "a rejected Clear preserves custody");
                let s = svc.sessions.get_mut(&id).unwrap();
                s.recovery.phase = Some(recovery::Phase::Starting);
                s.tracker.status = Status::Idle;
                expected.push_str(&crate::keys::encode_message("held caption"));
                svc.client_input(id, "\r").unwrap();
            }
            let at = util::now_ms();
            svc.recover_clients(at);
            svc.recover_clients(at + 2000);
            assert!(svc.sessions[&id].recovery.draft_restored);
        } else if kind == 1 {
            svc.sessions.get_mut(&id).unwrap().tracker.status = Status::Idle;
            svc.pending_continues.push(resume::PendingContinue { id, since: now, ready_since: Some(now) });
            svc.continues_due(now + Duration::from_secs(4));
            expected = crate::keys::encode_message(&svc.opts.config.resume_message);
        } else {
            let text = "queued before Stop";
            let s = svc.sessions.get_mut(&id).unwrap();
            s.recovery.replay_submitted = Some(text.into());
            s.recovery.pending.push(text.into());
            s.hosted.as_ref().unwrap().write(crate::keys::encode_message(text).into_bytes());
            svc.pending_submits.push(PendingSubmit::new(now, id, 0, s.inputs));
            expected = crate::keys::encode_message(text);
        }
        svc.client_input(id, "\x03").unwrap();
        svc.submit_due(now + Duration::from_secs(20));
        assert_eq!(recorded_keys(&svc, &dir, id, 1), format!("{expected}!").as_bytes());
        svc.client_input(id, "\r").unwrap();
        svc.recover_clients(util::now_ms() + 2100);
        svc.submit_due(Instant::now() + SUBMIT_DELAY);
        assert_eq!(
            recorded_keys(&svc, &dir, id, 2),
            format!("{expected}!\r!").as_bytes(),
            "resume enters the same paste once"
        );
        if kind >= 2 {
            let files = svc.sessions[&id].recovery.draft_files.clone();
            let meta = json!({"type":"session_meta","payload":{"cli_version":"0.167.0"}});
            let receipt = json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"local_image","path":files[0]},{"type":"local_image","path":files[1]},{"type":"text","text":"[Image #1] [Image #2] held caption"}]}}});
            let r = &mut svc.sessions.get_mut(&id).unwrap().recovery;
            r.rollout = Some(dir.write("receipt.jsonl", format!("{meta}\n{receipt}\n")));
            r.offset = 0;
            r.draft_submitted = false;
            r.draft_restored = false;
            r.reconcile().unwrap();
            assert!(r.draft_files.is_empty() && !r.draft_submit_requested, "the native image receipt releases custody");
        }
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn codex_client_native_image_edits_and_path_text_release_custody_on_acceptance() {
    for case in 0..12 {
        let kind = case % 4;
        let header_size = [2 * 1024, 64 * 1024, 2 * 1024 * 1024][case / 4];
        let dir = TempDir::new("native-image-edits");
        let (mut svc, id) = key_recorder_service(&dir, Tool::Codex, false);
        let ordinary_path = dir.write("README.md", "ordinary text file").to_string_lossy().into_owned();
        if kind == 2 {
            svc.client_input(id, &ordinary_path).unwrap();
        }
        svc.paste_file(id, "native.png", "Zml4dHVyZQ==").unwrap();
        let caption = match kind {
            0 => "appended caption".to_string(),
            1 => "edited captionX".to_string(),
            2 => ordinary_path.clone(),
            _ => "a wrapped caption\nwith an edited second line".to_string(),
        };
        if kind != 2 {
            svc.client_input(id, &crate::keys::encode_message(&caption)).unwrap();
        }
        if matches!(kind, 1 | 3) {
            svc.client_input(id, if kind == 1 { "\x7f" } else { "\x1b[D" }).unwrap();
            assert!(svc.sessions[&id].recovery.draft.is_none(), "native edits invalidate replay text");
        }
        let files = svc.sessions[&id].recovery.draft_files.clone();
        assert_eq!(files.len(), 1, "existing path text is never promoted to an owned image");
        if kind == 2 {
            assert_eq!(svc.sessions[&id].recovery.draft.as_deref(), Some(ordinary_path.as_str()));
        }
        svc.client_input(id, "\r").unwrap();
        svc.recover_clients(util::now_ms());
        svc.submit_due(Instant::now() + SUBMIT_DELAY);
        let keys = String::from_utf8(recorded_keys(&svc, &dir, id, 1)).unwrap();
        assert_eq!(keys.matches(files[0].as_str()).count(), 1, "the healthy native composer is not reconstructed");
        assert_eq!(keys.matches('\r').count(), 1, "edited and hidden captions still submit once");
        assert!(svc.sessions[&id].recovery.draft_submitted);
        let meta =
            json!({"type":"session_meta","payload":{"cli_version":"0.167.0","instructions":"x".repeat(header_size)}});
        let user = |path: &str, text: &str| json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"local_image","path":path},{"type":"text","text":text}]}}});
        let wrong = user("/unrelated/image.png", "[Image #1] unrelated");
        let log = dir.write("accepted.jsonl", format!("{meta}\n{wrong}\n"));
        let r = &mut svc.sessions.get_mut(&id).unwrap().recovery;
        r.rollout = Some(log.clone());
        r.offset = 0;
        r.reconcile().unwrap();
        assert_eq!(r.draft_files, files, "an unrelated image receipt cannot release custody");
        let accepted = if kind == 1 { "edited caption".to_string() } else { format!("[Image #1] {caption}") };
        let receipt = user(&files[0], &accepted);
        fs::write(log, format!("{meta}\n{wrong}\n{receipt}\n")).unwrap();
        svc.sessions.get_mut(&id).unwrap().recovery.reconcile().unwrap();
        assert!(svc.sessions[&id].recovery.draft_files.is_empty());
        assert!(!svc.sessions[&id].recovery.draft_submit_requested);
        assert!(svc.sessions[&id].recovery.phase.is_none(), "native acceptance releases healthy input immediately");
        svc.client_input(id, "next message\r").unwrap();
        assert!(recorded_keys(&svc, &dir, id, 2).ends_with(b"next message\r!"), "later healthy input remains usable");
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn codex_client_cancelled_capacity_submits_retire_only_their_automatic_marker() {
    for kind in 0..5 {
        let dir = TempDir::new("capacity-submit-ownership");
        let (mut svc, id) = key_recorder_service(&dir, Tool::Codex, kind == 2);
        if kind == 4 {
            dir.write("render-continue", "");
        }
        let now = Instant::now();
        let s = svc.sessions.get_mut(&id).unwrap();
        s.recovery.replay_submitted = Some("continue".into());
        s.retry.sent.push(now);
        s.hosted.as_ref().unwrap().write(crate::keys::encode_message("continue").into_bytes());
        svc.pending_submits.push(PendingSubmit::new(now, id, 0, s.inputs));
        match kind {
            0 => {
                svc.client_input(id, " edited").unwrap();
                svc.submit_due(now + SUBMIT_DELAY);
            }
            1 => svc.submit_due(now + PASTE_WAIT),
            2 => svc.submit_due(now + SUBMIT_DELAY),
            3 => svc.client_input(id, "\r").unwrap(),
            _ => {
                // The automatic paste still occupies the composer. A refused
                // phone message cannot replace or retire that owned input.
                wait_until(|| {
                    let guard = svc.sessions[&id].hosted.as_ref().unwrap().lock();
                    crate::providers::recovery::codex_draft_nonempty(
                        &screen::screen_text(&guard.term),
                        screen::text_cursor(&guard.term),
                    ) == Some(true)
                });
                let s = &svc.sessions[&id];
                let inputs = s.inputs;
                let revision = s.retry.input_revision;
                let pending = &svc.pending_submits[0];
                let original = (pending.id, pending.inputs, pending.at, pending.until);
                let (out, replies) = mpsc::channel();
                svc.handle(Ev::ClientNew { id: 7, out, frame_busy: Arc::new(AtomicBool::new(false)) });
                while replies.try_recv().is_ok() {}
                svc.broadcast_message(7, &[id], "owner replacement");
                let Out::Msg(reply) = replies.try_recv().unwrap() else { panic!("expected delivery outcome") };
                let reply = crate::protocol::read_msg::<_, ServerMsg>(&mut reply.as_slice()).unwrap().unwrap();
                assert!(
                    matches!(reply, ServerMsg::Notice { error: true, text } if text.contains("finish or clear the desktop draft"))
                );
                let s = &svc.sessions[&id];
                assert_eq!(s.inputs, inputs, "a refused replacement never changes input ownership");
                assert_eq!(s.retry.input_revision, revision);
                assert_eq!(s.recovery.replay_submitted.as_deref(), Some("continue"));
                assert_eq!(svc.pending_submits.len(), 1, "the original automatic Enter remains pending");
                let pending = &svc.pending_submits[0];
                assert_eq!((pending.id, pending.inputs, pending.at, pending.until), original);
                assert!(!svc.messages.busy(), "the refused phone delivery is settled");
                svc.client_input(id, "\x1b").unwrap();
                assert!(svc.sessions[&id].recovery.paused, "Stop still pauses the unsubmitted automatic input");
                assert!(svc.pending_submits.is_empty());
                assert_eq!(svc.sessions[&id].recovery.replay_submitted.as_deref(), Some("continue"));
                svc.submit_due(now + PASTE_WAIT);
                assert_eq!(
                    recorded_keys(&svc, &dir, id, 1),
                    format!("{}!", crate::keys::encode_message("continue")).as_bytes(),
                    "a refused replacement and Stop send no replacement text, Enter or native Esc"
                );
                svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
                continue;
            }
        }
        assert!(
            svc.sessions[&id].recovery.replay_submitted.is_none(),
            "cancelled automatic submission retires its marker (case {kind})"
        );
        svc.client_input(id, "\x1b").unwrap();
        assert!(!svc.sessions[&id].recovery.paused, "later native cancellation is not swallowed");
        assert!(recorded_keys(&svc, &dir, id, 1).ends_with(b"\x1b!"));
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn send_now_presses_claudes_key_once_the_message_it_follows_is_submitted() {
    // Owner, 2026-11-03: "the app from the staging branch doesnt let me
    // interrupt to send messages now like claude code works". Claude Code
    // 2.1.317 queues a message typed during a turn and shows "ctrl+x ctrl+s
    // to send now" under it. A phone tapping Send now on the message it just
    // sent must not press that key before the message's Enter: the key
    // would count as new input and cancel the Enter that is still due.
    let dir = TempDir::new("svc-send-now");
    let (mut svc, id) = key_recorder_service(&dir, Tool::Claude, false);
    svc.broadcast_message(0, &[id], "steer this");
    let sent = Instant::now();
    svc.send_now(id).unwrap();
    let paste = crate::keys::encode_message("steer this").into_bytes();
    let before_enter = recorded_keys(&svc, &dir, id, 1);
    svc.submit_due(sent + SUBMIT_DELAY);
    // Claude must have queued the message before the key can send it.
    let right_after_enter = recorded_keys(&svc, &dir, id, 2);
    svc.keys_due(sent + SUBMIT_DELAY * 2);
    let keys = recorded_keys(&svc, &dir, id, 3);
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    assert_eq!(before_enter, [&paste[..], b"!"].concat(), "nothing is pressed before the message's Enter");
    assert_eq!(right_after_enter, [&paste[..], b"!\r!"].concat(), "the Enter is not cancelled, and goes first");
    assert_eq!(keys, [&paste[..], b"!\r!", SEND_NOW_KEYS.as_bytes(), b"!"].concat());
}

#[test]
fn send_now_is_not_pressed_into_a_question_that_opened_after_the_enter() {
    // Between the Enter and the key, Claude may open a permission question,
    // whose dialog would take the key instead of the queue.
    let dir = TempDir::new("svc-send-now-asked");
    let (mut svc, id) = key_recorder_service(&dir, Tool::Claude, false);
    svc.broadcast_message(0, &[id], "steer this");
    let sent = Instant::now();
    svc.send_now(id).unwrap();
    svc.submit_due(sent + SUBMIT_DELAY);
    svc.sessions[&id].hosted.as_ref().unwrap().write(b"?".to_vec());
    wait_for_screen(&svc, id, "Do you want to proceed?");
    svc.keys_due(sent + SUBMIT_DELAY * 2);
    let keys = recorded_keys(&svc, &dir, id, 1);
    let lines = screen::screen_text(&svc.sessions[&id].hosted.as_ref().unwrap().lock().term);
    let shown = prompt::parse(&lines).unwrap();
    let inputs = svc.sessions[&id].inputs;
    svc.answer(id, inputs, &shown.fingerprint(), 1, &[], None).unwrap();
    svc.keys_due(Instant::now() + KEY_GAP);
    let answered = recorded_keys(&svc, &dir, id, 2);
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    let paste = crate::keys::encode_message("steer this");
    assert_eq!(keys, [paste.as_bytes(), b"\r?!"].concat());
    assert_eq!(
        answered,
        [keys.as_slice(), b"\x1b[B\r!"].concat(),
        "the send-now guard must still allow the Enter of a deliberate question answer"
    );
}

#[test]
fn send_now_presses_claudes_key_at_once_when_no_enter_is_due() {
    let dir = TempDir::new("svc-send-now-queued");
    let (mut svc, id) = key_recorder_service(&dir, Tool::Claude, false);
    svc.send_now(id).unwrap();
    let keys = recorded_keys(&svc, &dir, id, 1);
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    assert_eq!(keys, b"\r!", "phone send-now uses Enter and cannot fall through to Ctrl+S stash");
}

#[test]
fn send_now_holds_an_unrepaired_target_without_losing_its_draft() {
    for case in 0..3 {
        let named = case == 1;
        let dir = TempDir::new("send-now-policy-target");
        let (mut svc, id) = key_recorder_service(&dir, Tool::Claude, false);
        let root = if named { dir.path().join("work") } else { svc.opts.claude_dir.clone() };
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("keybindings.json"), if case == 2 { "not JSON" } else { r#"{"bindings":[]}"# }).unwrap();
        if named {
            svc.sessions.get_mut(&id).unwrap().account_root = Some(root.clone());
        }
        svc.client_input(id, "preserved draft").unwrap();
        let inputs = svc.sessions[&id].inputs;
        let draft = svc.sessions[&id].recovery.draft.clone();
        let result = svc.send_now(id);
        let keys = recorded_keys(&svc, &dir, id, 1);
        if result.is_ok() {
            svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
            panic!("unrepaired target accepted Send now");
        }
        assert!(format!("{:#}", result.unwrap_err()).contains("Enter policy"));
        assert_eq!(keys, b"preserved draft!", "an unrelated default profile cannot authorize this target");
        assert_eq!(svc.sessions[&id].inputs, inputs);
        assert_eq!(svc.sessions[&id].recovery.draft, draft);
        // Correcting malformed input is an owner-file edit; upkeep then applies
        // its fixed policy without changing this existing session or draft.
        fs::write(root.join("keybindings.json"), r#"{"bindings":[]}"#).unwrap();
        crate::providers::claude_keybindings::apply(
            &crate::hosts::logins::LoginPaths { claude_dir: root, codex_dir: svc.opts.codex_dir.clone() },
            &Config::default(),
        )
        .unwrap();
        svc.send_now(id).unwrap();
        let repaired = recorded_keys(&svc, &dir, id, 2);
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
        assert_eq!(repaired, b"preserved draft!\r!", "repair unholds the same receiver target");
    }
}

#[test]
fn send_now_rechecks_policy_before_each_deferred_enter() {
    for after_submit in [false, true] {
        let dir = TempDir::new("send-now-policy-deferred");
        let (mut svc, id) = key_recorder_service(&dir, Tool::Claude, false);
        svc.broadcast_message(0, &[id], "steer this");
        svc.send_now(id).unwrap();
        let at = Instant::now();
        if after_submit {
            svc.submit_due(at + SUBMIT_DELAY);
        }
        fs::write(svc.opts.claude_dir.join("keybindings.json"), r#"{"bindings":[]}"#).unwrap();
        let inputs = svc.sessions[&id].inputs;
        if !after_submit {
            svc.submit_due(at + SUBMIT_DELAY);
        }
        svc.keys_due(at + SUBMIT_DELAY * 2);
        let keys = recorded_keys(&svc, &dir, id, 1);
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
        let paste = crate::keys::encode_message("steer this");
        let ending: &[u8] = if after_submit { b"\r!" } else { b"!" };
        assert_eq!(keys, [paste.as_bytes(), ending].concat(), "held policy cannot silently send a pending draft");
        assert_eq!(svc.sessions[&id].inputs, inputs);
        assert!(svc.pending_submits.is_empty());
        assert!(svc.pending_keys.is_empty());
    }
}

#[test]
fn service_lua_output_wakes_bound_snapshots_but_keep_due_timers_and_requests() {
    let dir = TempDir::new("svc-lua-snapshot-cadence");
    let mut opts = options(&dir, "true");
    opts.config.lua = true;
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    svc.sessions.insert(1, fake_session(1, Tool::Claude));
    let mut engine = crate::lua::Engine::new_service(
        crate::lua::Places { config_dir: dir.path().join("config"), start_dir: dir.path().into() },
        Arc::new(|_| {}),
    )
    .unwrap();
    engine.eval("published=0; deck.on('state', function() published=published+1 end)");
    svc.extensions.engine = Some(engine);
    let start = Instant::now() + Duration::from_secs(1);
    svc.service_plugins_tick(start);
    let old_title = svc.sessions[&1].title.clone();
    svc.sessions.get_mut(&1).unwrap().title = "new output".into();
    svc.launch_capabilities.borrow_mut().take();
    svc.extensions
        .engine
        .as_mut()
        .unwrap()
        .eval("deck.after(0, function() fired=true; deck.service.annotate('sample',1,{timer=true}) end)");
    for millis in 1..250 {
        svc.service_plugins_tick(start + Duration::from_millis(millis));
    }
    assert!(svc.launch_capabilities.borrow().is_none(), "output wakes did not rebuild fleet state");
    assert_eq!(svc.extensions.metadata[&1]["sample"]["timer"], true, "timer requests drain between snapshots");
    let engine = svc.extensions.engine.as_mut().unwrap();
    assert_eq!(engine.eval("=fired"), Some("true".into()));
    assert_eq!(engine.eval("=published"), Some("1".into()));
    assert_eq!(engine.eval("=deck.state({'sessions'}).sessions[1].title"), Some(old_title));
    svc.service_plugins_tick(start + STATUS_INTERVAL);
    let engine = svc.extensions.engine.as_mut().unwrap();
    assert_eq!(engine.eval("=published"), Some("2".into()));
    assert_eq!(engine.eval("=deck.state({'sessions'}).sessions[1].title"), Some("new output".into()));
}

#[test]
fn service_lua_disabled_skips_snapshot_construction() {
    let dir = TempDir::new("svc-lua-disabled-snapshot");
    let (tx, _rx) = mpsc::channel();
    let mut opts = options(&dir, "true");
    opts.config.lua = false;
    let mut svc = Service::new(opts, tx);
    svc.service_plugins_tick(Instant::now());
    assert!(svc.launch_capabilities.borrow().is_none(), "no Lua engine needs no fleet snapshot");
    svc.opts.config.lua = true;
    svc.service_plugins_tick(Instant::now());
    assert!(svc.launch_capabilities.borrow().is_none(), "enabled Lua without an engine needs no fleet snapshot");
    svc.opts.config.lua = false;
    let mut engine = crate::lua::Engine::new_service(
        crate::lua::Places { config_dir: dir.path().join("config"), start_dir: dir.path().into() },
        Arc::new(|_| {}),
    )
    .unwrap();
    engine.eval("deck.after(0, function() fired=true end)");
    svc.extensions.engine = Some(engine);
    svc.service_plugins_tick(Instant::now() + Duration::from_secs(1));
    assert!(svc.launch_capabilities.borrow().is_none(), "disabled Lua does not build an unused snapshot");
    assert_eq!(svc.extensions.engine.as_mut().unwrap().eval("=fired"), Some("true".into()));
}

#[test]
fn send_now_refuses_other_programs_and_a_waiting_question() {
    // Codex has no such key, and in Claude's question dialog the key would
    // reach the dialog instead of the queue.
    for (tool, ask, refusal) in [
        (Tool::Codex, false, "only Claude Code sends a queued message now"),
        (Tool::Claude, true, "is waiting on a question: answer it first"),
    ] {
        let dir = TempDir::new("svc-send-now-refused");
        let (mut svc, id) = key_recorder_service(&dir, tool, ask);
        let error = format!("{:#}", svc.send_now(id).unwrap_err());
        let keys = recorded_keys(&svc, &dir, id, 1);
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
        assert!(error.contains(refusal), "{tool:?}: {error}");
        assert_eq!(keys, b"!", "{tool:?}: no key was pressed");
    }
}

// A PTY stand-in for Claude Code 2.1.317 on a busy machine: its input box
// sits between two rules, and after an image paste the screen stays as it was
// until the image is read (`~`), when the box fills with the message. On
// 2026-11-03, at a load average of 28, a real Claude first drew "Pasting…"
// 3.7 s after the paste, so the deck's Enter 250 ms after it found nothing
// to wait for and was lost (owner: "claude still doesnt send messages until
// after i press enter twice"). `!` is a test barrier, as above.
fn slow_image_paste_service(dir: &TempDir) -> (Service, u64) {
    let program = r#"
import os, sys, tty
from pathlib import Path
tty.setraw(0)
def show(s):
    os.write(1, s.encode())
rule = '─' * 120
def box(text):
    show('\x1b[30;1H' + rule + '\x1b[31;1H\x1b[2K❯ ' + text + '\x1b[32;1H' + rule
         + '\x1b[33;1H  Haiku 5.5 · wk 40%')
show('\x1b[?2004h\x1b[1;1Hready')
box('')
pasted = b''
while not pasted.endswith(b'\x1b[201~'):
    pasted += os.read(0, 1)
Path('pasted').write_bytes(pasted)
early = 0
while True:
    key = os.read(0, 1)
    if key == b'\r':
        early += 1
    elif key == b'!':
        Path('early').write_text(str(early))
        show('\x1b[2;1Hchecked')
    elif key == b'~':
        break
first = pasted[len(b'\x1b[200~'):].split(b'\n')[0].decode()
box('[Image #1]' + first)
while True:
    key = os.read(0, 1)
    if key == b'\r':
        Path('submitted').write_bytes(pasted)
        with Path('enters').open('ab') as entered:
            entered.write(key)
        box('')
        show('\x1b[3;1Hsubmitted')
    elif key == b'!':
        show('\x1b[4;1Hchecked again')
"#;
    let mut opts = options(dir, "true");
    opts.config.claude_command = vec!["python3".into(), "-u".into(), "-c".into(), program.into()];
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let id = svc.create(Tool::Claude, None).unwrap();
    wait_for_screen(&svc, id, "Haiku 5.5");
    (svc, id)
}

#[test]
fn an_image_message_is_submitted_once_claude_shows_it_even_when_pasting_never_showed() {
    let message = "Please inspect this\n\n/synthetic/shot.jpg";
    let dir = TempDir::new("svc-slow-image-paste");
    let (mut svc, id) = slow_image_paste_service(&dir);
    let write = |svc: &Service, key: &[u8]| svc.sessions[&id].hosted.as_ref().unwrap().write(key.to_vec());
    svc.broadcast_message(0, &[id], message);
    let due = Instant::now() + SUBMIT_DELAY;
    // Claude has drawn nothing for the paste yet when the Enter is due.
    svc.submit_due(due);
    svc.submit_due(due + Duration::from_secs(2));
    write(&svc, b"!");
    wait_for_screen(&svc, id, "checked");
    let early = std::fs::read_to_string(dir.path().join("early")).unwrap();
    if early != "0" {
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
        panic!("{early} Enter(s) reached Claude before it showed the paste, and were lost");
    }
    // The image is read and the box shows the message, with no second message.
    write(&svc, b"~");
    wait_for_screen(&svc, id, "[Image #1]Please inspect this");
    svc.submit_due(due + Duration::from_secs(3));
    wait_for_screen(&svc, id, "submitted");
    svc.submit_due(due + Duration::from_secs(4));
    write(&svc, b"!");
    wait_for_screen(&svc, id, "checked again");
    let submitted = std::fs::read(dir.path().join("submitted")).unwrap();
    let enters = std::fs::read(dir.path().join("enters")).unwrap();
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    assert_eq!(submitted, crate::keys::encode_message(message).into_bytes());
    assert_eq!(enters, b"\r", "the message is submitted exactly once");
    assert!(svc.pending_submits.is_empty(), "nothing is left to press Enter later");
}

#[test]
fn a_pasted_file_is_kept_here_and_its_path_pasted_into_the_session() {
    // Owner, 2026-11-01: "my clipboard uses the fedora clipboard instead of
    // this one". The deck sends its own clipboard's image to the host the
    // session runs on, which keeps it and pastes its path.
    let dir = TempDir::new("svc-paste-file");
    let out = dir.path().join("typed");
    let script = format!(
        r#"printf '\033[?2004h'; echo ready; IFS= read -r x; printf '%s' "$x" > '{}'; echo done; read y"#,
        out.display()
    );
    let opts = options(&dir, &script);
    let (socket, state) = (opts.socket.clone(), opts.state_dir.clone());
    let service = thread::spawn(move || run(opts));
    let mut c = TestClient::connect(&socket);
    c.send(ClientMsg::Create {
        tool: Tool::Claude,
        model: None,
        effort: None,
        host: None,
        cwd: None,
        command: Vec::new(),
        name: None,
        request: None,
    });
    let id = c.wait("created", |m, _| match m {
        ServerMsg::Created { id, .. } => Some(*id),
        _ => None,
    });
    c.wait_screen(id, "ready");
    let image = b"\x89PNG\r\n\x1a\n and the rest";
    c.send(ClientMsg::PasteFile { id, name: "shot.png".into(), data: util::base64(image) });
    c.send(ClientMsg::Input { id, data: "\r".into() });
    c.wait_screen(id, "done");
    let kept = state.join("pasted/shot.png");
    assert_eq!(std::fs::read(&kept).unwrap(), image);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), format!("\x1b[200~{}\x1b[201~", kept.display()));
    // A damaged file is refused, and nothing is typed.
    c.send(ClientMsg::PasteFile { id, name: "x.png".into(), data: "not base64!".into() });
    let text = c.wait("refused", |m, _| match m {
        ServerMsg::Notice { text, error: true } => Some(text.clone()),
        _ => None,
    });
    assert!(text.contains("Could not paste the image"), "{text}");
    assert!(!state.join("pasted/x.png").exists());
    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

#[test]
fn broadcast_withholds_enter_when_a_question_appears_first() {
    let dir = TempDir::new("svc-broadcast-question");
    let script = r#"printf '\033[?2004h'; echo ready; read x; echo "got $x"; read y"#;
    let opts = options(&dir, script);
    let socket = opts.socket.clone();
    let service = thread::spawn(move || run(opts));
    let mut c = TestClient::connect(&socket);
    c.send(ClientMsg::Create {
        tool: Tool::Claude,
        model: None,
        effort: None,
        host: None,
        cwd: None,
        command: Vec::new(),
        name: None,
        request: None,
    });
    let id = c.wait("created", |m, _| match m {
        ServerMsg::Created { id, .. } => Some(*id),
        _ => None,
    });
    c.wait_screen(id, "ready");
    c.send(ClientMsg::Broadcast { running_only: false, ids: vec![id], text: "hello".into() });
    // A permission prompt opens during the delay before Enter, which would
    // now approve it instead of submitting the message.
    let payload =
        json!({"notification_type": "permission_prompt", "message": "Claude needs your permission to use Bash"});
    c.send(ClientMsg::Hook { session: id, tool: Tool::Claude, payload });
    let notice = c.wait("withheld notice", |m, _| match m {
        ServerMsg::Notice { text, error: true } if text.contains("question") => Some(text.clone()),
        _ => None,
    });
    assert!(notice.starts_with("Sent to no session") && notice.contains("waiting on a question"), "{notice}");
    thread::sleep(SUBMIT_DELAY * 3);
    let joined = c.screen(id).join("\n");
    assert!(joined.contains("hello"), "the message is typed: {joined}");
    assert!(!joined.contains("got"), "Enter was never pressed: {joined}");
    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

/// The screen of a session hosted by a service the test drives directly.
pub(super) fn screen_of(svc: &Service, id: u64) -> String {
    let h = svc.sessions[&id].hosted.as_ref().unwrap();
    screen::screen_text(&h.lock().term).join("\n")
}

pub(super) fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "condition never held");
        thread::sleep(Duration::from_millis(20));
    }
}

pub(super) fn wait_for_screen(svc: &Service, id: u64, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !screen_of(svc, id).contains(needle) {
        assert!(Instant::now() < deadline, "screen of {id} never showed {needle:?}: {}", screen_of(svc, id));
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn broadcast_reads_each_screen_before_typing_and_before_enter() {
    // Codex asks its approval questions on screen only, with no hook, and the
    // service scans screens every 500 ms; a question that opened since the
    // last scan took the message or its Enter (review CR-20261026-3fdeb2041f).
    // The service loop is not running here, so no scan sees either question.
    // Each question is drawn near the bottom of the screen, where Codex and
    // Claude Code draw theirs and the scan looks.
    let dir = TempDir::new("svc-broadcast-screen");
    let question = "Would you like to run the following command?";
    let script = format!(
        r#"printf '\033[?2004h'; ask() {{ printf '\033[36;1H%s\n' "{question}"; }}; if [ "$CEPHEUS_SESSION" = 2 ]; then ask; else echo ready; stty -icanon -echo min 1; head -c 1 >/dev/null; ask; stty icanon echo; fi; read x; echo "got $x"; read y"#
    );
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, &script), tx);
    let asks_after_paste = svc.create(Tool::Codex, None).unwrap();
    let asking = svc.create(Tool::Codex, None).unwrap();
    wait_for_screen(&svc, asks_after_paste, "ready");
    wait_for_screen(&svc, asking, question);
    svc.broadcast_message(0, &[asks_after_paste, asking], "hello");
    let pending: Vec<u64> = svc.pending_submits.iter().map(|p| p.id).collect();
    assert_eq!(pending, vec![asks_after_paste], "the session already asking was skipped");
    // The paste reaches the other session, which asks before Enter is due.
    wait_for_screen(&svc, asks_after_paste, question);
    svc.submit_due(Instant::now() + SUBMIT_DELAY);
    assert!(svc.pending_submits.is_empty());
    thread::sleep(SUBMIT_DELAY);
    for id in [asks_after_paste, asking] {
        let text = screen_of(&svc, id);
        assert!(!text.contains("got"), "Enter never reached session {id}: {text}");
    }
    assert!(!screen_of(&svc, asking).contains("hello"), "nothing was typed into the asking session");
    for s in svc.sessions.values() {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn a_codex_question_queued_behind_its_turn_needs_you_and_still_takes_messages() {
    // Codex 0.166 shows the questions its agent asks while its turn runs on
    // as a summary above its input box (owner, 2026-10-31: "when codex has a
    // question, nothing indicates on the agent deck that it has a question").
    // Only its own key opens them; Enter in the input box queues a message,
    // so a broadcast or a phone message still reaches the session.
    let dir = TempDir::new("svc-queued-question");
    let script = r#"printf '\033[?2004h'; printf '• Working (3s • esc to interrupt)\n\n• Queued follow-up inputs\n  ? 1 question\n    shift+← to answer\n\n› \033[2mAsk Codex to do anything\033[0m\r\n\033[1A\033[3G'; read x; echo "got $x"; read y"#;
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, script), tx);
    let id = svc.create(Tool::Codex, None).unwrap();
    wait_for_screen(&svc, id, "to answer");
    svc.refresh_status(Instant::now() + Duration::from_secs(1));
    let state = svc.build_state();
    let row = state.sessions.iter().find(|s| s.id == id).unwrap();
    assert_eq!(row.status, Status::NeedsYou, "the list and its count show the question");
    svc.broadcast_message(0, &[id], "hello");
    let pending: Vec<u64> = svc.pending_submits.iter().map(|p| p.id).collect();
    assert_eq!(pending, vec![id], "the message is typed, not skipped");
    svc.submit_due(Instant::now() + SUBMIT_DELAY);
    wait_for_screen(&svc, id, "got ");
    for s in svc.sessions.values() {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

/// The errors kept for the deck's F2 list, oldest first.
pub(super) fn logged(svc: &Service) -> Vec<String> {
    svc.errors.borrow().iter().map(|e| e.text.clone()).collect()
}

#[test]
fn a_codexs_status_line_reaches_the_monitor() {
    // The monitor follows an attached Codex into the thread its status line
    // names (owner, 2026-10-26: "notifications still fire when agents are
    // not done"). The stand-in draws Codex's composer and status line, then
    // puts the cursor back in the composer, as Codex does.
    let dir = TempDir::new("svc-footer");
    let script = r#"printf '\342\200\272 \n  GPT-6.1-Sol xhigh \302\267 Merge everything to master \302\267 weekly 64%% left\n\033[2A\033[3G'; read x"#;
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, script), tx);
    let id = svc.create(Tool::Codex, None).unwrap();
    wait_for_screen(&svc, id, "weekly 64% left");
    let want = Some(["GPT-6.1-Sol xhigh", "Merge everything to master", "weekly 64% left"].map(String::from).to_vec());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut later = Instant::now();
    loop {
        // Each pass is a screen scan: one is due a scan interval after the last.
        later += Duration::from_secs(60);
        svc.refresh_status(later);
        let footer = svc.hosted_refs.lock().unwrap().iter().find(|r| r.id == id).map(|r| r.footer.clone());
        if footer == want || Instant::now() > deadline {
            assert_eq!(footer, want);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn clearing_the_errors_clears_them_for_every_deck() {
    // Owner, 2026-10-26: "there should be a way to clear the error messages";
    // a deck that cleared only its own list saw them again once reopened.
    let dir = TempDir::new("svc-clear-errors");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "echo ready; read x"), tx);
    svc.log_error("Dev server failed (exit 1 after 1s): boom");
    svc.log_error("Codex #2 exited with status 1");
    assert_eq!(svc.build_state().errors.len(), 2);
    svc.handle_client(0, ClientMsg::ClearErrors);
    assert!(logged(&svc).is_empty());
    assert!(svc.build_state().errors.is_empty(), "the state every deck gets lists none");
    svc.log_error("a later error");
    assert_eq!(logged(&svc), vec!["a later error".to_string()], "new errors still arrive");
}

#[test]
fn a_codex_session_stopped_at_capacity_gets_continue_after_a_wait() {
    // Owner, 2026-10-26: "can you make it so it automatically sends
    // contontinue if this codex error appears", showing "■ Selected model is
    // at capacity. Please try a different model." above an empty input box.
    let dir = TempDir::new("svc-auto-continue");
    let script = r#"printf '\033[?2004h'; printf '■ Selected model is at capacity. Please try a different model.\r\n\r\n› \033[2mAsk anything\033[0m\r\n\033[1A\033[3G'; stty -echo; while IFS= read -r x; do printf 'got %s\n' "$x"; done"#;
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, script), tx);
    let codex = svc.create(Tool::Codex, None).unwrap();
    let claude = svc.create(Tool::Claude, None).unwrap();
    for id in [codex, claude] {
        wait_for_screen(&svc, id, "›");
    }
    let start = Instant::now() + Duration::from_secs(1);
    svc.refresh_status(start);
    assert!(svc.pending_submits.is_empty(), "the first continue waits");
    svc.refresh_status(start + AUTO_CONTINUE_FIRST_WAIT - Duration::from_secs(1));
    assert!(svc.pending_submits.is_empty(), "the first continue waits");
    svc.refresh_status(start + AUTO_CONTINUE_FIRST_WAIT);
    let pending: Vec<u64> = svc.pending_submits.iter().map(|p| p.id).collect();
    assert_eq!(pending, vec![codex], "only the Codex session is answered");
    wait_for_screen(&svc, codex, "continue");
    svc.client_input(codex, "\x1b").unwrap();
    assert!(svc.sessions[&codex].recovery.paused, "the automatic capacity paste can be stopped");
    svc.refresh_status(start + AUTO_CONTINUE_FIRST_WAIT * 20);
    assert!(svc.pending_submits.is_empty(), "stale capacity text cannot resume a stopped session");
    svc.client_input(codex, "\r").unwrap();
    svc.submit_due(start + AUTO_CONTINUE_FIRST_WAIT + SUBMIT_DELAY);
    wait_for_screen(&svc, codex, "got </cross-session-message>");
    let text = screen_of(&svc, codex);
    assert!(text.contains("got <cross-session-message source=\"cepheus\" encoding=\"html\">"), "{text}");
    assert!(text.lines().any(|l| l.starts_with("got") && l.contains("continue")), "{text}");
    assert!(!screen_of(&svc, claude).contains("continue"), "Claude Code is never typed into");
    assert_eq!(logged(&svc), vec!["Codex: \"Selected model is at capacity\"; typed continue (1 of 8 in 30 minutes)"]);
    // The turn went on: nothing more is typed.
    svc.refresh_status(start + AUTO_CONTINUE_FIRST_WAIT * 40);
    assert!(svc.pending_submits.is_empty());
    for s in svc.sessions.values() {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn automatic_continues_wait_longer_each_time_and_stop_at_the_limit() {
    let mut retry = AutoContinue::default();
    let mut now = Instant::now();
    let mut waits = Vec::new();
    for _ in 0..AUTO_CONTINUE_LIMIT {
        let seen = now;
        assert!(!retry.due(now), "a new error is never answered at once");
        while !retry.due(now) {
            now += Duration::from_secs(1);
        }
        waits.push(now.duration_since(seen).as_secs());
        retry.sent.push(now);
        retry.seen = None;
    }
    assert_eq!(waits, vec![10, 20, 40, 80, 160, 300, 300, 300]);
    // Eight in half an hour: the session waits for the owner.
    assert!(!retry.due(now + Duration::from_secs(600)));
    // Once the first is half an hour old, one more follows the longest wait.
    let later = retry.sent[0] + AUTO_CONTINUE_WINDOW;
    retry.seen = None;
    assert!(!retry.due(later));
    assert!(retry.due(later + AUTO_CONTINUE_MAX_WAIT));
}

fn handover_options(dir: &TempDir) -> ServiceOptions {
    let mut opts = dev_options(dir, &["sh", "-c", "echo serving; exec sleep 30"]);
    let script = r#"echo "ready $CEPHEUS_SESSION"; read x; echo "got $x"; read y; exit 4"#;
    opts.config.claude_command = vec!["sh".into(), "-c".into(), script.into()];
    opts.config.codex_command = opts.config.claude_command.clone();
    opts
}

#[test]
fn an_update_hands_live_sessions_the_dev_server_and_errors_to_the_next_build() {
    // Owner, 2026-10-26: "make it so the service just auto updates in place"
    // after "i have agents running to the older service, hard to stop and
    // start it, cant lose whats running". The exec itself is covered by
    // tests/upgrade.rs; here both builds share the test process.
    let dir = TempDir::new("svc-handover");
    let (tx, _rx) = mpsc::channel();
    let mut old = Service::new(handover_options(&dir), tx);
    let claude = old.create(Tool::Claude, None).unwrap();
    let codex = old.create(Tool::Codex, None).unwrap();
    for id in [claude, codex] {
        wait_for_screen(&old, id, &format!("ready {id}"));
    }
    old.toggle_dev_server(1, true);
    let dev_pid = old.native.dev.as_ref().expect("the dev server runs").pid;
    old.log_error("an earlier error");
    old.sessions.get_mut(&claude).unwrap().tracker.unseen = true;
    old.sessions.get_mut(&claude).unwrap().owner_draft = true;
    let conversation = old.sessions[&claude].conversation.clone();
    assert!(conversation.is_some());

    let state = old.prepare_handover(Fds { listener: 100, lock: 101 }).unwrap();
    assert_eq!(state.sessions.iter().map(|s| s.id).collect::<Vec<_>>(), vec![claude, codex]);
    let path = handover::handover_path(&old.opts.socket);
    state.write(&path).unwrap();
    // What exec does to the old build: its values vanish, its terminals stay open.
    for s in old.sessions.values_mut() {
        if let Some(h) = s.hosted.take() {
            h.release();
        }
    }
    old.native.dev = None;
    old.registry_dirty = true;
    old.save_registry();
    drop(old);

    let (tx, rx) = mpsc::channel();
    let mut new = Service::new(handover_options(&dir), tx);
    new.adopt(Handover::take(&path).unwrap());
    for id in [claude, codex] {
        assert!(new.sessions[&id].alive, "session {id} lives on");
        wait_for_screen(&new, id, &format!("ready {id}"));
    }
    assert!(new.sessions[&claude].tracker.unseen, "its status comes along");
    assert!(new.sessions[&claude].owner_draft, "unsubmitted owner input remains protected across handover");
    assert!(!new.sessions[&codex].owner_draft, "a known clean draft keeps its state");
    assert_eq!(new.sessions[&claude].conversation, conversation);
    assert_eq!(logged(&new), vec!["an earlier error".to_string()]);
    assert_eq!(new.dev_server_state().pid, Some(dev_pid), "the dev server is still the deck's");
    let registry = Registry::load(&dir.path().join("state/sessions.json"));
    assert!(registry.sessions.iter().all(|e| e.ended_at.is_none()), "{:?}", registry.sessions);
    assert_eq!(new.create(Tool::Claude, None).unwrap(), codex + 1, "ids keep counting");

    new.handle_client(0, ClientMsg::Input { id: claude, data: "hello\r".into() });
    wait_for_screen(&new, claude, "got hello");
    claude_transcript(&dir, conversation.as_deref().unwrap());
    new.handle_client(0, ClientMsg::Input { id: claude, data: "\r".into() });
    let deadline = Instant::now() + Duration::from_secs(10);
    while new.sessions[&claude].alive {
        assert!(Instant::now() < deadline, "the adopted program's exit was never seen");
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(50)) {
            new.handle(ev);
        }
    }
    assert_eq!(new.sessions[&claude].exit_code, Some(4));

    new.toggle_dev_server(1, false);
    wait_for_dev_end(&mut new);
    for s in new.sessions.values().filter(|s| s.alive) {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn an_update_waits_while_the_service_is_mid_action() {
    let dir = TempDir::new("svc-update-wait");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    assert!(!svc.mid_action());
    svc.pending_submits.push(PendingSubmit::new(Instant::now(), 1, 1, 0));
    assert!(svc.mid_action(), "a broadcast's Enter is still due");
    svc.pending_submits.clear();
    svc.moves_in_flight = 1;
    assert!(svc.mid_action(), "a session is moving in");
    svc.moves_in_flight = 0;
    let mut closing = fake_session(3, Tool::Claude);
    closing.closing = Some((Instant::now(), 1));
    svc.sessions.insert(3, closing);
    assert!(svc.mid_action(), "a session is closing");
}

fn dev_options(dir: &TempDir, command: &[&str]) -> ServiceOptions {
    let mut opts = options(dir, "true");
    opts.config.dev_server_command = command.iter().map(|s| s.to_string()).collect();
    util::ensure_private_dir(&opts.state_dir).unwrap();
    opts
}

/// Run the service's housekeeping until its dev server has ended.
fn wait_for_dev_end(svc: &mut Service) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while svc.native.dev.is_some() {
        assert!(Instant::now() < deadline, "the dev server never ended");
        svc.housekeeping(Instant::now());
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_dev_button_starts_and_stops_a_server_the_deck_runs() {
    // Owner, 2026-10-26: "just a toggle button to start or stop the dev
    // server". The tests never use port 8000, which is the owner's alone.
    let dir = TempDir::new("svc-dev");
    let command = "echo serving in $(pwd -P); exec sleep 30";
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(dev_options(&dir, &["sh", "-c", command]), tx);
    svc.toggle_dev_server(1, true);
    let state = svc.dev_server_state();
    let pid = state.pid.expect("the server runs");
    assert!(state.started_at.is_some() && !state.port_busy, "{state:?}");
    assert_eq!(state.command, format!("sh -c {command}"));
    let log = dir.path().join("state/dev-server.log");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(&log).unwrap_or_default().contains('\n') {
        assert!(Instant::now() < deadline, "the server never wrote its log");
        thread::sleep(Duration::from_millis(20));
    }
    let cwd = fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("serving in {}\n", cwd.display()),
        "it runs in the start directory"
    );
    // A second start leaves the running server alone.
    svc.toggle_dev_server(1, true);
    assert_eq!(svc.dev_server_state().pid, Some(pid));
    svc.toggle_dev_server(1, false);
    let state = svc.dev_server_state();
    assert_eq!((state.pid, state.started_at.is_some()), (None, true), "stopping: {state:?}");
    wait_for_dev_end(&mut svc);
    assert!(!Path::new(&format!("/proc/{pid}")).exists(), "the server ended and was reaped");
    let state = svc.dev_server_state();
    assert_eq!((state.pid, state.started_at, state.port_busy), (None, None, false));
    assert!(logged(&svc).is_empty(), "a stop is not an error: {:?}", logged(&svc));
}

#[test]
fn a_dev_server_that_fails_is_listed_with_its_last_output() {
    // Owner, 2026-10-26: "a f2 in the agent-desk itself for errors say for
    // example if the dev server fails to start".
    let dir = TempDir::new("svc-dev-fail");
    let script = "echo Watching for file changes; echo 'Error: That port is already in use.' >&2; exit 1";
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(dev_options(&dir, &["sh", "-c", script]), tx);
    svc.toggle_dev_server(1, true);
    wait_for_dev_end(&mut svc);
    let log = logged(&svc);
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].starts_with("Dev server failed (exit 1 after "), "{log:?}");
    assert!(
        log[0].ends_with(
            "s): Error: That port is already in use.\nWatching for file changes\nError: That port is already in use."
        ),
        "{log:?}"
    );
    assert_eq!(svc.dev_server_state().pid, None);
    // A program that is not there fails at once, naming where it looked.
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(dev_options(&dir, &[".venv/bin/python", "manage.py", "runserver"]), tx);
    svc.toggle_dev_server(1, true);
    assert!(svc.native.dev.is_none());
    let log = logged(&svc);
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].starts_with("Dev server failed to start: starting .venv/bin/python in "), "{log:?}");
}

#[test]
fn a_dev_server_running_an_older_build_than_its_sources_is_no_error() {
    // Owner, 2026-11-07: "the source files being newer shouldnt be a "nope
    // doesnt work" rather it uses the old one and says "warning not the
    // latest version etc"". The dev server warns and runs on; only a server
    // that ends on its own is an error.
    let dir = TempDir::new("svc-dev-stale");
    let script = "echo '[startup] Warning: Sample is not the latest version (its sources are newer). \
                  Continuing using the existing binary.' >&2; exec sleep 30";
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(dev_options(&dir, &["sh", "-c", script]), tx);
    svc.toggle_dev_server(1, true);
    let log = dir.path().join("state/dev-server.log");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(&log).unwrap_or_default().contains("Warning") {
        assert!(Instant::now() < deadline, "the server never wrote its log");
        svc.housekeeping(Instant::now());
        thread::sleep(Duration::from_millis(20));
    }
    for _ in 0..10 {
        svc.housekeeping(Instant::now());
        thread::sleep(Duration::from_millis(20));
    }
    assert!(svc.dev_server_state().pid.is_some(), "it runs on the existing build");
    assert!(logged(&svc).is_empty(), "a warning is not an error: {:?}", logged(&svc));
    svc.toggle_dev_server(1, false);
    wait_for_dev_end(&mut svc);
    assert!(logged(&svc).is_empty(), "{:?}", logged(&svc));
}

#[test]
fn a_port_held_by_another_server_is_reported_and_never_touched() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let address = format!("127.0.0.1:{port}");
    let dir = TempDir::new("svc-dev-busy");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(dev_options(&dir, &["sh", "-c", "exec sleep 30", &address]), tx);
    let checked = Instant::now();
    svc.housekeeping(checked);
    let state = svc.dev_server_state();
    assert_eq!((state.port, state.port_busy), (Some(port), true));
    svc.toggle_dev_server(1, true);
    assert!(svc.native.dev.is_none(), "nothing was started");
    svc.toggle_dev_server(1, false);
    assert_eq!(
        logged(&svc),
        vec![
            format!("Dev server not started: something already listens on port {port}."),
            format!("Port {port} belongs to a server the deck did not start; stop that one where it runs."),
        ]
    );
    drop(listener);
    // A child another test is starting can hold a copy of the socket until it
    // runs its program (it failed twice in full runs on 2026-10-26).
    let deadline = Instant::now() + Duration::from_secs(10);
    while devserver::port_listening(port) {
        assert!(Instant::now() < deadline, "port {port} stayed open");
        thread::sleep(Duration::from_millis(20));
    }
    svc.housekeeping(checked + PORT_CHECK_INTERVAL);
    assert!(!svc.dev_server_state().port_busy, "the port is looked up again");
}

#[test]
fn a_failed_start_leaves_the_list_with_its_reason_logged() {
    // Owner, 2026-11-03, of a Codex that failed to start on Fedora: "it stays
    // listed as a ghost session when its ended". A session that ends by
    // itself before it has a conversation has nothing to reopen; one with a
    // conversation, and one the deck closed, stay listed.
    let dir = TempDir::new("svc-exit");
    let go = dir.path().join("go");
    let script = format!(
        r#"go='{}'
case "$CEPHEUS_SESSION" in
  4) trap 'kill $! 2>/dev/null; exit 7' HUP; echo waiting; sleep 30 & wait ;;
  5) echo ready; while [ ! -e "$go" ]; do sleep 0.05; done; exit 0 ;;
  *) echo "Error: start $CEPHEUS_SESSION failed"; while [ ! -e "$go" ]; do sleep 0.05; done; exit $((CEPHEUS_SESSION + 2)) ;;
esac"#,
        go.display()
    );
    let (tx, rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, &script), tx);
    let failing = svc.create(Tool::Claude, None).unwrap();
    let chatted = svc.create(Tool::Claude, None).unwrap();
    let codex = svc.create(Tool::Codex, None).unwrap();
    let closed = svc.create(Tool::Codex, None).unwrap();
    let quit = svc.create(Tool::Claude, None).unwrap();
    for id in [failing, chatted, codex] {
        wait_for_screen(&svc, id, "failed");
    }
    wait_for_screen(&svc, closed, "waiting");
    wait_for_screen(&svc, quit, "ready");
    // The second Claude wrote its conversation down before it failed; the
    // others were given an id but never used it.
    claude_transcript(&dir, &svc.sessions[&chatted].conversation.clone().unwrap());
    svc.close(closed);
    fs::write(&go, "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while svc.sessions.values().any(|s| s.alive) {
        assert!(Instant::now() < deadline, "the sessions never ended");
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
            svc.handle(ev);
        }
    }
    let listed: Vec<u64> = svc.sessions.keys().copied().collect();
    let registered: Vec<u64> = svc.registry.sessions.iter().map(|e| e.id).collect();
    assert_eq!(
        (listed.as_slice(), registered.as_slice()),
        ([chatted, closed].as_slice(), [chatted, closed].as_slice())
    );
    assert_eq!((svc.sessions[&chatted].exit_code, svc.sessions[&closed].exit_code), (Some(4), Some(7)));
    let mut errors = logged(&svc);
    errors.sort();
    assert_eq!(
        errors,
        vec![
            "Claude exited with status 3: Error: start 1 failed",
            "Claude exited with status 4",
            "Codex exited with status 5: Error: start 3 failed",
        ]
    );
}

#[test]
fn review_clients_wait_for_hello_then_receive_only_compatible_sessions() {
    let dir = TempDir::new("svc-hello-version");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    for (i, tool) in [Tool::Claude, Tool::Codex, Tool::Antigravity, Tool::Grok, Tool::OpenCode].into_iter().enumerate()
    {
        let id = i as u64 + 1;
        svc.sessions.insert(id, fake_session(id, tool));
    }
    let state = Arc::new(svc.build_state());
    let receive = |rx: &Receiver<Out>| match rx.try_recv().expect("an outgoing message") {
        Out::Msg(buf) => read_msg::<_, ServerMsg>(&mut buf.as_slice()).unwrap().unwrap(),
        Out::Frame(_) => panic!("a control connection received a frame"),
    };
    for version in 1..=PROTOCOL_VERSION {
        let cid = u64::from(version);
        let (out, rx) = mpsc::channel();
        svc.handle(Ev::ClientNew { id: cid, out, frame_busy: Arc::new(AtomicBool::new(false)) });
        // ClientNew precedes the reader's Hello event. A state change in
        // between must not send new Tool variants to an older decoder.
        svc.broadcast_state(&state);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)), "state arrived before protocol negotiation");
        svc.broadcast(&ServerMsg::Notice { text: "a state change".into(), error: false });
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)), "a broadcast arrived before Welcome");

        svc.handle_client(cid, ClientMsg::Hello { version, role: ClientRole::Control });
        assert!(matches!(receive(&rx), ServerMsg::Welcome { .. }), "Welcome must be first");
        let ServerMsg::State(first) = receive(&rx) else { panic!("initial state") };
        let expected: &[Tool] = match version {
            1 => &[Tool::Claude, Tool::Codex],
            2 | 3 => &[Tool::Claude, Tool::Codex, Tool::Antigravity],
            _ => &[Tool::Claude, Tool::Codex, Tool::Antigravity, Tool::Grok, Tool::OpenCode],
        };
        assert_eq!(first.sessions.iter().map(|s| s.tool).collect::<Vec<_>>(), expected);
        svc.broadcast_state(&state);
        if version >= STATE_PATCH_PROTOCOL {
            // From protocol 20 an unchanged state is not sent again, and a
            // change arrives as only the part that changed.
            assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)), "an unchanged state was sent again");
            let moved = Arc::new(DeckState { start_dir: "/elsewhere".into(), ..(*state).clone() });
            svc.broadcast_state(&moved);
            let ServerMsg::StatePatch(patch) = receive(&rx) else { panic!("a state patch") };
            assert_eq!(patch.fields(), ["start_dir"]);
            assert_eq!(patch.start_dir.as_deref(), Some("/elsewhere"));
        } else {
            let ServerMsg::State(next) = receive(&rx) else { panic!("updated state") };
            assert_eq!(next.sessions.iter().map(|s| s.tool).collect::<Vec<_>>(), expected);
        }
        // Keep earlier clients' broadcasts out of this iteration's queue.
        svc.handle(Ev::ClientGone(cid));
    }
}

#[test]
fn a_phone_answer_is_pressed_only_while_the_question_it_read_still_waits() {
    // Review AR-20261027-339e742b7b: a notification's answer is checked and
    // pressed by the service in one step, against the input count and the
    // question the phone read, so an answer typed at the desk meanwhile, a
    // repeated tap or an identical later question never takes it.
    let dir = TempDir::new("svc-answer");
    let script = r#"printf '\033[30;1HDo you want to proceed?\n❯ 1. Yes\n  2. No\n'; stty -echo; read x; printf 'got %s\n' "$x" | cat -v; read y"#;
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, script), tx);
    let id = svc.create(Tool::Claude, None).unwrap();
    wait_for_screen(&svc, id, "2. No");
    let lines = screen::screen_text(&svc.sessions[&id].hosted.as_ref().unwrap().lock().term);
    let fp = prompt::parse(&lines).expect("the question is read").fingerprint();
    let read_at = svc.sessions[&id].inputs;

    let err = svc.answer(id, read_at, "another question", 1, &[], None).unwrap_err();
    assert!(format!("{err}").contains("question changed"), "{err}");
    // The phone is told that question no longer waits, by code.
    assert!(crate::protocol::is_question_stale(&err), "{err}");
    // Typed at the desk after the phone read the question.
    svc.handle_client(1, ClientMsg::Input { id, data: "n".into() });
    let err = svc.answer(id, read_at, &fp, 1, &[], None).unwrap_err();
    assert!(format!("{err}").contains("took other input"), "{err}");
    assert!(!crate::protocol::is_question_stale(&err), "the question still waits: {err}");
    assert!(svc.pending_keys.is_empty());

    let read_at = svc.sessions[&id].inputs;
    svc.answer(id, read_at, &fp, 1, &[], None).unwrap();
    assert_eq!(svc.pending_keys.len(), 1, "the arrow goes at once and Enter after it");
    // A second tap of the same notification: the question still shows.
    let err = svc.answer(id, read_at, &fp, 1, &[], None).unwrap_err();
    assert!(format!("{err}").contains("took other input"), "{err}");
    svc.keys_due(Instant::now() + KEY_GAP);
    assert!(svc.pending_keys.is_empty());
    wait_for_screen(&svc, id, "got n^[[B");
    let text = screen_of(&svc, id);
    assert_eq!(text.matches("got").count(), 1, "one answer: {text}");
    for s in svc.sessions.values() {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn the_rest_of_a_phone_answer_is_not_pressed_into_newer_desk_input() {
    // ROOT-034: the answer's arrow went at once and its Enter waited; typing
    // at the desk in between had that Enter submit the owner's text.
    let dir = TempDir::new("svc-answer-rest");
    let script = r#"printf '\033[30;1HDo you want to proceed?\n❯ 1. Yes\n  2. No\n'; stty -echo; read x; printf 'got %s\n' "$x" | cat -v; read y"#;
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, script), tx);
    let id = svc.create(Tool::Claude, None).unwrap();
    wait_for_screen(&svc, id, "2. No");
    let lines = screen::screen_text(&svc.sessions[&id].hosted.as_ref().unwrap().lock().term);
    let fp = prompt::parse(&lines).expect("the question is read").fingerprint();
    let read_at = svc.sessions[&id].inputs;

    svc.answer(id, read_at, &fp, 1, &[], None).unwrap();
    assert_eq!(svc.pending_keys.len(), 1, "the arrow goes at once and Enter after it");
    svc.handle_client(1, ClientMsg::Input { id, data: "x".into() });
    svc.keys_due(Instant::now() + KEY_GAP);
    assert!(svc.pending_keys.is_empty());
    svc.handle_client(1, ClientMsg::Input { id, data: "y\r".into() });
    wait_for_screen(&svc, id, "got ");
    let text = screen_of(&svc, id);
    assert!(text.contains("got ^[[Bxy"), "the owner's own Enter submits his text: {text}");
    for s in svc.sessions.values() {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn a_phone_answers_a_several_choice_question_and_its_review() {
    // Owner, 2026-10-30: "the app doesnt let me answer questions". Claude
    // Code 2.1.314 ticks a box with Enter, sends the boxes from its Submit
    // row, then asks "Ready to submit your answers?"; the phone's Submit is
    // the answer, so the service presses that last Enter too.
    let dir = TempDir::new("svc-several");
    let script = r#"printf '\033[2J\033[1;1HPick toppings?\n\n❯ 1. [ ] Cheese\n  2. [ ] Olives\n  3. [ ] Type something\n     Submit\n──────────\n  4. Chat about this\n\nEnter to select · ↑/↓ to navigate\n'; stty -echo; read a; read b; printf '\033[2J\033[1;1HReview your answers\n\nReady to submit your answers?\n\n❯ 1. Submit answers\n  2. Cancel\n'; read c; printf 'submitted %s|%s|%s\n' "$a" "$b" "$c" | cat -v; read d"#;
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, script), tx);
    let id = svc.create(Tool::Claude, None).unwrap();
    wait_for_screen(&svc, id, "Enter to select");
    let lines = screen::screen_text(&svc.sessions[&id].hosted.as_ref().unwrap().lock().term);
    let shown = prompt::parse(&lines).expect("the question is read");
    assert!(shown.multi && shown.submit.is_some(), "{shown:?}");
    let read_at = svc.sessions[&id].inputs;

    // Olives: Down, Enter to tick it, then Down twice to Submit and Enter.
    svc.answer(id, read_at, &shown.fingerprint(), 0, &[1], None).unwrap();
    assert_eq!(svc.pending_keys.len(), 4);
    svc.keys_due(Instant::now() + KEY_GAP * 10);
    wait_for_screen(&svc, id, "Submit answers");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !screen_of(&svc, id).contains("submitted") && Instant::now() < deadline {
        svc.reviews_due(Instant::now() + REVIEW_POLL);
        std::thread::sleep(Duration::from_millis(20));
    }
    let text = screen_of(&svc, id);
    assert!(text.contains("submitted ^[[B|^[[B^[[B|"), "the review was confirmed once: {text}");
    assert!(svc.pending_reviews.is_empty());
    for s in svc.sessions.values() {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn a_review_after_input_from_the_desk_is_left_alone() {
    let dir = TempDir::new("svc-review-desk");
    let script = r#"printf '\033[2J\033[1;1HPick one\n❯ 1. Red\n  2. Blue\n\nEnter to select\n'; stty -echo; read a; printf '\033[2J\033[1;1HReady to submit your answers?\n\n❯ 1. Submit answers\n  2. Cancel\n'; read b; printf 'pressed\n'; read c"#;
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, script), tx);
    let id = svc.create(Tool::Claude, None).unwrap();
    wait_for_screen(&svc, id, "Enter to select");
    let lines = screen::screen_text(&svc.sessions[&id].hosted.as_ref().unwrap().lock().term);
    let fp = prompt::parse(&lines).unwrap().fingerprint();
    let read_at = svc.sessions[&id].inputs;
    svc.answer(id, read_at, &fp, 0, &[], None).unwrap();
    assert_eq!(svc.pending_reviews.len(), 1);
    wait_for_screen(&svc, id, "Submit answers");
    // The desk moves the cursor first: the review is now the desk's.
    svc.handle_client(1, ClientMsg::Input { id, data: "\x1b[B".into() });
    svc.reviews_due(Instant::now() + REVIEW_POLL);
    assert!(svc.pending_reviews.is_empty());
    std::thread::sleep(Duration::from_millis(300));
    assert!(!screen_of(&svc, id).contains("pressed"));
    for s in svc.sessions.values() {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn alerts_reach_only_phone_gateways() {
    // A deck older than protocol 5 cannot read an alert and would drop the
    // connection, so only `remote` clients are sent one.
    let dir = TempDir::new("svc-alerts");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let mut clients = Vec::new();
    for (cid, role) in [(1, ClientRole::Control), (2, ClientRole::Remote), (3, ClientRole::Tui)] {
        let (out, rx) = mpsc::channel();
        svc.handle(Ev::ClientNew { id: cid, out, frame_busy: Arc::new(AtomicBool::new(false)) });
        svc.handle_client(cid, ClientMsg::Hello { version: PROTOCOL_VERSION, role });
        clients.push(rx);
    }
    let drain = |rx: &Receiver<Out>| -> Vec<ServerMsg> {
        rx.try_iter()
            .filter_map(|o| match o {
                Out::Msg(buf) => read_msg::<_, ServerMsg>(&mut buf.as_slice()).unwrap(),
                Out::Frame(_) => None,
            })
            .collect()
    };
    for rx in &clients {
        drain(rx);
    }
    svc.announce_error("Codex #3 gave up after 5 tries");
    let got: Vec<Vec<ServerMsg>> = clients.iter().map(drain).collect();
    let alerts = |msgs: &[ServerMsg]| -> Vec<(AlertKind, String)> {
        msgs.iter()
            .filter_map(|m| match m {
                ServerMsg::Alert(a) => Some((a.kind, a.reason.clone())),
                _ => None,
            })
            .collect()
    };
    assert_eq!(alerts(&got[1]), vec![(AlertKind::Error, "Codex #3 gave up after 5 tries".to_string())]);
    for i in [0, 2] {
        assert!(alerts(&got[i]).is_empty(), "client {i} got an alert");
        assert!(
            got[i].iter().any(|m| matches!(m, ServerMsg::Notice { error: true, .. })),
            "the notice still reaches every deck"
        );
    }
}

#[test]
fn a_typed_grok_turn_ends_with_one_finished_alert_from_groks_own_record() {
    // The device check's stand-in, hosted as Grok, showed `esc to interrupt`
    // on row 3 of an otherwise blank screen for 4 s; the deck never saw the
    // turn, so the phone got no "finished" alert. Grok reports no turn end
    // itself: its screen going quiet only marks the turn, and Grok's own
    // record of the turn's end announces it, once (owner, 2026-11-07:
    // "notifications that agents are finished when no agents are finished").
    let dir = TempDir::new("svc-finish-alert");
    let script = r#"printf '\033[?2004hready\n> '; read x; printf 'thinking (esc to interrupt)'; sleep 2; printf '\r\033[2Kdone\n> '; read y"#;
    let mut opts = options(&dir, script);
    opts.config.grok_command = vec!["sh".into(), "-c".into(), script.into()];
    let grok_home = opts.grok_home.clone();
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let (out, rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 2, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(2, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Remote });
    let (desk, _desk) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 3, out: desk, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(3, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Tui });
    let id = svc.create(Tool::Grok, None).unwrap();
    let conv = svc.sessions[&id].conversation.clone().expect("the deck picks a Grok conversation's id");
    // Grok's record of the conversation, with a turn it ended before.
    let folder = grok_home.join("sessions/%2Fsrv%2Frepo").join(&conv);
    fs::create_dir_all(&folder).unwrap();
    let events = folder.join("events.jsonl");
    let turn = |start: &str, end: &str| {
        format!(
            "{{\"ts\":\"{start}\",\"type\":\"turn_started\",\"session_id\":\"{conv}\"}}\n\
             {{\"ts\":\"{start}\",\"type\":\"phase_changed\",\"phase\":\"streaming_text\"}}\n\
             {{\"ts\":\"{end}\",\"type\":\"turn_ended\",\"outcome\":\"completed\"}}\n"
        )
    };
    fs::write(&events, turn("2026-11-01T10:00:00.000Z", "2026-11-01T10:00:09.000Z")).unwrap();
    // What the monitor reads of it each cycle.
    let report = || Report {
        facts: HashMap::from([(
            id,
            SessionFacts {
                known_conversation: Some(conv.clone()),
                conversation: Some(conv.clone()),
                turn_end: other::grok_events(&grok_home, &conv)
                    .and_then(|path| util::read_tail(&path, 64 * 1024).ok())
                    .and_then(|tail| other::grok_turn_end(&tail)),
                ..SessionFacts::default()
            },
        )]),
        ..Report::default()
    };
    let finished_alerts = |rx: &Receiver<Out>| -> usize {
        rx.try_iter()
            .filter(|o| match o {
                Out::Msg(buf) => matches!(
                    read_msg::<_, ServerMsg>(&mut buf.as_slice()),
                    Ok(Some(ServerMsg::Alert(a))) if a.kind == AlertKind::Finished && a.id == Some(id)
                ),
                Out::Frame(_) => false,
            })
            .count()
    };
    let tick = |svc: &mut Service| {
        let now = Instant::now();
        svc.submit_due(now);
        svc.refresh_status(now);
        thread::sleep(Duration::from_millis(50));
    };
    svc.apply_report(report());
    wait_for_screen(&svc, id, "ready");
    // Grok has no native message interface, so an owner message to it is
    // refused unsent; the owner starts its turn by typing into its terminal
    // at the desk, and the phone gets the alert.
    svc.handle_client(3, ClientMsg::Input { id, data: "hello\r".into() });
    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut worked, mut alerts) = (false, 0);
    while !(screen_of(&svc, id).contains("done") && svc.sessions[&id].tracker.status == Status::Finished) {
        assert!(Instant::now() < deadline, "never finished; worked: {worked}; screen: {}", screen_of(&svc, id));
        tick(&mut svc);
        worked |= svc.sessions[&id].tracker.status == Status::Working;
        alerts += finished_alerts(&rx);
    }
    assert!(worked, "the turn showed as working first");
    for _ in 0..10 {
        tick(&mut svc);
        alerts += finished_alerts(&rx);
    }
    assert_eq!(alerts, 0, "a screen gone quiet is no evidence the turn is done");
    // Grok records the turn's end: one alert, and none for the same record
    // read again on later cycles.
    let mut record = fs::read_to_string(&events).unwrap();
    record.push_str(&turn("2026-11-01T10:05:00.000Z", "2026-11-01T10:05:04.000Z"));
    fs::write(&events, record).unwrap();
    for _ in 0..20 {
        svc.apply_report(report());
        tick(&mut svc);
        alerts += finished_alerts(&rx);
    }
    assert_eq!(alerts, 1);
    for s in svc.sessions.values() {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn start_flags_name_the_model_in_each_programs_own_form() {
    let start = |model: &str, effort: &str| StartModel {
        model: Some(model.into()).filter(|m: &String| !m.is_empty()),
        effort: Some(effort.into()).filter(|e: &String| !e.is_empty()),
    };
    assert_eq!(model_flags(Tool::Claude, &start("opus", "high")).unwrap(), ["--model", "opus", "--effort", "high"]);
    assert_eq!(
        model_flags(Tool::Grok, &start("grok-5", "low")).unwrap(),
        ["-m", "grok-5", "--reasoning-effort", "low"]
    );
    assert_eq!(
        model_flags(Tool::OpenCode, &start("moonshot/kimi-k2", "high")).unwrap(),
        ["-m", "moonshot/kimi-k2#high"]
    );
    // Codex is never given a `-c` override: it would run without the shared
    // server's thread tools (review AR-20261027-339e742b7b).
    assert!(model_flags(Tool::Codex, &start("gpt-6-sol", "xhigh")).unwrap().is_empty());
    assert!(model_flags(Tool::Claude, &start("", "")).unwrap().is_empty());
    // A phone cannot slip in a flag of its own.
    assert!(model_flags(Tool::Claude, &start("--dangerously-skip-permissions", "")).is_err());
    assert!(model_flags(Tool::Claude, &start("opus", "high --yolo")).is_err());
    assert!(model_flags(Tool::Grok, &start("a b", "")).is_err());
}

#[test]
fn a_name_given_at_the_deck_titles_the_chat_until_a_new_one_begins() {
    let dir = TempDir::new("svc-rename");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "echo ready; read x"), tx);
    let mut s = fake_session(5, Tool::Codex);
    s.facts.first_prompt = Some("port the parser".into());
    svc.sessions.insert(5, s);
    svc.registry.sessions.push(Entry::started(5, Tool::Codex, None, "Codex".into(), 0));
    let entry = |svc: &Service| svc.registry.sessions.iter().find(|e| e.id == 5).cloned().unwrap();
    // Free text from a deck: one line, no control characters, capped.
    svc.rename(5, Some("  Ship\nthe \u{1b}parser  ")).unwrap();
    assert_eq!(svc.sessions[&5].title, "Ship the parser");
    assert_eq!((entry(&svc).name.as_deref(), entry(&svc).title.as_str()), (Some("Ship the parser"), "Ship the parser"));
    svc.rename(5, Some(&"x".repeat(500))).unwrap();
    assert_eq!(svc.sessions[&5].title.chars().count(), MAX_NAME);
    // It stays over what the chat says until a blank name gives that back.
    svc.rename(5, Some("Mine")).unwrap();
    svc.refresh_status(Instant::now());
    assert_eq!(svc.sessions[&5].title, "Mine");
    svc.rename(5, Some("   ")).unwrap();
    assert_eq!((svc.sessions[&5].title.as_str(), entry(&svc).name), ("port the parser", None));
    // A new chat drops it, in the list the next service reads too.
    svc.rename(5, None).unwrap();
    svc.rename(5, Some("Mine")).unwrap();
    svc.sessions.get_mut(&5).unwrap().start_new_chat();
    svc.refresh_status(Instant::now());
    assert_eq!((svc.sessions[&5].name.clone(), entry(&svc).name), (None, None));
    assert!(svc.rename(99, Some("x")).is_err(), "no such session");
}

#[path = "service_creation.rs"]
mod creation;

#[test]
fn outside_move_never_uses_a_guessed_conversation() {
    let dir = TempDir::new("outside-move-identity");
    let (tx, _) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let (out, rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 1, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(1, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
    while rx.try_recv().is_ok() {}
    svc.outside = vec![crate::tui::tests::outside(50, None)];
    svc.handle_client(1, ClientMsg::MoveIn { tool: Tool::Claude, pid: 50, conversation: Some("11111111-aaaa".into()) });
    assert_eq!(svc.moves_in_flight, 0, "never stop a process to resume an unverified conversation");
    let Out::Msg(reply) = rx.try_recv().unwrap() else { panic!("expected an explanation") };
    let reply = crate::protocol::read_msg::<_, ServerMsg>(&mut reply.as_slice()).unwrap().unwrap();
    assert!(matches!(reply, ServerMsg::Notice { error: true, text } if text.contains("left running")));
    assert!(svc.sessions.is_empty());
}

#[test]
fn outside_move_in_control_ends_only_that_process_and_opens_the_same_conversation() {
    struct Running(std::process::Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = TempDir::new("outside-enter");
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(options(&dir, r#"printf 'resumed %s\n' "$*""#), tx);
    let (out, replies) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 1, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(1, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
    while replies.try_recv().is_ok() {}
    let mut process = Running(
        std::process::Command::new("/bin/sh")
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let pid = process.0.id();
    let conversation = "11111111-aaaa-4bbb-8ccc-123456789abc";
    claude_transcript(&dir, conversation);
    let mut outside = crate::tui::tests::outside(pid, Some(conversation));
    outside.start_ticks = procs::stat(pid).unwrap().start_ticks;
    svc.outside = vec![outside.clone()];
    let (mut app, requests) = crate::tui::tests::app_with(vec![], vec![outside]);
    // Outside sessions stay off the live list. Exercise the retained move
    // handler explicitly rather than relying on automatic row selection.
    app.current = Some(crate::tui::Sel::Outside(pid));
    crate::tui::tests::press(&mut app, crossterm::event::KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
    assert!(app.dialog.is_none());
    svc.handle_client(1, requests.recv().unwrap());
    let deadline = Instant::now() + Duration::from_secs(15);
    let id = 'created: loop {
        while let Ok(Out::Msg(bytes)) = replies.try_recv() {
            if let Some(ServerMsg::Created { id, .. }) =
                crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap()
            {
                break 'created id;
            }
        }
        assert!(Instant::now() < deadline, "move-in never completed");
        svc.handle(events.recv_timeout(Duration::from_secs(10)).unwrap());
    };
    assert!(process.0.try_wait().unwrap().is_some());
    assert_eq!(svc.sessions[&id].conversation.as_deref(), Some(conversation));
    wait_for_screen(&svc, id, conversation);
    while svc.sessions[&id].alive {
        svc.handle(events.recv_timeout(Duration::from_secs(10)).unwrap());
    }
}

#[test]
fn agy_clear_drops_the_sidebar_title_and_rejects_the_old_monitor_report() {
    let dir = TempDir::new("svc-agy-clear");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    let mut s = fake_session(1, Tool::Antigravity);
    s.conversation = Some("old".into());
    s.spawned_ms = 50;
    s.name = Some("SampleDB enterprise reading".into());
    s.term_title = Some("Previous terminal title".into());
    s.facts.first_prompt = s.name.clone();
    svc.sessions.insert(1, s);
    let mut entry = Entry::started(1, Tool::Antigravity, Some("old".into()), "SampleDB enterprise reading".into(), 0);
    entry.name = Some("SampleDB enterprise reading".into());
    svc.registry.sessions.push(entry);
    svc.update_refs();
    let report = |cleared_ms| Report {
        facts: [(
            1,
            SessionFacts {
                known_conversation: Some("old".into()),
                conversation: Some("old".into()),
                first_prompt: Some("SampleDB enterprise reading".into()),
                cleared_ms,
                ..SessionFacts::default()
            },
        )]
        .into(),
        ..Report::default()
    };
    svc.apply_report(report(Some(200)));
    svc.refresh_status(Instant::now());
    let s = &svc.sessions[&1];
    assert_eq!(s.title, "Antigravity", "the empty AGY chat uses the program name");
    assert_eq!(s.conversation, None);
    assert_eq!(s.name, None);
    assert_eq!(s.term_title, None);
    assert_eq!(s.spawned_ms, 200);
    assert_eq!(s.left, vec!["old"]);
    assert_eq!(svc.registry.sessions[0].conversation, None, "reopen cannot resume the cleared chat");
    assert_eq!(svc.registry.sessions[0].title, "Antigravity");
    assert_eq!(svc.registry.sessions[0].name, None);
    assert_eq!(svc.hosted_refs.lock().unwrap()[0].conversation, None);
    svc.apply_report(report(None));
    svc.refresh_status(Instant::now());
    assert_eq!(svc.sessions[&1].title, "Antigravity", "an in-flight stale report cannot restore the title");
    let mut stale = report(None);
    stale.facts.get_mut(&1).unwrap().known_conversation = None;
    svc.apply_report(stale);
    svc.refresh_status(Instant::now());
    assert_eq!(
        svc.sessions[&1].title, "Antigravity",
        "a snapshot from before first discovery cannot restore a left chat"
    );
    svc.apply_report(Report {
        facts: [(
            1,
            SessionFacts {
                conversation: Some("new".into()),
                first_prompt: Some("Fix the parser".into()),
                ..SessionFacts::default()
            },
        )]
        .into(),
        ..Report::default()
    });
    svc.refresh_status(Instant::now());
    assert_eq!(svc.sessions[&1].title, "Fix the parser");
}

#[test]
fn resizing_a_shell_preserves_two_agents_sizes_and_desktop_focus() {
    let dir = TempDir::new("svc-targeted-terminal-size");
    let mut opts = options(&dir, "echo ready; while read x; do stty size; done");
    opts.config.shell_command =
        vec!["sh".into(), "-c".into(), "echo shell-ready; while read x; do stty size; done".into()];
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let (out, replies) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 1, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(1, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Tui });
    let first = svc.create(Tool::Claude, None).unwrap();
    let second = svc.create(Tool::Codex, None).unwrap();
    let shell = svc.create(Tool::Shell, None).unwrap();
    svc.handle_client(1, ClientMsg::Attach { id: Some(first), cols: 120, rows: 35 });
    wait_for_screen(&svc, first, "ready");
    wait_for_screen(&svc, second, "ready");
    wait_for_screen(&svc, shell, "shell-ready");
    let dimensions = |svc: &Service, id| {
        let h = svc.sessions[&id].hosted.as_ref().unwrap();
        (h.cols, h.rows)
    };
    let view = svc.view;
    svc.handle_client(1, ClientMsg::ResizeTerminal { id: shell, cols: 91, rows: 23 });
    assert_eq!(dimensions(&svc, shell), (91, 23));
    assert_eq!(dimensions(&svc, first), (120, 35));
    assert_eq!(dimensions(&svc, second), (120, 35));
    assert_eq!(svc.view, view);
    assert_eq!(svc.clients[&1].attached, Some(first));
    assert!(svc.clients[&1].focused);
    svc.handle_client(1, ClientMsg::Input { id: shell, data: "size\r".into() });
    wait_for_screen(&svc, shell, "23 91");
    for (id, cols, rows) in [(first, 99, 25), (shell, 1, 25), (shell, 99, 1), (99999, 99, 25)] {
        svc.handle_client(1, ClientMsg::ResizeTerminal { id, cols, rows });
    }
    assert_eq!(dimensions(&svc, first), (120, 35));
    assert_eq!(dimensions(&svc, shell), (91, 23));
    assert!(
        replies
            .try_iter()
            .filter_map(|m| match m {
                Out::Msg(bytes) => crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap(),
                _ => None,
            })
            .filter(|m| matches!(m, ServerMsg::Notice { error: true, .. }))
            .count()
            >= 4
    );
    for id in [first, second, shell] {
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn remote_shared_codex_move_in_control_reads_its_terminal_and_preserves_identity() {
    struct Running(std::process::Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = TempDir::new("kitty-move");
    let socket = dir.path().join("k");
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(options(&dir, r#"printf 'resumed %s\n' "$*""#), tx);
    let (out, replies) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 1, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(1, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
    while replies.try_recv().is_ok() {}
    let mut process = Running(
        std::process::Command::new("/bin/sh")
            .args(["-c", "read line"])
            .env("KITTY_LISTEN_ON", format!("unix:{}", socket.display()))
            .env("KITTY_WINDOW_ID", "2")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let pid = process.0.id();
    let conversation = "11111111-aaaa-4bbb-8ccc-123456789abc";
    let screen = format!(
        "• Working (esc to interrupt)\n\n› Ask Codex to do anything\n\n GPT-6.1-Sol · {conversation} · weekly 88% left\n ? for shortcuts"
    );
    let mut outside = crate::tui::tests::outside(pid, None);
    outside.tool = Tool::Codex;
    outside.cwd = "/srv/repo".into();
    dir.write(&format!("codex/sessions/2026/11/04/rollout-x-{conversation}.jsonl"),
        json!({"type":"session_meta","payload":{"id":conversation,"cwd":"/srv/repo","timestamp":"2026-11-04T00:00:00Z"}}).to_string()+"\n");
    outside.start_ticks = procs::stat(pid).unwrap().start_ticks;
    svc.outside = vec![outside.clone()];
    let remote = crate::hosts::fed::outside_from(1, "fedora", outside);
    let remote_pid = remote.pid;
    let (mut app, requests) = crate::tui::tests::app_with(vec![], vec![remote]);
    // The outside-row handler remains usable independently of live-list
    // visibility; this fixture selects that legacy entry explicitly.
    app.current = Some(crate::tui::Sel::Outside(remote_pid));
    crate::tui::tests::press(&mut app, crossterm::event::KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
    assert!(app.dialog.is_none());
    let routed = crate::hosts::fed::route(requests.recv().unwrap(), |_| Some(1));
    let crate::hosts::fed::Route::Host(1, request) = routed else { panic!("move did not reach Fedora") };
    let terminal = crate::testsupport::kitty_replies(
        &socket,
        vec![
            ("ls", crate::testsupport::kitty_windows(pid)),
            ("get-text", json!({"ok":true,"data":screen})),
            ("ls", crate::testsupport::kitty_windows(pid)),
        ],
    );
    svc.handle_client(1, request);
    terminal.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let id = 'created: loop {
        while let Ok(Out::Msg(bytes)) = replies.try_recv() {
            if let Some(ServerMsg::Created { id, .. }) =
                crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap()
            {
                break 'created id;
            }
        }
        assert!(Instant::now() < deadline, "move-in never completed");
        svc.handle(events.recv_timeout(Duration::from_secs(10)).unwrap());
    };
    assert!(process.0.try_wait().unwrap().is_some());
    assert_eq!(svc.sessions[&id].conversation.as_deref(), Some(conversation));
    wait_for_screen(&svc, id, conversation);
    while svc.sessions[&id].alive {
        svc.handle(events.recv_timeout(Duration::from_secs(10)).unwrap());
    }
}

#[test]
fn a_screen_only_new_chat_cannot_attest_that_owner_input_was_cleared() {
    let mut session = fake_session(1, Tool::Codex);
    session.owner_draft = true;
    session.chat.seen_ms = Some(util::now_ms());
    session.start_new_chat();
    assert!(session.owner_draft, "welcome-card heuristics cannot discard owner draft uncertainty");
    session.chat.asked = Some((Instant::now(), util::now_ms()));
    session.start_new_chat();
    assert!(!session.owner_draft, "a confirmed explicit clear retains the existing fast path");
}

#[test]
fn a_screen_is_read_again_only_after_its_terminal_changed() {
    let dir = TempDir::new("svc-screen-read");
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "printf 'ready\\n'; read x; printf 'got %s\\n' \"$x\"; read y"), tx);
    let id = svc.create(Tool::Claude, None).unwrap();
    wait_for_screen(&svc, id, "ready");
    let kept = |svc: &Service| {
        let read = svc.sessions[&id].screen_read.as_ref().expect("the scan read the screen");
        (read.generation, read.lines.as_ptr())
    };
    let mut at = Instant::now() + SCREEN_SCAN_INTERVAL;
    svc.refresh_status(at);
    let first = kept(&svc);
    for _ in 0..3 {
        at += SCREEN_SCAN_INTERVAL;
        svc.refresh_status(at);
        assert_eq!(kept(&svc), first, "an unchanged screen was copied and parsed again");
    }
    svc.handle_client(1, ClientMsg::Input { id, data: "x\r".into() });
    wait_for_screen(&svc, id, "got x");
    at += SCREEN_SCAN_INTERVAL;
    svc.refresh_status(at);
    let (generation, _) = kept(&svc);
    assert!(generation > first.0, "a changed terminal is read again");
    assert!(svc.sessions[&id].screen_read.as_ref().unwrap().lines.iter().any(|line| line.contains("got x")));
    for s in svc.sessions.values() {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}

#[test]
fn a_fresh_agent_starts_on_the_saved_launch_model_and_effort() {
    use crate::providers::launch;
    let dir = TempDir::new("svc-launch-settings");
    let (tx, _rx) = mpsc::channel();
    let opts = options(&dir, "exec sleep 30");
    let file = launch::path(&opts.state_dir);
    let saved = launch::Launch { model: Some("opus".into()), effort: Some("xhigh".into()) };
    launch::save(&file, &launch::Settings::from([("claude".to_string(), saved)])).unwrap();
    let mut svc = Service::new(opts, tx);
    let place = Placement { cwd: Some(dir.path().into()), ..Placement::default() };
    let start = |model: Option<&str>| StartModel { model: model.map(Into::into), effort: None };
    let fresh = svc.create_with(Tool::Claude, None, &start(None), &place).unwrap();
    let named = svc.create_with(Tool::Claude, None, &start(Some("sonnet")), &place).unwrap();
    let resumed = svc.create_with(Tool::Claude, Some("7f0c2a51-resumed".into()), &start(None), &place).unwrap();
    let started = |svc: &Service, id: u64| {
        let entry = svc.registry.sessions.iter().find(|entry| entry.id == id).unwrap();
        (entry.start_model.clone(), entry.start_effort.clone())
    };
    assert_eq!(started(&svc, fresh), (Some("opus".into()), Some("xhigh".into())));
    assert_eq!(started(&svc, named), (Some("sonnet".into()), Some("xhigh".into())));
    assert_eq!(started(&svc, resumed), (None, None), "a resumed conversation keeps its own model");

    fs::write(&file, b"not json").unwrap();
    assert!(svc.create_with(Tool::Claude, None, &start(None), &place).is_err(), "a broken file stops the start");
    for s in svc.sessions.values() {
        s.hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
}
