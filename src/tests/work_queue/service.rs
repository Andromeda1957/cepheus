use super::*;
use crate::service::tests::options;
use crate::testsupport::TempDir;
fn service(d: &TempDir) -> Service {
    let (tx, _rx) = std::sync::mpsc::channel();
    Service::new(options(d, "true"), tx)
}
fn session_spec(d: &TempDir, key: &str) -> w::Spec {
    serde_json::from_value(serde_json::json!({"key":key,"title":"fake task","kind":"session","cwd":d.path(),"provider":"codex","placement":"local","prompt":"PRIVATE_TASK"})).unwrap()
}
fn chore(d: &TempDir) -> Value {
    serde_json::json!({"id":"daily","owner":"owner","repository":"repo","title":"daily","target":"job","enabled":true,"schedule":{"cron":"0 9 * * *","timezone":"UTC"},"spec":{"key":"template","title":"daily","kind":"job","cwd":d.path(),"placement":"local","argv":["true"]}})
}
#[test]
fn existing_interactive_receipt_reconciles_before_new_quota_admission_and_stagger() {
    let d = TempDir::new("queue-interactive-replay");
    let mut svc = service(&d);
    let spec = session_spec(&d, "same-key");
    let fingerprint = crate::remote::auth::sha256_hex(&serde_json::to_vec(&spec).unwrap());
    let mut entry = Entry::started(33, Tool::Codex, None, "previous".into(), 1);
    entry.launch_key = Some(spec.key.clone());
    entry.launch_fingerprint = Some(fingerprint);
    svc.registry.sessions.push(entry);
    svc.scheduler.error = Some("injected unavailable admission".into());
    svc.scheduler.last_started.insert("codex".into(), u64::MAX);
    let before = svc.scheduler.last_started.clone();
    let reply = svc.work_interactive_start(&serde_json::to_value(&spec).unwrap()).unwrap();
    assert_eq!(reply["session"], 33);
    assert_eq!(svc.scheduler.last_started, before);
    assert!(svc.sessions.is_empty());
    let mut changed = spec;
    changed.prompt = Some("different payload".into());
    assert!(svc.work_interactive_start(&serde_json::to_value(changed).unwrap()).is_err());

    let tool = Tool::Dynamic("fixture-cloud");
    svc.opts.config.providers = vec![serde_json::from_value(serde_json::json!({
        "id":"fixture-cloud","label":"Cloud","command":["true"],"cloud":true
    }))
    .unwrap()];
    let mut cloud = session_spec(&d, "cloud-receipt");
    cloud.provider = Some(tool);
    let fingerprint = crate::remote::auth::sha256_hex(&serde_json::to_vec(&cloud).unwrap());
    let mut entry = Entry::started(34, tool, None, "prior cloud receipt".into(), 1);
    entry.launch_key = Some(cloud.key.clone());
    entry.launch_fingerprint = Some(fingerprint.clone());
    svc.registry.sessions.push(entry);
    assert_eq!(svc.work_interactive_start(&serde_json::to_value(&cloud).unwrap()).unwrap()["session"], 34);
    let place =
        Placement { launch_key: Some(cloud.key.clone()), launch_fingerprint: Some(fingerprint), ..Default::default() };
    assert_eq!(svc.create_with(tool, None, &StartModel::default(), &place).unwrap(), 34);
    let item = svc.work_queue.as_mut().unwrap().add_batch(vec![cloud.clone()], 1).unwrap()[0].id;
    let record = svc.work_queue.as_mut().unwrap().records.get_mut(&item).unwrap();
    record.dispatch_key = cloud.key.clone();
    record.executor = Some("local".into());
    svc.work_dispatch(item, 2).unwrap();
    let record = &svc.work_queue.as_ref().unwrap().records[&item];
    assert_eq!(record.info.dispatch.as_ref().unwrap().id, 34);
    assert_eq!(record.turn_anchor, Some(0), "real launch reconciliation preserves its completion fence");
    cloud.prompt = Some("changed cloud payload".into());
    assert!(svc.work_interactive_start(&serde_json::to_value(cloud).unwrap()).is_err());
    assert!(svc.sessions.is_empty(), "genuine cloud receipts reconcile without a new bridge");
}
#[test]
fn pending_reserved_interactive_identity_never_blindly_respawns() {
    let d = TempDir::new("queue-interactive-unknown");
    let mut svc = service(&d);
    let spec = session_spec(&d, "reserved");
    let mut entry = Entry::started(44, Tool::Codex, None, "reserved".into(), 1);
    entry.launch_key = Some(spec.key.clone());
    entry.launch_pending = true;
    entry.launch_fingerprint = Some(crate::remote::auth::sha256_hex(&serde_json::to_vec(&spec).unwrap()));
    svc.registry.sessions.push(entry);
    let error = svc.work_interactive_start(&serde_json::to_value(spec).unwrap()).unwrap_err();
    assert!(error.to_string().contains("unknown"));
    assert!(svc.sessions.is_empty());
    assert_eq!(svc.registry.sessions.len(), 1);
}
#[test]
fn failed_pre_spawn_registry_barrier_starts_no_pty() {
    let d = TempDir::new("queue-pre-spawn-barrier");
    let mut svc = service(&d);
    svc.registry_path = d.path().join("missing-parent").join("registry.json");
    let place = Placement {
        launch_key: Some("reserved".into()),
        launch_fingerprint: Some("fingerprint".into()),
        cwd: Some(d.path().to_path_buf()),
        ..Default::default()
    };
    assert!(svc.create_with(Tool::Codex, None, &StartModel::default(), &place).is_err());
    assert!(svc.sessions.is_empty());
    assert!(svc.registry.sessions.is_empty());

    for alive in [false, true] {
        let d = TempDir::new("queue-cloud-start-unsupported");
        let mut svc = service(&d);
        let tool = Tool::Dynamic("fixture-cloud");
        svc.opts.config.providers = vec![serde_json::from_value(serde_json::json!({
            "id":"fixture-cloud","label":"Cloud","command":["true"],"cloud":true
        }))
        .unwrap()];
        if alive {
            let mut bridge = crate::service::tests::fake_session(7, tool);
            bridge.tracker.completed_turns = 5;
            bridge.tracker.status = Status::Finished;
            svc.sessions.insert(7, bridge);
        }
        let place = Placement {
            launch_key: Some("new-cloud-launch".into()),
            launch_fingerprint: Some("new-payload".into()),
            prompt: Some(crate::keys::non_owner_message("PRIVATE_TASK")),
            ..Default::default()
        };
        assert!(svc
            .create_with(tool, None, &StartModel::default(), &place)
            .unwrap_err()
            .to_string()
            .contains("Cloud providers do not support"));
        let mut spec = session_spec(&d, "new-cloud-peer");
        spec.provider = Some(tool);
        assert!(svc
            .work_interactive_start(&serde_json::to_value(&spec).unwrap())
            .unwrap_err()
            .to_string()
            .contains("Cloud providers do not support"));
        for placement in ["local"] {
            spec.key = format!("new-cloud-{placement}");
            spec.placement = placement.into();
            let id = svc.work_queue.as_mut().unwrap().add_batch(vec![spec.clone()], 1).unwrap()[0].id;
            svc.work_dispatch(id, 2).unwrap();
            svc.work_observe_dispatches(3);
            let record = &svc.work_queue.as_ref().unwrap().records[&id];
            assert_eq!(record.info.state, "held");
            assert!(record.info.reason.contains("Cloud providers do not support"));
            assert!(!record.attempted && record.info.dispatch.is_none() && record.info.cancelable);
            assert!(record.executor.is_none() && record.turn_anchor.is_none(), "no placement or counter binding");
        }
        assert!(svc.registry.sessions.is_empty(), "new cloud keys never reserve a native session");
        assert_eq!(svc.registry.next_id, 1);
        assert!(svc.scheduler.last_started.is_empty());
        assert!(svc.placement_reserved.borrow().is_empty());
        assert_eq!(svc.sessions.len(), usize::from(alive));
        if alive {
            assert_eq!(svc.sessions[&7].inputs, 0);
            assert_eq!(svc.sessions[&7].tracker.completed_turns, 5);
            assert!(svc.sessions[&7].hosted.is_none());
            assert_eq!(svc.create_with(tool, None, &StartModel::default(), &Placement::default()).unwrap(), 7);
        }
    }
}
#[test]
fn conditional_stop_holds_supervisor_gate_before_owner_publication() {
    use std::os::fd::AsRawFd;
    let d = TempDir::new("queue-conditional-cancel-gate");
    let mut svc = service(&d);
    let spec =
        crate::job_sessions::Spec { argv: vec!["true".into()], cwd: d.path().to_path_buf(), ..Default::default() };
    let job = svc.job_sessions.as_mut().unwrap().add("gate".into(), "gate".into(), spec, "local".into(), None).unwrap();
    let path = svc.job_sessions.as_ref().unwrap().dir(job.id);
    let gate = crate::job_sessions::store::append_log(&path.join("run.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    assert!(!path.join("owner.json").exists());
    let reply = svc.job_command("job-stop", &serde_json::json!({"key":"gate","if_unstarted":true})).unwrap();
    assert_eq!(reply["cancelled"], false);
    assert!(!path.join("stop.json").exists(), "a gate-owned launch must never be killed by stale starting metadata");
    assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_UN) }, 0);
    drop(gate);
    let reply = svc.job_command("job-stop", &serde_json::json!({"key":"gate","if_unstarted":true})).unwrap();
    assert_eq!(reply["cancelled"], true);
    assert!(path.join("exit.json").exists());
}
#[test]
fn manual_daily_run_and_automatic_tick_share_identity_without_unchanged_writes() {
    let d = TempDir::new("queue-daily-manual");
    let mut svc = service(&d);
    svc.work_command("chore-register", &chore(&d)).unwrap();
    svc.work_command("queue-enable", &serde_json::json!({"enabled":true})).unwrap();
    let now = 1_793_952_000;
    let cal = w::schedule::calendar(now, "UTC").unwrap();
    assert_eq!(cal.hour, 8);
    let first = svc.run_chore("daily", "manual", now).unwrap();
    let path = svc.work_queue.as_ref().unwrap().chore_path("daily");
    let stamp = std::fs::metadata(path).unwrap().modified().unwrap();
    svc.work_tick_at(now + 3600);
    assert_eq!(svc.work_queue.as_ref().unwrap().records.len(), 1);
    assert_eq!(svc.work_queue.as_ref().unwrap().chores["daily"].claims.len(), 1);
    assert_eq!(svc.work_queue.as_ref().unwrap().chores["daily"].claims.values().next().unwrap().item, Some(first.id));
    assert_eq!(
        std::fs::metadata(svc.work_queue.as_ref().unwrap().chore_path("daily")).unwrap().modified().unwrap(),
        stamp
    );
}

