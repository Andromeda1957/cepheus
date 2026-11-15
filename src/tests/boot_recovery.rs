//! Cold-start acceptance checks for lost work and unwanted input.
use super::*;
use crate::testsupport::{scaled_timeout, TempDir};

fn saved(dir: &TempDir, working: &[bool]) -> ServiceOptions {
    let opts = super::tests::options(dir, "true");
    let sessions = working
        .iter()
        .enumerate()
        .map(|(i, working)| {
            let mut entry = Entry::started(
                i as u64 + 1,
                Tool::Claude,
                Some(format!("chat-{}", i + 1)),
                format!("task-{}", i + 1),
                1,
            );
            entry.working = *working;
            entry
        })
        .collect();
    Registry { next_id: working.len() as u64 + 1, sessions }.save(&opts.state_dir.join("sessions.json")).unwrap();
    opts
}

/// A Claude-like agent that records the directory it runs in and its
/// arguments (`argv-<session>`), then every key it is sent (`received-<session>`).
fn recorder(dir: &TempDir) -> Vec<String> {
    agent(
        dir,
        "claude.sh",
        r"\033[?2004h\033[?25h\033[2J\033[H────────────────────────────────────────\r\n❯ \r\n────────────────────────────────────────\r\n\033[2;3H",
    )
}

/// `screen` is a printf format the agent draws first.
fn agent(dir: &TempDir, name: &str, screen: &str) -> Vec<String> {
    let out = dir.path().display();
    let script = dir.executable(
        name,
        format!(
            "#!/bin/sh\nstty raw -echo\nprintf '{screen}'\n\
             {{ pwd -P; for arg in \"$@\"; do printf '%s\\n' \"$arg\"; done; }} > '{out}'/argv-$CEPHEUS_SESSION.part\n\
             mv '{out}'/argv-$CEPHEUS_SESSION.part '{out}'/argv-$CEPHEUS_SESSION\n\
             exec cat > '{out}'/received-$CEPHEUS_SESSION\n"
        ),
    );
    vec![script.to_string_lossy().into_owned()]
}

/// The directory and arguments session `id` was started with.
fn launched(dir: &TempDir, id: u64) -> (PathBuf, Vec<String>) {
    let path = dir.path().join(format!("argv-{id}"));
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(15));
    while !path.exists() {
        assert!(Instant::now() < deadline, "session {id} never started");
        thread::sleep(Duration::from_millis(10));
    }
    let text = fs::read_to_string(path).unwrap();
    let mut lines = text.lines().map(str::to_owned);
    (PathBuf::from(lines.next().unwrap()), lines.collect())
}

fn resumed_with(args: &[String], conversation: &str, message: &str) -> bool {
    let resumes = args
        .windows(2)
        .any(|w| matches!(w, [flag, id] if (flag == "--resume" || flag == "resume") && id == conversation));
    resumes && args.ends_with(&["--".to_owned(), message.to_owned()])
}

fn stop_children(svc: &Service) {
    for s in svc.sessions.values() {
        if let Some(h) = &s.hosted {
            h.signal(libc::SIGKILL);
        }
    }
}

