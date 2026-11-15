use super::*;
use crate::{
    protocol::Tool,
    scheduler::{Headroom, Snapshot},
    service::Ev,
    testsupport::TempDir,
};
use serde_json::json;
use std::sync::mpsc::{self, Receiver};
fn service(dir: &TempDir, low: bool) -> Service {
    let mut opts = crate::service::tests::options(dir, "true");
    let probe = dir.path().join("proc");
    std::fs::create_dir_all(probe.join("pressure")).unwrap();
    std::fs::write(
        probe.join("meminfo"),
        format!("MemTotal: 16777216 kB\nMemAvailable: {} kB\n", if low { 1024 } else { 12582912 }),
    )
    .unwrap();
    std::fs::write(probe.join("pressure/memory"), "full avg10=0\n").unwrap();
    std::fs::write(probe.join("pressure/cpu"), "some avg60=0\n").unwrap();
    opts.config.scheduler.probe.root = Some(probe);
    opts.config.scheduler.repositories.insert("same-repository".into(), dir.path().to_path_buf());
    let (tx, _) = mpsc::channel();
    Service::new(opts, tx)
}
fn peer(
    svc: &mut Service,
    name: &str,
    tag: u16,
    memory: u64,
    online: bool,
    same: bool,
    version: u32,
) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let headroom = Headroom {
        available_mib: Some(memory),
        total_mib: Some(65536),
        memory_pressure: Some(0.0),
        cpu_pressure: Some(0.0),
        load: Some(0.0),
        cpus: 8,
        at: crate::util::now_secs(),
    };
    let snapshot = Snapshot {
        headroom,
        admitted: true,
        memory_free: true,
        repositories: if same {
            [("same-repository".into(), "/srv/project".into())].into()
        } else {
            Default::default()
        },
        ..Snapshot::default()
    };
    let state = DeckState {
        scheduler: snapshot,
        available_tools: Some(vec!["codex".into(), "claude".into()]),
        ..DeckState::default()
    };
    svc.hosts.insert(
        tag,
        HostSlot {
            config: HostConfig {
                role: Default::default(),
                name: name.into(),
                tag,
                label: None,
                addresses: vec![],
                ssh: None,
                fingerprint: "a".repeat(64),
            },
            stop: Arc::new(AtomicBool::new(false)),
            out: online.then_some(tx),
            welcome: Some(Welcome { ok: true, version, ..Welcome::default() }),
            state: Some(state),
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    rx
}

#[test]
fn automatic_placement_ignores_offline_old_stale_and_wrong_repositories() {
    let dir = TempDir::new("scheduler-placement");
    let mut svc = service(&dir, false);
    let _ = peer(&mut svc, "eligible", 1, 30000, true, true, 14);
    let _ = peer(&mut svc, "less", 2, 25000, true, true, 14);
    let _ = peer(&mut svc, "offline", 3, 60000, false, true, 14);
    let _ = peer(&mut svc, "wrong", 4, 60000, true, false, 14);
    let _ = peer(&mut svc, "old", 5, 60000, true, true, 11);
    let _ = peer(&mut svc, "stale", 6, 60000, true, true, 14);
    svc.hosts.get_mut(&6).unwrap().state.as_mut().unwrap().scheduler.headroom.at = 0;
    let (host, path) = svc.scheduler_host(None, "auto").unwrap();
    assert_eq!(host.as_deref(), Some("eligible"));
    assert_eq!(path, std::path::Path::new("/srv/project"));
    assert_eq!(svc.workspace_host(None, "auto", Some(Tool::Codex)).unwrap().0.as_deref(), Some("eligible"));
    svc.hosts.get_mut(&1).unwrap().state.as_mut().unwrap().available_tools = Some(vec![]);
    assert_eq!(svc.workspace_host(None, "auto", Some(Tool::Codex)).unwrap().0.as_deref(), Some("less"));
    svc.hosts.get_mut(&2).unwrap().state.as_mut().unwrap().available_tools = Some(vec![]);
    svc.opts.config.plugins.insert("provider-codex".into(), false);
    *svc.launch_capabilities.borrow_mut() = None;
    assert!(svc.workspace_host(None, "auto", Some(Tool::Codex)).is_err());
    assert!(
        svc.workspace_host(None, "auto", Some(Tool::Shell)).is_ok(),
        "terminals do not require an agent executable"
    );
}
#[test]
fn a_resource_short_hub_dispatches_to_an_eligible_peer_and_tracks_ack() {
    let dir = TempDir::new("scheduler-hub-short");
    let mut svc = service(&dir, true);
    let rx = peer(&mut svc, "eligible", 1, 30000, true, true, 14);
    let queue = svc.fleet_command("enqueue", &json!({"provider":"codex"})).unwrap();
    svc.scheduler_tick();
    let bytes = rx.try_recv().expect("the destination may admit work even while the hub cannot");
    let msg = crate::protocol::read_msg::<_, ClientMsg>(&mut bytes.as_slice()).unwrap().unwrap();
    assert!(
        matches!(&msg,ClientMsg::Fleet{action,args,..} if action=="start" && args["provider"]=="codex" && args["cwd"].as_str().is_some_and(|p| std::path::Path::new(p)==std::path::Path::new("/srv/project")) && args["repository"]=="same-repository")
    );
    assert!(svc.fleet_command("cancel", &queue).unwrap_err().to_string().contains("already dispatched"));
    assert_eq!(svc.scheduler.starts.len(), 1, "an inflight remote start keeps its ownership receipt");
    assert_eq!(svc.placement_reserved.borrow()[&Some("eligible".into())].len(), 1);
    let key = svc.scheduler.starts[0].key.clone();
    svc.scheduler.last_poll = 0;
    svc.scheduler.starts[0].attempted_at = Some(0);
    svc.scheduler_tick();
    assert_eq!(
        svc.placement_reserved.borrow()[&Some("eligible".into())].len(),
        1,
        "retrying an idempotent dispatch does not reserve twice"
    );
    let again = rx.try_recv().unwrap();
    let again = crate::protocol::read_msg::<_, ClientMsg>(&mut again.as_slice()).unwrap().unwrap();
    assert!(matches!(again,ClientMsg::Fleet{args,..} if args["key"]==key));
    let reset = crate::util::now_secs() + 60;
    svc.on_host_msg(
        1,
        ServerMsg::FleetResult {
            request: queue["id"].as_u64().unwrap(),
            data: json!({"held":true,"reason":"quota","reset":reset}),
            error: None,
        },
    );
    assert_eq!(svc.scheduler.starts[0].reset, Some(reset), "the hub displays the destination's actual reset");
    assert_eq!(svc.scheduler.starts[0].reason, "quota");
    svc.on_host_msg(
        1,
        ServerMsg::FleetResult { request: queue["id"].as_u64().unwrap(), data: json!({"session":7}), error: None },
    );
    assert_eq!(svc.scheduler.starts[0].session, Some(crate::protocol::host_id(1, 7)));
    let saved: crate::scheduler::Saved =
        serde_json::from_slice(&std::fs::read(svc.opts.state_dir.join("scheduler.json")).unwrap()).unwrap();
    assert_eq!(saved.starts[0].target.as_deref(), Some("eligible"));
    assert_eq!(saved.starts[0].key, key);

    // A failed checkpoint must prevent dispatch and retain every queued item.
    let blocked = TempDir::new("scheduler-dispatch-save-failure");
    let mut svc = service(&blocked, true);
    let rx = peer(&mut svc, "eligible", 1, 30000, true, true, PROTOCOL_VERSION);
    for _ in 0..2 {
        svc.fleet_command("enqueue", &json!({"provider":"codex"})).unwrap();
    }
    let state = svc.opts.state_dir.join("scheduler.json");
    std::fs::rename(&state, state.with_extension("preserved")).unwrap();
    std::fs::create_dir(&state).unwrap();
    svc.scheduler_tick();
    assert!(rx.try_recv().is_err(), "an unsaved destination must never be sent");
    assert_eq!(svc.scheduler.starts.len(), 2, "failed checkpoint retains the complete queue");
    assert!(svc.scheduler.starts.iter().all(|item| item.target.as_deref() == Some("eligible")));
}
#[test]
fn a_federated_session_cannot_be_recorded_against_another_hosts_same_number() {
    let dir = TempDir::new("scheduler-session-host");
    let mut svc = service(&dir, false);
    let a = peer(&mut svc, "one", 1, 30000, true, true, 14);
    let b = peer(&mut svc, "two", 2, 30000, true, true, 14);
    let (tx, rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 3, out: tx, frame_busy: Arc::new(AtomicBool::new(false)) });
    let message = ClientMsg::Fleet {
        request: 88,
        action: "rerun-record".into(),
        args: json!({"host":"two","session":crate::protocol::host_id(1,5),"command":"cargo test"}),
    };
    assert!(svc.route_client(3, message).is_none());
    let replies: Vec<_> = rx
        .try_iter()
        .filter_map(|out| match out {
            Out::Msg(bytes) => crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).ok().flatten(),
            _ => None,
        })
        .collect();
    assert!(replies.iter().any(
        |m| matches!(m,ServerMsg::FleetResult{request:88,error:Some(error),..} if error.contains("another host"))
    ));
    assert!(a.try_recv().is_err() && b.try_recv().is_err());
}
#[test]
fn an_old_peer_gets_an_explicit_update_error_before_any_fleet_forward() {
    let dir = TempDir::new("scheduler-old-peer");
    let mut svc = service(&dir, false);
    let peer = peer(&mut svc, "old", 1, 30000, true, true, 11);
    let (tx, rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 3, out: tx, frame_busy: Arc::new(AtomicBool::new(false)) });
    assert!(svc
        .route_client(3, ClientMsg::Fleet { request: 44, action: "status".into(), args: json!({"host":"old"}) })
        .is_none());
    assert!(rx.try_iter().any(|out|matches!(out,Out::Msg(bytes) if matches!(crate::protocol::read_msg::<_,ServerMsg>(&mut bytes.as_slice()).ok().flatten(),Some(ServerMsg::FleetResult{request:44,error:Some(error),..}) if error.contains("Update needed")))));
    assert!(peer.try_recv().is_err());
}