#[test]
fn idle_chore_completion_requires_a_new_turn_even_with_same_display_second() {
    let d = TempDir::new("queue-idle-turn-fence");
    let mut svc = service(&d);
    let spec = session_spec(&d, "idle-turn");
    let q = svc.work_queue.as_mut().unwrap();
    let id = q.add_batch(vec![spec], 1).unwrap()[0].id;
    q.records.get_mut(&id).unwrap().turn_anchor = Some(1);
    q.transition(id, "dispatched", "idle", 1, Some(w::Dispatch { kind: "session".into(), id: 7 })).unwrap();
    let mut session = crate::service::tests::fake_session(7, Tool::Codex);
    session.tracker.status = Status::Finished;
    session.tracker.since = 123;
    session.tracker.completed_turns = 1;
    svc.sessions.insert(7, session);
    svc.work_observe_dispatches(2);
    assert_eq!(svc.work_queue.as_ref().unwrap().records[&id].info.state, "dispatched");
    svc.sessions.get_mut(&7).unwrap().tracker.completed_turns = 2;
    svc.work_observe_dispatches(3);
    assert_eq!(svc.work_queue.as_ref().unwrap().records[&id].info.state, "done");
    assert_eq!(svc.sessions[&7].tracker.since, 123);
}

#[test]
fn stale_chore_requires_known_job_receipt_and_holds_missing_or_unknown_custody() {
    let d = TempDir::new("queue-unknown-chore");
    let mut svc = service(&d);
    let spec: w::Spec = serde_json::from_value(
        serde_json::json!({"key":"old","title":"old","kind":"job","cwd":d.path(),"argv":["true"]}),
    )
    .unwrap();
    let q = svc.work_queue.as_mut().unwrap();
    let item = q.add_batch(vec![spec], 1).unwrap()[0].id;
    q.begin_attempt(item).unwrap();
    let jobs = svc.job_sessions.as_mut().unwrap();
    let job = jobs
        .add(
            "old-job".into(),
            "old-job".into(),
            crate::job_sessions::Spec { argv: vec!["true".into()], cwd: d.path().to_path_buf(), ..Default::default() },
            "local".into(),
            None,
        )
        .unwrap();
    let mut dead = crate::job_sessions::Identity::read(std::process::id()).unwrap();
    dead.start += 1;
    let nonce = jobs.records[&job.id].intent.nonce.clone();
    let dir = jobs.dir(job.id);
    crate::job_sessions::store::write(
        &dir.join("launch.json"),
        &crate::job_sessions::Launch {
            nonce: nonce.clone(),
            supervisor: dead.clone(),
            child: dead.clone(),
            pgid: dead.pid,
            started_at: 1,
        },
    )
    .unwrap();
    jobs.refresh().unwrap();
    assert_eq!(jobs.records[&job.id].info.status, "failed");
    assert!(jobs.records[&job.id].info.exit_code.is_none());
    svc.work_queue
        .as_mut()
        .unwrap()
        .transition(
            item,
            "failed",
            "job failed or exit unknown",
            2,
            Some(w::Dispatch { kind: "job".into(), id: job.id }),
        )
        .unwrap();
    svc.work_command("chore-register", &chore(&d)).unwrap();
    let mut claim = svc.claim_chore("daily", "first", "session", 100).unwrap();
    claim.item = Some(item);
    assert!(svc.quiesce_prior_chore(&claim).unwrap_err().to_string().contains("unknown"));
    assert!(!dir.join("stop.json").exists());
    crate::job_sessions::store::write(
        &dir.join("exit.json"),
        &crate::job_sessions::Exit {
            nonce,
            code: Some(7),
            signal: None,
            finished_at: 200,
            stopped: false,
            error: None,
        },
    )
    .unwrap();
    svc.job_sessions.as_mut().unwrap().refresh().unwrap();
    assert!(svc.quiesce_prior_chore(&claim).is_ok());
    svc.job_sessions.as_mut().unwrap().records.remove(&job.id);
    assert!(svc.quiesce_prior_chore(&claim).is_err());
}
#[test]
fn stale_interactive_chore_holds_running_question_offline_and_unfinished_idle_turn() {
    let d = TempDir::new("queue-session-chore-custody");
    let mut svc = service(&d);
    let item = svc.work_queue.as_mut().unwrap().add_batch(vec![session_spec(&d, "session")], 1).unwrap()[0].id;
    svc.work_queue
        .as_mut()
        .unwrap()
        .transition(item, "dispatched", "session", 1, Some(w::Dispatch { kind: "session".into(), id: 7 }))
        .unwrap();
    svc.work_command("chore-register", &chore(&d)).unwrap();
    let mut claim = svc.claim_chore("daily", "first", "claimant", 100).unwrap();
    claim.item = Some(item);
    assert!(svc.quiesce_prior_chore(&claim).is_err(), "missing remote/local handle is not a finish");
    let session = crate::service::tests::fake_session(7, Tool::Codex);
    svc.sessions.insert(7, session);
    for status in [Status::Starting, Status::Working, Status::NeedsYou, Status::Idle, Status::Ended] {
        svc.sessions.get_mut(&7).unwrap().tracker.status = status;
        assert!(svc.quiesce_prior_chore(&claim).is_err());
    }
    let s = svc.sessions.get_mut(&7).unwrap();
    s.tracker.status = Status::Finished;
    s.tracker.completed_turns = 3;
    svc.work_queue.as_mut().unwrap().records.get_mut(&item).unwrap().turn_anchor = Some(3);
    assert!(svc.quiesce_prior_chore(&claim).is_err());
    svc.sessions.get_mut(&7).unwrap().tracker.completed_turns = 4;
    assert!(svc.quiesce_prior_chore(&claim).is_ok());
    svc.sessions.get_mut(&7).unwrap().alive = false;
    assert!(svc.quiesce_prior_chore(&claim).is_err());
}