#[test]
fn boot_restores_sessions_and_resumes_only_interrupted_work_before_any_tui() {
    let dir = TempDir::new("boot-headless");
    let mut opts = saved(&dir, &[true, false, true, true, false, false]);
    opts.config.claude_command = recorder(&dir);
    opts.config.recovery.launch_interval_ms = 0;
    opts.config
        .recovery
        .sessions
        .insert("4".into(), crate::recovery_policy::Override { resume_interrupted: Some(false), ..Default::default() });
    opts.config
        .recovery
        .sessions
        .insert("6".into(), crate::recovery_policy::Override { restore: Some(false), ..Default::default() });
    let mut registry = Registry::load(&opts.state_dir.join("sessions.json"));
    registry.sessions[2].owner_paused = true;
    registry.sessions[4].conversation = None;
    registry.save(&opts.state_dir.join("sessions.json")).unwrap();
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    svc.resume_after_start();
    assert!((1..=5).all(|id| svc.sessions[&id].alive), "open sessions must all return");
    assert!(!svc.sessions[&6].alive, "an explicit restoration exception must remain off");
    assert!(svc.sessions[&5].conversation.is_some(), "new empty sessions must acquire and save their native identity");
    let message = svc.opts.config.resume_message.clone();
    let (cwd, args) = launched(&dir, 1);
    assert_eq!(cwd, dir.path().canonicalize().unwrap(), "a session returns in its own directory");
    assert!(
        resumed_with(&args, "chat-1", &message),
        "interrupted work must carry on through the provider's own resume of the same conversation: {args:?}"
    );
    for id in 2..=5 {
        let (_, args) = launched(&dir, id);
        assert!(!args.contains(&message), "idle, paused and resume-off sessions must receive no task: {args:?}");
    }
    assert!(!dir.path().join("argv-6").exists());
    assert!(svc.pending_continues.is_empty(), "nothing waits to be typed into a restored terminal");
    // The service loop runs on; nothing reaches any agent's terminal.
    for _ in 0..20 {
        while let Ok(event) = events.try_recv() {
            svc.handle(event);
        }
        for session in svc.sessions.values_mut() {
            session.tracker.status = Status::Idle;
        }
        svc.continues_due(Instant::now());
        svc.submit_due(Instant::now() + SUBMIT_DELAY);
        thread::sleep(Duration::from_millis(10));
    }
    for id in 1..=5 {
        assert_eq!(fs::read(dir.path().join(format!("received-{id}"))).unwrap_or_default(), b"", "session {id}");
    }
    assert_eq!(svc.sessions[&1].conversation.as_deref(), Some("chat-1"));
    stop_children(&svc);
}

#[test]
fn boot_keeps_continuation_intent_across_a_second_crash_during_startup() {
    let dir = TempDir::new("boot-repeat-crash");
    let mut opts = saved(&dir, &[true]);
    opts.config.claude_command = recorder(&dir);
    let (tx, _events) = mpsc::channel();
    let config = opts.config.clone();
    let mut svc = Service::new(opts, tx);
    svc.resume_after_start();
    launched(&dir, 1);
    fs::remove_file(dir.path().join("argv-1")).unwrap();
    svc.track_working();
    // The turn goes to disk on the registry writer thread; wait for it.
    assert!(svc.save_registry());
    let registry = Registry::load(&svc.registry_path);
    assert!(
        registry.sessions[0].working && registry.sessions[0].boot_pending,
        "startup must retain the unfinished turn"
    );
    stop_children(&svc);
    drop(svc);
    let (tx, _events) = mpsc::channel();
    let mut opts = super::tests::options(&dir, "true");
    opts.config = config;
    let mut restored = Service::new(opts, tx);
    restored.resume_after_start();
    assert!(restored.sessions[&1].alive);
    let (_, args) = launched(&dir, 1);
    assert!(
        resumed_with(&args, "chat-1", &restored.opts.config.resume_message),
        "a second crash must still resume unfinished work: {args:?}"
    );
    stop_children(&restored);
}

