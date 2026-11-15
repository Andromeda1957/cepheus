use super::*;
use crate::testsupport::TempDir;
fn service(d: &TempDir) -> Service {
    let (tx, _) = std::sync::mpsc::channel();
    Service::new(crate::service::tests::options(d, "true"), tx)
}
fn config() -> HostConfig {
    HostConfig {
        role: Default::default(),
        name: "worker".into(),
        label: None,
        tag: 1,
        addresses: vec![],
        ssh: None,
        fingerprint: String::new(),
    }
}
#[test]
fn protocol13_never_receives_conditional_chore_cancellation() {
    let d = TempDir::new("queue-old-conditional-stop");
    let mut svc = service(&d);
    let (out, requests) = std::sync::mpsc::channel();
    svc.hosts.insert(
        1,
        HostSlot {
            config: config(),
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: 13, ..Default::default() }),
            state: None,
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    let error =
        svc.job_peer_send("worker", 7, "job-stop", serde_json::json!({"key":"old","if_unstarted":true})).unwrap_err();
    assert!(error.to_string().contains("Update needed"));
    assert!(requests.try_recv().is_err(), "P2would ignore unknown conditional flag and stop a running turn");
    svc.hosts.get_mut(&1).unwrap().welcome.as_mut().unwrap().version = 14;
    svc.job_peer_send("worker", 7, "job-stop", serde_json::json!({"key":"old","if_unstarted":true})).unwrap();
    let message: ClientMsg = crate::protocol::read_msg(&mut requests.recv().unwrap().as_slice()).unwrap().unwrap();
    assert!(matches!(message,ClientMsg::Fleet{action,..}if action=="job-stop"));
}
#[test]
fn peer_interactive_dispatch_handle_keeps_origin_scope_when_local_ids_collide() {
    let d = TempDir::new("queue-dispatch-host-id");
    let mut svc = service(&d);
    let spec=serde_json::from_value(serde_json::json!({"key":"queue","title":"queue","kind":"session","cwd":d.path(),"provider":"codex","prompt":"private","placement":"worker"})).unwrap();
    let q = svc.work_queue.as_mut().unwrap();
    let id = q.add_batch(vec![spec], 1).unwrap()[0].id;
    q.records.get_mut(&id).unwrap().executor = Some("worker".into());
    svc.sessions.insert(7, crate::service::tests::fake_session(7, crate::protocol::Tool::Codex));
    assert!(svc.work_peer_result(
        "worker",
        1,
        id | (1u64 << 62),
        &serde_json::json!({"session":7,"turn_anchor":4}),
        None
    ));
    let dispatch = &svc.work_queue.as_ref().unwrap().records[&id].info.dispatch.as_ref().unwrap();
    assert_eq!(dispatch.kind, "session");
    assert_eq!(dispatch.id, crate::protocol::host_id(1, 7));
    assert_ne!(dispatch.id, 7);
    assert!(svc.sessions[&7].alive);
    let before = (*dispatch).clone();
    assert!(!svc.work_peer_result("other", 2, id | (1u64 << 62), &serde_json::json!({"session":8}), None));
    assert_eq!(svc.work_queue.as_ref().unwrap().records[&id].info.dispatch.as_ref().unwrap(), &before);

    for alive in [false, true] {
        let d = TempDir::new("queue-advertised-cloud-unsupported");
        let mut svc = service(&d);
        let tool = crate::protocol::Tool::Dynamic("fixture-cloud");
        svc.scheduler.settings.repositories.insert("cloud-workspace".into(), d.path().to_path_buf());
        let meminfo = svc.scheduler.settings.probe.root.as_ref().unwrap().join("meminfo");
        std::fs::write(&meminfo, "MemTotal: 16777216 kB\nMemAvailable: 1024 kB\n").unwrap();
        let mut native = crate::service::tests::fake_session(7, tool);
        native.tracker.completed_turns = 5;
        svc.sessions.insert(7, native);
        let mut peer_session = svc.own_state().sessions.pop().unwrap();
        svc.sessions.clear();
        peer_session.id = crate::protocol::host_id(1, 7);
        peer_session.host = Some("worker".into());
        let (out, requests) = std::sync::mpsc::channel();
        svc.hosts.insert(
            1,
            HostSlot {
                config: config(),
                stop: Arc::new(AtomicBool::new(false)),
                out: Some(out),
                welcome: Some(Welcome { version: 14, ..Default::default() }),
                state: Some(DeckState {
                    work_queue_capable: true,
                    scheduler: crate::scheduler::Snapshot {
                        headroom: crate::scheduler::Headroom {
                            available_mib: Some(65536),
                            total_mib: Some(131072),
                            memory_pressure: Some(0.0),
                            cpu_pressure: Some(0.0),
                            load: Some(0.0),
                            cpus: 8,
                            at: crate::util::now_secs(),
                        },
                        repositories: [("cloud-workspace".into(), d.path().to_string_lossy().into_owned())].into(),
                        ..Default::default()
                    },
                    providers: vec![crate::providers::Info {
                        id: "fixture-cloud".into(),
                        label: "Cloud".into(),
                        cloud: true,
                        host: Some("worker".into()),
                        ..Default::default()
                    }],
                    sessions: if alive { vec![peer_session] } else { vec![] },
                    ..Default::default()
                }),
                reason: None,
                stats: None,
                outage: Default::default(),
                reached: false,
            },
        );
        assert_eq!(
            svc.scheduler_host_min(Some(d.path()), "auto", false, true).unwrap().0.as_deref(),
            Some("worker"),
            "the constrained native fixture makes Auto select this capable cloud peer"
        );
        for placement in ["worker", "auto"] {
            let spec = serde_json::from_value(serde_json::json!({
                "key":format!("new-cloud-{placement}"),"title":"cloud","kind":"session","cwd":d.path(),
                "provider":"fixture-cloud","prompt":"PRIVATE_TASK","placement":placement
            }))
            .unwrap();
            let q = svc.work_queue.as_mut().unwrap();
            let item = q.add_batch(vec![spec], crate::util::now_secs()).unwrap()[0].id;
            q.enabled = true;
            q.last_poll = 0;
            svc.work_tick();
            let record = &svc.work_queue.as_ref().unwrap().records[&item];
            assert_eq!(record.info.state, "held");
            assert!(record.info.reason.contains("Cloud providers do not support"));
            assert!(!record.attempted && record.info.dispatch.is_none());
            assert!(record.turn_anchor.is_none() && record.executor.is_none());
            assert!(requests.try_recv().is_err(), "unsupported cloud starts never send work-start to a peer");
        }
        assert!(svc.sessions.is_empty() && svc.registry.sessions.is_empty());

        // The same provider ID is ordinary on this host. A peer's cloud flag
        // must not veto native placement; independent admission still holds it.
        std::fs::write(&meminfo, "MemTotal: 16777216 kB\nMemAvailable: 12582912 kB\n").unwrap();
        svc.opts.config.providers = vec![serde_json::from_value(serde_json::json!({
            "id":"fixture-cloud","label":"Native","command":["true"],"cloud":false
        }))
        .unwrap()];
        *svc.launch_capabilities.borrow_mut() = None;
        svc.scheduler.error = Some("injected native admission hold".into());
        let spec = serde_json::from_value(serde_json::json!({
            "key":"native-homonym","title":"native","kind":"session","cwd":d.path(),
            "provider":"fixture-cloud","prompt":"PRIVATE_TASK","placement":"local"
        }))
        .unwrap();
        let item = svc.work_queue.as_mut().unwrap().add_batch(vec![spec], crate::util::now_secs()).unwrap()[0].id;
        svc.work_queue.as_mut().unwrap().last_poll = 0;
        svc.work_tick();
        let record = &svc.work_queue.as_ref().unwrap().records[&item];
        assert_eq!(record.executor.as_deref(), Some("local"));
        assert_eq!(record.turn_anchor, Some(0));
        assert!(!record.info.reason.contains("Cloud providers"));
        assert!(!record.attempted && record.info.dispatch.is_none(), "native admission prevents a real launch");
        assert!(svc.sessions.is_empty() && svc.registry.sessions.is_empty());
        assert!(requests.try_recv().is_err());
    }
}