#[test]
fn completed_turn_receipt_survives_owner_clear_next_turn_and_session_end() {
    for status in [Status::Idle, Status::Working, Status::Ended] {
        let d = TempDir::new("queue-completed-turn-receipt");
        let mut svc = service(&d);
        let id = svc.work_queue.as_mut().unwrap().add_batch(vec![session_spec(&d, "turn")], 1).unwrap()[0].id;
        svc.work_queue.as_mut().unwrap().records.get_mut(&id).unwrap().turn_anchor = Some(0);
        svc.work_queue
            .as_mut()
            .unwrap()
            .transition(id, "dispatched", "started", 1, Some(w::Dispatch { kind: "session".into(), id: 7 }))
            .unwrap();
        let mut session = crate::service::tests::fake_session(7, Tool::Codex);
        session.tracker.status = status;
        session.tracker.completed_turns = 1;
        session.alive = status != Status::Ended;
        svc.sessions.insert(7, session);
        svc.work_observe_dispatches(2);
        assert_eq!(svc.work_queue.as_ref().unwrap().records[&id].info.state, "done");
        assert_eq!(svc.sessions[&7].tracker.status, status, "completion must not touch the owner's next turn");
    }
}

#[test]
fn queue_pause_gate_is_durable_monotonic_and_atomic_before_owner_publication() {
    use std::os::fd::AsRawFd;
    let d = TempDir::new("queue-policy-gate");
    let mut svc = service(&d);
    let spec=serde_json::from_value(serde_json::json!({"key":"gate","title":"gate","kind":"job","cwd":d.path(),"placement":"local","argv":["true"]})).unwrap();
    let item = svc.work_queue.as_mut().unwrap().add_batch(vec![spec], 1).unwrap()[0].id;
    svc.work_command("queue-enable", &serde_json::json!({"enabled":true})).unwrap();
    svc.work_dispatch(item, 1).unwrap();
    let jobs = svc.job_sessions.as_ref().unwrap();
    let job = jobs.records.values().next().unwrap().clone();
    let dir = jobs.dir(job.info.id);
    let key = job.intent.key.clone();
    let binding = job.intent.gate_binding.clone().unwrap();
    svc.sync_work_job_policy(job.info.id);
    assert!(crate::job_sessions::policy::permitted(&dir, &job.intent).unwrap());
    let resume_revision = svc.work_queue.as_ref().unwrap().policy_revision;
    let gate = crate::job_sessions::store::append_log(&dir.join("run.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    assert!(!dir.join("owner.json").exists());
    let reply = svc.work_command("queue-enable", &serde_json::json!({"enabled":false})).unwrap();
    assert_eq!(reply["pause_effective"], false);
    assert_eq!(reply["pause_pending"], serde_json::json!(["local"]));
    assert!(!crate::job_sessions::policy::permitted(&dir, &job.intent).unwrap());
    assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_UN) }, 0);
    drop(gate);
    svc.sync_work_job_policy(job.info.id);
    assert!(svc.work_snapshot().pause_effective);
    let stale = svc.job_sessions.as_mut().unwrap().policy(&key, &binding, resume_revision, true).unwrap();
    assert_eq!(stale.outcome, "superseded");
    assert!(!stale.policy.enabled);
    assert!(!crate::job_sessions::policy::permitted(&dir, &job.intent).unwrap());
    assert!(svc.job_sessions.as_mut().unwrap().policy(&key, "foreign", resume_revision + 10, true).is_err());
    svc.jobs_tick();
    assert!(!dir.join("owner.json").exists());
    assert!(!dir.join("execution.json").exists());
    assert!(!dir.join("stop.json").exists(), "pause retains the immutable runnable intent");
    let loaded = w::store::Queue::load(&svc.opts.state_dir).unwrap();
    assert!(!loaded.enabled);
    assert!(loaded.policy_revision > resume_revision);
    svc.work_command("queue-enable", &serde_json::json!({"enabled":true})).unwrap();
    assert!(crate::job_sessions::policy::permitted(&dir, &job.intent).unwrap());
    assert_eq!(svc.job_sessions.as_ref().unwrap().records.len(), 1);
}
#[test]
fn scheduled_tick_reclaims_one_strictly_stale_unstarted_generation() {
    let d = TempDir::new("queue-stale-automatic-tick");
    let mut svc = service(&d);
    svc.work_command("chore-register", &chore(&d)).unwrap();
    svc.work_command("queue-enable", &serde_json::json!({"enabled":true})).unwrap();
    let now = 1_793_952_000;
    svc.run_chore("daily", "original", now).unwrap();
    svc.work_tick_at(now + 8 * 3600);
    assert_eq!(svc.work_queue.as_ref().unwrap().chores["daily"].claims.values().next().unwrap().info.generation, 1);
    svc.work_tick_at(now + 8 * 3600 + 1);
    let generation = svc.work_queue.as_ref().unwrap().chores["daily"].claims.values().next().unwrap().info.generation;
    assert_eq!(generation, 2);
    svc.work_tick_at(now + 8 * 3600 + 2);
    assert_eq!(svc.work_queue.as_ref().unwrap().chores["daily"].claims.values().next().unwrap().info.generation, 2);
    let original_session = svc.work_queue.as_ref().unwrap().chores["daily"].retired["original"].info.session.clone();
    let old = svc.claim_chore("daily", "original", &original_session, now + 8 * 3600 + 3).unwrap();
    assert_eq!(old.info.generation, 1);
    assert_eq!(svc.work_queue.as_ref().unwrap().chores["daily"].claims.values().next().unwrap().info.generation, 2);
}