#[test]
fn an_ordinary_checkpoint_is_written_on_the_registry_writer_without_waiting() {
    // A draft keystroke's checkpoint must not hold the loop that echoes keys
    // on a file and directory sync: it is handed over, and reported back.
    let dir = TempDir::new("boot-queued-checkpoint");
    let opts = saved(&dir, &[true]);
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    svc.opts.config.recovery.checkpoint_interval_ms = 0;
    svc.sessions.get_mut(&1).unwrap().recovery.draft = Some("typed so far".into());
    assert!(svc.checkpoint_recovery(false));
    assert_eq!(svc.registry_writer.in_flight, 1, "handed over, not waited for");
    assert!(!svc.registry_dirty);
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(10));
    loop {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(Ev::RegistrySaved(result)) => {
                assert!(result.is_ok(), "{result:?}");
                svc.handle(Ev::RegistrySaved(result));
                break;
            }
            Ok(_) => {}
            Err(_) => assert!(Instant::now() < deadline, "the write never reported back"),
        }
    }
    assert_eq!(svc.registry_writer.in_flight, 0);
    let registry = Registry::load(&svc.registry_path);
    assert_eq!(registry.sessions[0].recovery.as_ref().unwrap().draft.as_deref(), Some("typed so far"));
    // Nothing changed since: the next checkpoint writes nothing.
    assert!(svc.checkpoint_recovery(false));
    assert_eq!(svc.registry_writer.in_flight, 0, "an unchanged session list was written again");
    stop_children(&svc);
}

#[test]
fn boot_preserves_captured_input_receipts_attachments_and_owner_pause() {
    let dir = TempDir::new("boot-input-custody");
    let opts = saved(&dir, &[true]);
    let (tx, _events) = mpsc::channel();
    let config = opts.config.clone();
    let mut svc = Service::new(opts, tx);
    let session = svc.sessions.get_mut(&1).unwrap();
    session.tool = Tool::Codex;
    session.recovery.draft = Some("unfinished draft".into());
    session.recovery.pending = vec!["first queued request".into(), "second queued request".into()];
    session.recovery.draft_files = vec![dir.write("upload.jpg", "image").to_string_lossy().into_owned()];
    session.recovery.rollout = Some(dir.write("rollout.jsonl", ""));
    session.recovery.offset = 0;
    session.recovery.paused = true;
    session.recovery.phase = Some(recovery::Phase::Replaying);
    session.recovery.draft_restored = true;
    assert!(svc.checkpoint_recovery(true));
    let captured = svc.registry.sessions[0].recovery.clone();
    // Even a clean attempt starts the ordinary checkpoint interval. It must
    // not depend on when the registry last needed a disk write.
    svc.registry_saved = Instant::now() - Duration::from_secs(120);
    svc.opts.config.recovery.checkpoint_interval_ms = 0;
    assert!(svc.checkpoint_recovery(false));
    assert!(!svc.registry_dirty);
    svc.opts.config.recovery.checkpoint_interval_ms = 60_000;
    svc.sessions.get_mut(&1).unwrap().recovery.draft = Some("new ordinary draft".into());
    assert!(svc.checkpoint_recovery(false));
    assert_eq!(svc.registry.sessions[0].recovery, captured, "an ordinary tick must defer its snapshot");
    assert_eq!(Registry::load(&svc.registry_path).sessions[0].recovery, captured);
    assert!(svc.checkpoint_recovery(true));
    assert_eq!(
        Registry::load(&svc.registry_path).sessions[0].recovery.as_ref().unwrap().draft.as_deref(),
        Some("new ordinary draft"),
        "a forced barrier must save without waiting for the interval"
    );
    svc.sessions.get_mut(&1).unwrap().recovery.draft = Some("unfinished draft".into());
    assert!(svc.save_registry());
    let registry = Registry::load(&svc.registry_path);
    assert_eq!(registry.sessions[0].recovery.as_ref().unwrap().pending.len(), 2);
    let (tx, _events) = mpsc::channel();
    let mut opts = super::tests::options(&dir, "true");
    opts.config = config;
    let mut restored = Service::new(opts, tx);
    let recovery = &restored.sessions[&1].recovery;
    assert_eq!(recovery.draft.as_deref(), Some("unfinished draft"));
    assert_eq!(recovery.pending, vec!["first queued request", "second queued request"]);
    assert!(recovery.paused);
    assert_eq!(recovery.draft_files.len(), 1);
    assert!(recovery.rollout.is_some());
    assert!(!recovery.draft_restored, "a dead PTY cannot still own a pasted draft");
    assert_eq!(recovery.phase, None);
    restored.client_input(1, "\r").unwrap();
    assert!(
        Registry::load(&restored.registry_path).sessions[0].recovery.as_ref().unwrap().draft_submit_requested,
        "an accepted image submission must be durable immediately"
    );
    assert!(
        !Registry::load(&restored.registry_path).sessions[0].recovery.as_ref().unwrap().paused,
        "an accepted resume must be durable before returning to the owner"
    );
}