#[test]
fn first_remote_enable_cannot_reuse_a_preplacement_local_policy_ack() {
    let d = TempDir::new("queue-first-remote-gate");
    let mut svc = service(&d);
    svc.work_command("queue-enable", &serde_json::json!({"enabled":true})).unwrap();
    let spec = serde_json::from_value(
        serde_json::json!({"key":"remote","title":"remote","kind":"job","cwd":d.path(),"argv":["true"]}),
    )
    .unwrap();
    let q = svc.work_queue.as_mut().unwrap();
    let item = q.add_batch(vec![spec], crate::util::now_secs()).unwrap()[0].id;
    let key = q.records[&item].dispatch_key.clone();
    let info = svc
        .job_sessions
        .as_mut()
        .unwrap()
        .add_gated(
            key.clone(),
            "remote".into(),
            crate::job_sessions::Spec { argv: vec!["true".into()], cwd: d.path().into(), ..Default::default() },
            "worker".into(),
            None,
            Some(key.clone()),
        )
        .unwrap();
    svc.sync_work_job_policy(info.id);
    assert!(svc.job_sessions.as_ref().unwrap().policy_ack(info.id).unwrap().is_some());
    svc.job_sessions.as_mut().unwrap().records.get_mut(&info.id).unwrap().executor =
        Some(crate::job_sessions::Executor { host: Some("worker".into()), cwd: d.path().into(), repository: None });
    assert!(svc.job_sessions.as_ref().unwrap().policy_ack(info.id).unwrap().is_none());
    let (out, requests) = std::sync::mpsc::channel();
    svc.hosts.insert(
        1,
        HostSlot {
            config: config(),
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: 14, ..Default::default() }),
            state: None,
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    svc.sync_work_job_policy(info.id);
    let frame = requests.recv().unwrap();
    let msg: ClientMsg = crate::protocol::read_msg(&mut frame.as_slice()).unwrap().unwrap();
    assert!(
        matches!(msg,ClientMsg::Fleet{action,args,..}if action=="job-policy"&&args["enabled"]==true&&args["binding"]==key)
    );
    let revision = svc.work_queue.as_ref().unwrap().policy_revision;
    let reply = serde_json::json!({"policy":{"policy":{"nonce":"private-peer-nonce","binding":key,"revision":revision,"enabled":true},"outcome":"enabled"}});
    assert!(svc.job_peer_result("worker", info.id | (1u64 << 63), &reply, None));
    let ack = svc.job_sessions.as_ref().unwrap().policy_ack(info.id).unwrap().unwrap();
    assert_eq!(ack.executor.as_deref(), Some("worker"));
    svc.sync_work_job_policy(info.id);
    assert!(requests.try_recv().is_err(), "acknowledged unchanged remote policy must not poll/write forever");
}