#[test]
fn completed_turn_receipt_is_persisted_by_actual_forget_before_the_poll() {
    let d = TempDir::new("queue-completed-forget");
    let mut svc = service(&d);
    let id = svc.work_queue.as_mut().unwrap().add_batch(vec![session_spec(&d, "finished")], 1).unwrap()[0].id;
    svc.work_queue.as_mut().unwrap().records.get_mut(&id).unwrap().turn_anchor = Some(0);
    svc.work_queue
        .as_mut()
        .unwrap()
        .transition(id, "dispatched", "started", 1, Some(w::Dispatch { kind: "session".into(), id: 7 }))
        .unwrap();
    let mut session = crate::service::tests::fake_session(7, Tool::Codex);
    session.tracker.completed_turns = 1;
    session.tracker.status = Status::Ended;
    session.alive = false;
    svc.sessions.insert(7, session);
    svc.forget(7).unwrap();
    assert!(!svc.sessions.contains_key(&7));
    assert_eq!(svc.work_queue.as_ref().unwrap().records[&id].info.state, "done");
    assert_eq!(w::store::Queue::load(&svc.opts.state_dir).unwrap().records[&id].info.state, "done");
}
#[test]
fn repeated_policy_request_reestablishes_the_durable_barrier_before_ack() {
    let d = TempDir::new("queue-policy-retry-barrier");
    let mut svc = service(&d);
    let jobs = svc.job_sessions.as_mut().unwrap();
    let info = jobs
        .add_gated(
            "key".into(),
            "title".into(),
            crate::job_sessions::Spec { argv: vec!["true".into()], cwd: d.path().into(), ..Default::default() },
            "local".into(),
            None,
            Some("binding".into()),
        )
        .unwrap();
    jobs.fail_policy_barrier = true;
    assert!(jobs.policy("key", "binding", 2, false).is_err());
    let path = jobs.dir(info.id).join("policy.json");
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    jobs.fail_policy_barrier = true;
    assert!(jobs.policy("key", "binding", 2, false).is_err(), "same-payload retry must not skip the barrier");
    assert_eq!(jobs.policy("key", "binding", 2, false).unwrap().outcome, "paused");
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
}