#[test]
fn boot_retries_a_transient_launch_failure_without_losing_the_request() {
    let dir = TempDir::new("boot-launch-retry");
    let mut opts = saved(&dir, &[true]);
    opts.config.claude_command = recorder(&dir);
    opts.config.recovery.retry_delay_secs = 1;
    let mut registry = Registry::load(&opts.state_dir.join("sessions.json"));
    registry.sessions[0].cwd = Some(dir.path().join("not-ready-yet").to_string_lossy().into_owned());
    registry.save(&opts.state_dir.join("sessions.json")).unwrap();
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    svc.resume_after_start();
    assert!(!svc.sessions[&1].alive);
    assert!(Registry::load(&svc.registry_path).sessions[0].resume);
    svc.handle_client(0, ClientMsg::Shutdown);
    assert!(svc.shutdown_at.is_none(), "public Stop must not discard a cold retry's recovery request");
    svc.note_for_resume(false);
    assert!(
        Registry::load(&svc.registry_path).sessions[0].working,
        "a failed launch must retain interrupted intent across a service stop"
    );
    // Resource pressure after a power loss holds the restore without using up
    // its launch attempts.
    svc.opts.config.recovery.launch_attempts = 2;
    let meminfo = svc.scheduler.settings.probe.root.clone().unwrap().join("meminfo");
    fs::write(&meminfo, "MemTotal: 16777216 kB\nMemAvailable: 1024 kB\n").unwrap();
    for round in 1..=4 {
        svc.restores_due(Instant::now() + Duration::from_secs(2 * round));
    }
    assert!(!svc.sessions[&1].alive);
    fs::write(&meminfo, "MemTotal: 16777216 kB\nMemAvailable: 12582912 kB\n").unwrap();
    fs::create_dir(dir.path().join("not-ready-yet")).unwrap();
    svc.restores_due(Instant::now() + Duration::from_secs(20));
    assert!(svc.sessions[&1].alive);
    let (cwd, args) = launched(&dir, 1);
    assert_eq!(cwd, dir.path().join("not-ready-yet").canonicalize().unwrap(), "never another directory");
    assert!(resumed_with(&args, "chat-1", &svc.opts.config.resume_message), "{args:?}");
    stop_children(&svc);
}

#[test]
fn boot_codex_continuation_never_submits_an_owners_unsent_draft_or_replays_itself() {
    let dir = TempDir::new("boot-codex-draft");
    let mut opts = saved(&dir, &[true]);
    opts.config.codex_command = agent(
        &dir,
        "codex.sh",
        r"\033[?2004h\033[?25h\033[2J\033[H\r\n› \033[2mAsk Codex to do anything\033[0m\r\n\r\n  GPT-6.1-Sol xhigh · Test conversation\033[2;3H",
    );
    opts.config.resume_message = "continue".into();
    opts.config.recovery.client_settle_ms = 0;
    opts.config.recovery.settle_ms = 0;
    let mut registry = Registry::load(&opts.state_dir.join("sessions.json"));
    registry.sessions[0].tool = Tool::Codex;
    registry.sessions[0].recovery =
        Some(recovery::ClientRecovery { draft: Some("owner has not submitted this".into()), ..Default::default() });
    registry.save(&opts.state_dir.join("sessions.json")).unwrap();
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    svc.resume_after_start();
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(15));
    let received = dir.path().join("received-1");
    loop {
        while let Ok(event) = events.try_recv() {
            svc.handle(event);
        }
        svc.sessions.get_mut(&1).unwrap().tracker.status = Status::Idle;
        svc.recover_clients(util::now_ms());
        svc.continues_due(Instant::now());
        assert_eq!(
            svc.sessions[&1].recovery.draft.as_deref(),
            Some("owner has not submitted this"),
            "a generated continuation must retain the saved owner draft"
        );
        svc.submit_due(Instant::now() + SUBMIT_DELAY);
        if fs::read_to_string(&received).is_ok_and(|s| s.contains("owner has not submitted this")) {
            break;
        }
        assert!(Instant::now() < deadline, "recovered draft was never restored");
        thread::sleep(Duration::from_millis(10));
    }
    let keys = fs::read_to_string(received).unwrap();
    let (_, args) = launched(&dir, 1);
    assert!(resumed_with(&args, "chat-1", "continue"), "the continuation goes through `codex resume`: {args:?}");
    assert!(!keys.contains("continue"), "a generated continuation is never typed");
    assert_eq!(keys.matches("owner has not submitted this").count(), 1);
    assert_eq!(keys.matches('\r').count(), 0, "the owner's draft must remain unsubmitted");
    assert!(svc.sessions[&1].recovery.replay.is_empty());
    assert!(!svc.sessions[&1].recovery.pending.iter().any(|p| p.contains("owner has not submitted this")));
    stop_children(&svc);
}