#[test]
fn a_peers_default_account_windows_never_hold_local_dispatch() {
    let dir = TempDir::new("scheduler-local-quota");
    let mut svc = service(&dir, false);
    let _remote = peer(&mut svc, "different-default-account", 1, 30000, true, true, 14);
    svc.hosts.get_mut(&1).unwrap().state.as_mut().unwrap().usage.codex = vec![crate::protocol::UsageWindow {
        label: "weekly".into(),
        used_percent: Some(99.0),
        resets_at: Some(crate::util::now_secs() + 3600),
        updated_at: crate::util::now_secs(),
    }];
    let result = svc.fleet_command("start", &json!({"provider":"codex","key":"local-account-only"})).unwrap();
    assert!(result["session"].as_u64().is_some(), "destination quotas use local account evidence: {result}");
    for session in svc.sessions.values() {
        session.hosted.as_ref().unwrap().signal(libc::SIGTERM);
    }
}

#[test]
fn a_hundred_sibling_worktree_placements_use_precomputed_identity_without_git() {
    let dir = TempDir::new("scheduler-many-worktrees");
    let siblings = TempDir::new("scheduler-sibling-roots");
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let mut svc = service(&dir, false);
    let _remote = peer(&mut svc, "eligible", 1, 30000, true, true, 14);
    let began = Instant::now();
    for i in 0..100 {
        let root = siblings.path().join(format!("worker-{i}"));
        let git = dir.path().join(format!(".git/worktrees/worker-{i}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("commondir"), "../..\n").unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", git.display())).unwrap();
        // This deliberately is not a valid Git repository: matching must use
        // the precomputed common-directory identity, never spawn Git.
        for _ in 0..2 {
            let (host, _) = svc.scheduler_host(Some(&root), "auto").unwrap();
            assert!(host.is_none(), "a linked task tree must stay on its native host");
        }
        svc.fleet_command("enqueue", &json!({"provider":"codex","cwd":root})).unwrap();
    }
    // The same native tree boundary also wins when nested inside the primary.
    let nested = dir.path().join("tasks/nested");
    let metadata = dir.path().join(".git/worktrees/nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(&metadata).unwrap();
    std::fs::write(metadata.join("commondir"), "../..\n").unwrap();
    std::fs::write(nested.join(".git"), format!("gitdir: {}\n", metadata.display())).unwrap();
    assert_eq!(svc.scheduler.repository_for(&nested).unwrap().1, nested);
    assert!(
        svc.workspace_host(Some(&nested), "auto", None).unwrap().0.is_none(),
        "a nested linked tree cannot map to peer primary files"
    );
    svc.scheduler_tick();
    assert!(svc.scheduler.starts.iter().all(|item| item.target.as_deref().is_none_or(|host| host == "@here")));
    eprintln!("fleet100-sibling-placement: elapsed_ms={}", began.elapsed().as_millis());
}

#[test]
fn automatic_placement_resolves_real_directories_before_repository_mapping() {
    let dir = TempDir::new("scheduler-placement-boundary");
    let outside = TempDir::new("scheduler-placement-outside");
    let mut svc = service(&dir, true);
    let _remote = peer(&mut svc, "eligible", 1, 30000, true, true, 14);
    std::fs::create_dir_all(dir.path().join("nested")).unwrap();
    let cwd = dir.path().join("nested/../nested");
    let (host, path) = svc.scheduler_host(Some(&cwd), "auto").unwrap();
    assert_eq!(host.as_deref(), Some("eligible"));
    assert_eq!(path, std::path::Path::new("/srv/project/nested"));
    let escape = dir.path().join("..").join(outside.path().file_name().unwrap());
    assert!(svc.scheduler_host(Some(&escape), "auto").is_err(), "dot-dot cannot borrow the lexical repository prefix");
    std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
    assert!(
        svc.scheduler_host(Some(&dir.path().join("escape")), "auto").is_err(),
        "a symlink outside the repository cannot be placed on its peer"
    );
    svc.hosts
        .get_mut(&1)
        .unwrap()
        .state
        .as_mut()
        .unwrap()
        .scheduler
        .repositories
        .insert("same-repository".into(), "relative/project".into());
    assert!(svc.scheduler_host(Some(&cwd), "auto").is_err(), "peers must advertise absolute roots");
    svc.hosts
        .get_mut(&1)
        .unwrap()
        .state
        .as_mut()
        .unwrap()
        .scheduler
        .repositories
        .insert("same-repository".into(), "/srv/project/../other".into());
    assert!(svc.scheduler_host(Some(&cwd), "auto").is_err(), "peer roots cannot contain traversal");
}

#[test]
fn queued_remote_starts_validate_destination_containment() {
    for escape in [false, true] {
        let source = TempDir::new("scheduler-proof-source");
        let destination = TempDir::new("scheduler-proof-destination");
        let outside = TempDir::new("scheduler-proof-outside");
        std::fs::create_dir_all(source.path().join("nested")).unwrap();
        if escape {
            std::os::unix::fs::symlink(outside.path(), destination.path().join("nested")).unwrap();
        } else {
            std::fs::create_dir_all(destination.path().join("nested")).unwrap();
        }
        let mut hub = service(&source, true);
        let mut receiver = service(&destination, false);
        let fleet_rx = peer(&mut hub, "eligible", 1, 30000, true, true, 14);
        hub.hosts
            .get_mut(&1)
            .unwrap()
            .state
            .as_mut()
            .unwrap()
            .scheduler
            .repositories
            .insert("same-repository".into(), destination.path().to_string_lossy().into_owned());
        let (hub_tx, _hub_rx) = mpsc::channel();
        hub.handle(Ev::ClientNew { id: 9, out: hub_tx, frame_busy: Arc::new(AtomicBool::new(false)) });
        let (upstream_tx, upstream_rx) = mpsc::channel();
        hub.clients.get_mut(&9).unwrap().upstreams.insert(1, upstream_tx);
        let (receiver_tx, _receiver_rx) = mpsc::channel();
        receiver.handle(Ev::ClientNew { id: 9, out: receiver_tx, frame_busy: Arc::new(AtomicBool::new(false)) });
        hub.fleet_command("enqueue", &json!({"provider":"codex","cwd":source.path().join("nested")})).unwrap();
        hub.scheduler_tick();
        let bytes = fleet_rx.try_recv().expect("queued remote start wire request");
        let message = crate::protocol::read_msg::<_, ClientMsg>(&mut bytes.as_slice()).unwrap().unwrap();
        let ClientMsg::Fleet { action, args, .. } = message else { panic!("expected fleet start") };
        assert_eq!(args["repository"], "same-repository");
        let result = receiver.fleet_command(&action, &args);
        assert_eq!(result.is_err(), escape, "receiver validates mapped cwd before spawning");
        assert_eq!(receiver.sessions.len(), if escape { 0 } else { 1 });
        for session in receiver.sessions.values() {
            assert_eq!(session.cwd.as_deref(), Some(destination.path().join("nested").as_path()));
            session.hosted.as_ref().unwrap().signal(libc::SIGTERM);
        }
        // An explicitly selected host retains its existing arbitrary-cwd
        // semantics. Claude: native Codex creation needs a protocol-19 host.
        if escape {
            let create = ClientMsg::Create {
                tool: Tool::Claude,
                model: None,
                effort: None,
                host: Some("eligible".into()),
                cwd: Some(outside.path().to_string_lossy().into_owned()),
                command: vec![],
                name: None,
                request: Some(98),
            };
            assert!(hub.route_client(9, create).is_none());
            let bytes = upstream_rx.try_recv().unwrap();
            let message = crate::protocol::read_msg::<_, ClientMsg>(&mut bytes.as_slice()).unwrap().unwrap();
            assert!(matches!(&message, ClientMsg::Create { host: None, .. }));
            let create = receiver.route_client(9, message).expect("explicit host start stays unguarded");
            receiver.handle_client(9, create);
            assert_eq!(receiver.sessions.len(), 1);
            for session in receiver.sessions.values() {
                session.hosted.as_ref().unwrap().signal(libc::SIGTERM);
            }
        }
    }
}