#[test]
fn forgotten_peer_launch_keeps_completion_dedup_and_expired_replay_never_respawns() {
    let d = TempDir::new("queue-forgotten-peer-receipt");
    let mut svc = service(&d);
    let key = "queue-1-12345678-1234-4567-89ab-123456789abc";
    let mut entry = Entry::started(7, Tool::Codex, None, "worker".into(), 1);
    entry.launch_key = Some(key.into());
    entry.launch_fingerprint = Some("private-fingerprint".into());
    svc.registry.sessions.push(entry);
    let mut session = crate::service::tests::fake_session(7, Tool::Codex);
    session.alive = false;
    session.tracker.status = Status::Ended;
    session.tracker.completed_turns = 1;
    svc.sessions.insert(7, session);
    svc.forget(7).unwrap();
    assert_eq!(svc.work_turn_receipt(key).unwrap()["completed_turns"], 1);
    assert_eq!(svc.reconcile_launch(key, Some(&"private-fingerprint".into())).unwrap(), Some(7));
    assert!(svc.reconcile_launch(key, Some(&"changed".into())).is_err());
    svc.work_receipts_gc(crate::util::now_secs() + 31 * 86400).unwrap();
    assert!(svc.work_retired_receipt(key).unwrap().is_none());
    assert!(svc
        .reconcile_launch(key, Some(&"private-fingerprint".into()))
        .unwrap_err()
        .to_string()
        .contains("expired"));
    assert!(svc.sessions.is_empty());
}