#[test]
fn boot_recovery_withholds_owner_submission_when_the_durable_barrier_fails() {
    let dir = TempDir::new("boot-disk-failure");
    let (mut svc, id) = super::tests::key_recorder_service(&dir, Tool::Codex, false);
    // Capture the known composer before the recorder acknowledgement moves its cursor.
    let draft = {
        let guard = svc.sessions[&id].hosted.as_ref().unwrap().lock();
        crate::providers::recovery::codex_recovery_draft(&guard.term)
    };
    assert_eq!(draft, Some(String::new()));
    svc.sessions.get_mut(&id).unwrap().recovery.draft = draft;
    svc.pending_continues.push(resume::PendingContinue { id, since: Instant::now(), ready_since: None });
    fs::remove_file(&svc.registry_path).unwrap();
    fs::create_dir(&svc.registry_path).unwrap();
    let result = svc.client_input(id, "run this task\r");
    assert!(result.is_err(), "failed durability must not acknowledge a submitted task");
    assert!(svc.sessions[&id].recovery.pending.is_empty(), "rejected input must not become a replay request");
    assert!(
        svc.pending_continues.iter().any(|pending| pending.id == id),
        "a rejected send must retain interrupted work"
    );
    assert_eq!(super::tests::recorded_keys(&svc, &dir, id, 1), b"!", "no rejected input may reach the agent");
    fs::remove_dir(&svc.registry_path).unwrap();
    svc.client_input(id, "run this task\r").unwrap();
    assert_eq!(svc.sessions[&id].recovery.pending, vec!["run this task"], "retry must accept exactly one request");
    assert!(svc.pending_continues.is_empty());
    let after = super::tests::recorded_keys(&svc, &dir, id, 2);
    assert_eq!(String::from_utf8_lossy(&after).matches("run this task").count(), 1);
    assert_eq!(after.iter().filter(|byte| **byte == b'\r').count(), 1);
    stop_children(&svc);
}

#[test]
fn boot_image_caption_rejection_can_be_retried_without_loss_or_duplication() {
    let dir = TempDir::new("boot-image-disk-failure");
    let (mut svc, id) = super::tests::key_recorder_service(&dir, Tool::Codex, false);
    {
        let r = &mut svc.sessions.get_mut(&id).unwrap().recovery;
        r.draft = Some(String::new());
        r.draft_files = vec![dir.write("upload.jpg", "image").to_string_lossy().into_owned()];
        r.draft_restored = true;
    }
    assert!(svc.checkpoint_recovery(true));
    fs::remove_file(&svc.registry_path).unwrap();
    fs::create_dir(&svc.registry_path).unwrap();
    assert!(svc.client_input(id, "caption\r").is_err());
    assert_eq!(
        svc.sessions[&id].recovery.draft.as_deref(),
        Some(""),
        "rejected caption must leave saved input unchanged"
    );
    let before = super::tests::recorded_keys(&svc, &dir, id, 1);
    assert!(!String::from_utf8_lossy(&before).contains("caption"));
    fs::remove_dir(&svc.registry_path).unwrap();
    svc.client_input(id, "caption\r").unwrap();
    assert_eq!(
        svc.sessions[&id].recovery.draft.as_deref(),
        Some("caption"),
        "retry must save the caption exactly once"
    );
    svc.recover_clients(util::now_ms());
    svc.submit_due(Instant::now() + SUBMIT_DELAY);
    let after = super::tests::recorded_keys(&svc, &dir, id, 2);
    assert_eq!(String::from_utf8_lossy(&after).matches("caption").count(), 1);
    assert_eq!(after.iter().filter(|b| **b == b'\r').count(), 1);
    stop_children(&svc);
}

#[test]
fn boot_holds_saved_codex_input_when_draft_restoration_is_disabled() {
    let dir = TempDir::new("boot-draft-restoration-disabled");
    let mut opts = saved(&dir, &[true]);
    opts.config.recovery.restore_drafts = false;
    let mut registry = Registry::load(&opts.state_dir.join("sessions.json"));
    registry.sessions[0].tool = Tool::Codex;
    registry.sessions[0].recovery = Some(recovery::ClientRecovery {
        draft: Some("owner has not submitted this".into()),
        pending: vec!["accepted request still awaiting delivery".into()],
        ..Default::default()
    });
    registry.save(&opts.state_dir.join("sessions.json")).unwrap();
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    svc.resume_after_start();
    let recovery = &svc.sessions[&1].recovery;
    assert!(svc.sessions[&1].alive);
    assert!(recovery.paused, "disabled draft restoration must hold all captured input");
    assert_eq!(recovery.draft.as_deref(), Some("owner has not submitted this"));
    assert_eq!(recovery.pending, vec!["accepted request still awaiting delivery"]);
    assert!(!recovery.draft_restored);
    assert!(svc.pending_continues.is_empty(), "held input must not receive automatic continuation");
    let saved = Registry::load(&svc.registry_path).sessions.remove(0).recovery.unwrap();
    assert!(saved.paused, "the hold must survive another abrupt shutdown");
    stop_children(&svc);
}

#[test]
fn boot_paused_input_reconciles_receipts_before_building_replay() {
    let dir = TempDir::new("boot-paused-receipts");
    let (mut svc, id) = super::tests::key_recorder_service(&dir, Tool::Codex, false);
    let receipt = serde_json::json!({"type":"event_msg","payload":{"type":"item_completed","item":{
        "type":"UserMessage","content":[{"type":"text","text":"already accepted"}]
    }}});
    let metadata = "{\"type\":\"session_meta\",\"payload\":{\"cli_version\":\"0.166.0\"}}\n";
    let rollout = dir.write("rollout.jsonl", metadata);
    svc.opts.config.recovery.client_settle_ms = 0;
    svc.sessions.get_mut(&id).unwrap().recovery = recovery::ClientRecovery {
        paused: true,
        phase: Some(recovery::Phase::Starting),
        phase_at_ms: util::now_ms(),
        pending: vec!["already accepted".into()],
        rollout: Some(rollout.clone()),
        captured: true,
        ..Default::default()
    };
    svc.recover_clients(util::now_ms());
    assert!(svc.sessions[&id].recovery.replay.is_empty(), "held input must wait for fresh receipts at release");
    fs::write(&rollout, format!("{metadata}{receipt}\n")).unwrap();
    svc.recover_clients(util::now_ms() + 1_000_000);
    assert_eq!(svc.sessions[&id].recovery.phase, Some(recovery::Phase::Starting), "an owner hold must not time out");
    svc.client_input(id, "\r").unwrap();
    svc.recover_clients(util::now_ms());
    assert!(svc.sessions[&id].recovery.pending.is_empty(), "receipt must retire accepted input before release");
    assert!(svc.sessions[&id].recovery.replay.is_empty(), "accepted input must never enter replay");
    assert!(svc.pending_submits.iter().all(|submit| submit.id != id));
    stop_children(&svc);
}