#[test]
fn terminal_queue_receipt_reconciles_chore_after_crash_before_outcome_mirror() {
    for state in ["done", "failed"] {
        let d = TempDir::new("queue-chore-outcome-reconcile");
        let mut svc = service(&d);
        svc.work_command("chore-register", &chore(&d)).unwrap();
        svc.work_command("queue-enable", &serde_json::json!({"enabled":true})).unwrap();
        let now = 1_793_952_000;
        let item = svc.run_chore("daily", "original", now).unwrap();
        svc.work_queue.as_mut().unwrap().transition(item.id, state, "exact terminal receipt", now + 1, None).unwrap();
        assert!(svc.work_queue.as_ref().unwrap().chores["daily"].claims.values().next().unwrap().outcome.is_none());
        svc.work_queue = Some(w::store::Queue::load(&svc.opts.state_dir).unwrap());
        svc.work_tick_at(now + 8 * 3600 + 1);
        let chore = &svc.work_queue.as_ref().unwrap().chores["daily"];
        assert_eq!(
            chore.claims.values().next().unwrap().outcome.as_deref(),
            Some(if state == "done" { "done" } else { "failed" })
        );
        assert_eq!(chore.claims.values().next().unwrap().info.generation, 1);
        assert_eq!(svc.work_queue.as_ref().unwrap().records.len(), 1);
    }
}