#[test]
fn boot_holds_interrupted_providers_without_a_resumable_conversation() {
    let dir = TempDir::new("boot-stateless-provider");
    let mut opts = saved(&dir, &[true]);
    opts.config.providers = vec![serde_json::from_value(serde_json::json!({
        "id":"boot-stateless-fixture", "label":"Stateless", "command":["cat"]
    }))
    .unwrap()];
    let mut registry = Registry::load(&opts.state_dir.join("sessions.json"));
    registry.sessions[0].tool = Tool::Dynamic("boot-stateless-fixture");
    registry.sessions[0].conversation = None;
    registry.save(&opts.state_dir.join("sessions.json")).unwrap();
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    svc.resume_after_start();
    let restarted = svc.sessions[&1].alive;
    stop_children(&svc);
    assert!(!restarted, "interrupted work without an identity must not restart in a fresh conversation");
}

#[test]
fn boot_resumes_an_interrupted_codex_turn_through_codex_resume_in_its_directory() {
    let dir = TempDir::new("boot-codex-native");
    let mut opts = saved(&dir, &[true, false]);
    opts.config.codex_command = agent(&dir, "codex.sh", r"\033[?2004h\033[2J\033[H\r\n› \r\n");
    opts.config.recovery.launch_interval_ms = 0;
    let mut registry = Registry::load(&opts.state_dir.join("sessions.json"));
    for entry in &mut registry.sessions {
        entry.tool = Tool::Codex;
    }
    registry.save(&opts.state_dir.join("sessions.json")).unwrap();
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    svc.resume_after_start();
    let message = svc.opts.config.resume_message.clone();
    let (cwd, args) = launched(&dir, 1);
    assert!(resumed_with(&args, "chat-1", &message), "{args:?}");
    assert_eq!(cwd, dir.path().canonicalize().unwrap());
    assert!(args.windows(2).any(|w| w[0] == "--cd" && Path::new(&w[1]) == dir.path()), "{args:?}");
    let (_, idle) = launched(&dir, 2);
    assert!(!idle.contains(&message), "an idle Codex session only reopens: {idle:?}");
    assert!(svc.pending_continues.is_empty());
    assert_eq!(svc.sessions[&1].recovery.phase, None, "no composer wait or replay without saved input");
    let saved = Registry::load(&svc.registry_path);
    assert!(saved.sessions[0].boot_pending, "the intent stays saved until the turn is seen running");
    stop_children(&svc);
}