#[test]
fn numeric_legacy_uuid_keys_remain_valid_and_keep_retired_receipts() {
    let d = TempDir::new("queue-numeric-legacy-key");
    let mut svc = service(&d);
    for (i, key) in [
        "queue-12345678-1234-4567-89ab-123456789abc",
        "queue-00000001-0000-4000-8000-000000000001",
        "queue-87654321-abcd-4abc-8abc-0123456789ab",
    ]
    .into_iter()
    .enumerate()
    {
        assert!(svc.work_validate_new_key(key).is_ok(), "legacy key {key}");
        let id = i as u64 + 7;
        let mut entry = Entry::started(id, Tool::Codex, None, "worker".into(), 1);
        entry.launch_key = Some(key.into());
        entry.launch_fingerprint = Some("fingerprint".into());
        svc.registry.sessions.push(entry);
        let mut session = crate::service::tests::fake_session(id, Tool::Codex);
        session.alive = false;
        svc.sessions.insert(id, session);
        svc.forget(id).unwrap();
        svc.work_receipts_gc(crate::util::now_secs() + 31 * 86400).unwrap();
        assert_eq!(svc.reconcile_launch(key, Some(&"fingerprint".into())).unwrap(), Some(id));
    }
    assert!(svc.work_validate_new_key("queue-1-12345678-1234-4567-89ab-123456789abc").is_err());
}
#[test]
fn ordinary_ended_session_can_be_forgotten_when_work_queue_is_unavailable() {
    let d = TempDir::new("queue-unavailable-forget");
    let mut svc = service(&d);
    svc.work_queue = None;
    let mut session = crate::service::tests::fake_session(7, Tool::Codex);
    session.alive = false;
    svc.sessions.insert(7, session);
    svc.registry.sessions.push(Entry::started(7, Tool::Codex, None, "manual".into(), 1));
    svc.forget(7).unwrap();
    assert!(!svc.sessions.contains_key(&7));
    assert!(!svc.registry.sessions.iter().any(|entry| entry.id == 7));
}