#[test]
fn boot_restores_each_session_in_its_recorded_directory_after_the_start_directory_changes() {
    let dir = TempDir::new("boot-exact-directory");
    let first = dir.path().join("first start");
    let second = dir.path().join("second start");
    let elsewhere = dir.path().join("chosen directory");
    for path in [&first, &second, &elsewhere] {
        fs::create_dir(path).unwrap();
    }
    let mut opts = saved(&dir, &[true, false]);
    opts.config.claude_command = recorder(&dir);
    opts.config.recovery.launch_interval_ms = 0;
    opts.start_dir = first.clone();
    let config = opts.config.clone();
    // An older list: no directory recorded for a session in the start directory.
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    assert_eq!(svc.registry.sessions[0].cwd.as_deref(), Some(first.to_str().unwrap()));
    // A session made now records its directory even when it is the start one.
    let made = svc.create(Tool::Claude, None).unwrap();
    let elsewhere_id = svc
        .create_with(
            Tool::Claude,
            None,
            &StartModel::default(),
            &Placement { cwd: Some(elsewhere.clone()), ..Placement::default() },
        )
        .unwrap();
    assert_eq!(svc.registry.sessions.iter().find(|e| e.id == made).unwrap().cwd.as_deref(), first.to_str());
    for id in [made, elsewhere_id] {
        launched(&dir, id);
    }
    stop_children(&svc);
    drop(svc);
    for entry in fs::read_dir(dir.path()).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("argv-") {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    // Power is lost; the owner then changes the start directory.
    let (tx, _events) = mpsc::channel();
    let mut opts = super::tests::options(&dir, "true");
    opts.config = config;
    opts.start_dir = second.clone();
    let mut restored = Service::new(opts, tx);
    restored.resume_after_start();
    for (id, expected) in [(1, &first), (2, &first), (made, &first), (elsewhere_id, &elsewhere)] {
        let (cwd, _) = launched(&dir, id);
        assert_eq!(&cwd, &expected.canonicalize().unwrap(), "session {id} must return in its own directory");
    }
    stop_children(&restored);
}

#[test]
fn boot_after_power_loss_mid_write_restores_every_session_from_the_last_durable_list() {
    let dir = TempDir::new("boot-power-loss");
    let mut opts = saved(&dir, &[true, false, true]);
    opts.config.claude_command = recorder(&dir);
    opts.config.recovery.launch_interval_ms = 0;
    let state = opts.state_dir.clone();
    let registry_path = state.join("sessions.json");
    // The last durable list was written by a running service: no session ended.
    let mut registry = Registry::load(&registry_path);
    registry.sessions[2].owner_paused = true;
    registry.save(&registry_path).unwrap();
    let durable = fs::read(&registry_path).unwrap();
    // Power went off while the next list was being written: its temporary
    // file holds a prefix and was never renamed over the list.
    let mut next = registry.clone();
    next.sessions.clear();
    let partial = serde_json::to_vec_pretty(&next).unwrap();
    fs::write(state.join(".sessions.json.4242.tmp"), &partial[..partial.len() / 2]).unwrap();
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    svc.resume_after_start();
    assert!((1..=3).all(|id| svc.sessions[&id].alive), "every open session returns after a power loss");
    let message = svc.opts.config.resume_message.clone();
    assert!(resumed_with(&launched(&dir, 1).1, "chat-1", &message), "the interrupted turn carries on");
    for id in [2, 3] {
        assert!(!launched(&dir, id).1.contains(&message), "idle and owner-paused sessions only reopen");
    }
    let after = Registry::load(&registry_path);
    assert_eq!(after.sessions.len(), 3, "the unfinished write never replaced the durable list");
    assert!(after.sessions.iter().all(|e| e.cwd.is_some()));
    assert_ne!(fs::read(&registry_path).unwrap(), durable, "the restored list is saved again");
    assert!(
        fs::read_dir(&state)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with("sessions.corrupt")),
        "an interrupted write never makes the list look corrupt"
    );
    stop_children(&svc);
}

#[test]
fn boot_recovery_and_turn_resumption_are_each_configurable() {
    for (resume_sessions, resume_interrupted) in [(false, true), (true, false)] {
        let dir = TempDir::new("boot-configurable");
        let mut opts = saved(&dir, &[true]);
        opts.config.claude_command = recorder(&dir);
        opts.config.resume_sessions = resume_sessions;
        opts.config.recovery.resume_interrupted = resume_interrupted;
        let (tx, _events) = mpsc::channel();
        let mut svc = Service::new(opts, tx);
        svc.resume_after_start();
        if !resume_sessions {
            assert!(!svc.sessions[&1].alive, "recovery off leaves sessions closed until the owner reopens them");
            continue;
        }
        let (_, args) = launched(&dir, 1);
        assert!(!args.contains(&svc.opts.config.resume_message), "turn resumption off only reopens: {args:?}");
        assert!(svc.pending_continues.is_empty());
        stop_children(&svc);
    }
}
