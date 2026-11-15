//! Federation authority and projection regressions. All peers are in-memory channels.
use super::*;
use crate::{
    attention as a,
    protocol::{host_id, Tool},
    service::Ev,
    testsupport::TempDir,
};
use serde_json::{json, Value};
use std::sync::mpsc::{self, Receiver};

fn service(dir: &TempDir) -> Service {
    let mut opts = crate::service::tests::options(dir, "true");
    opts.config.scheduler.repositories.insert("shared".into(), dir.path().canonicalize().unwrap());
    let (tx, _) = mpsc::channel();
    Service::new(opts, tx)
}
fn peer(svc: &mut Service, name: &str, tag: u16, online: bool, version: u32) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let mut state = DeckState { attention_capable: true, ..Default::default() };
    state.scheduler.headroom.at = crate::util::now_secs();
    state.scheduler.repositories.insert("shared".into(), "/srv/project".into());
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
            welcome: Some(Welcome { ok: true, version, ..Default::default() }),
            state: Some(state),
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    rx
}
fn client(svc: &mut Service) -> Receiver<Out> {
    let (tx, rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 7, out: tx, frame_busy: Arc::new(AtomicBool::new(false)) });
    rx
}
fn fleet(bytes: &[u8]) -> (u64, String, Value) {
    match crate::protocol::read_msg::<_, ClientMsg>(&mut &bytes[..]).unwrap().unwrap() {
        ClientMsg::Fleet { request, action, args } => (request, action, args),
        _ => panic!("expected fleet command"),
    }
}
/// An older peer's snapshot; its inbox, stalls and away are ignored.
fn fixture() -> a::Snapshot {
    serde_json::from_value(json!({
        "inbox":[{"id":"opaque-entry","session":2,"job":3,"reason":"stall","blocked_since":10}],
        "stalls":[{"session":2,"kind":"files","since":11}], "queued":{"2":4},
        "messages":[{"id":"opaque-uuid","session":2,"state":"queued","created_at":12,"updated_at":13,"reason":"held"}],
        "away":null,"supervisor":{"enabled":false,"revision":8,"rules":0,"pending":[],
            "audit":[{"session":2,"at":14,"category":"declined","rule":null}]}
    }))
    .unwrap()
}

#[test]
fn attention_wrong_host_and_unknown_tags_never_fall_back_to_same_local_session() {
    let dir = TempDir::new("attention-route-host");
    let mut svc = service(&dir);
    let one = peer(&mut svc, "one", 1, true, 17);
    let two = peer(&mut svc, "two", 2, true, 15);
    let replies = client(&mut svc);
    svc.sessions.insert(42, crate::service::tests::fake_session(42, Tool::Codex));
    for args in [
        json!({"session":host_id(99,42)}),
        json!({"session":host_id(1,42),"host":"two"}),
        json!({"session":host_id(1,42),"host":"@here"}),
        json!({"session":42,"host":"missing"}),
        json!({"session":42,"host":99}),
    ] {
        assert!(svc.route_client(7, ClientMsg::Fleet { request: 80, action: "attention-send".into(), args }).is_none());
    }
    let errors = replies
        .try_iter()
        .filter(|reply| {
            matches!(reply, Out::Msg(bytes) if matches!(
        crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).ok().flatten(),
        Some(ServerMsg::FleetResult { request:80,error:Some(_),.. })))
        })
        .count();
    assert_eq!(errors, 5);
    assert!(one.try_recv().is_err() && two.try_recv().is_err());
    assert!(svc.attention.as_ref().unwrap().store.messages.is_empty());
    assert_eq!(svc.sessions[&42].inputs, 0);
    let mut args = json!({"session":host_id(1,42)});
    svc.attention_route_target("attention-send", &mut args).unwrap();
    assert_eq!(args, json!({"session":42,"host":"one"}));
}

#[test]
fn old_attention_peer_is_explicitly_refused_before_routing() {
    let native =
        json!({"id":"read","tool":"Read","pattern":{"kind":"native_exact","input":{"file_path":"/repo/safe.txt"}}});
    let shell = json!({"id":"shell","tool":"Bash","pattern":{"kind":"exact","command":"cat safe.txt"}});
    for (version, online, known) in
        [(14, true, true), (15, true, true), (16, true, true), (17, true, true), (17, false, true), (17, true, false)]
    {
        for (action, args, minimum) in [
            ("attention-send", json!({"host":"old","session":2}), 17),
            ("attention-supervisor-rules", json!({"host":"old","rules":[shell.clone()]}), 15),
            ("attention-supervisor-rules", json!({"host":"old","rules":[native.clone(),shell.clone()]}), 16),
        ] {
            let dir = TempDir::new("attention-old-peer");
            let mut svc = service(&dir);
            let coordinator = peer(&mut svc, "old", 1, online, version);
            if !known {
                svc.hosts.get_mut(&1).unwrap().welcome = None;
            }
            let replies = client(&mut svc);
            let (upstream, rx) = mpsc::channel();
            svc.clients.get_mut(&7).unwrap().upstreams.insert(1, upstream);
            assert!(svc.route_client(7, ClientMsg::Fleet { request: 80, action: action.into(), args }).is_none());
            if online && known && version >= minimum {
                let (_, forwarded, _) = fleet(&rx.try_recv().expect("supported target receives one request"));
                assert_eq!(forwarded, action);
                assert!(replies.try_recv().is_err());
            } else {
                assert!(rx.try_recv().is_err(), "unsupported native rules never reach the old peer parser");
                let Out::Msg(bytes) = replies.try_recv().expect("explicit compatibility refusal") else {
                    panic!("reply")
                };
                assert!(matches!(crate::protocol::read_msg::<_,ServerMsg>(&mut bytes.as_slice()).unwrap(),
                    Some(ServerMsg::FleetResult{request:80,error:Some(error),..}) if error.contains("Update needed")));
            }
            assert!(
                coordinator.try_recv().is_err(),
                "client requests use their own upstream, never the background coordinator channel"
            );
        }
    }
}

#[test]
fn message_body_and_cancel_keep_pinned_origin_even_when_peer_cache_disappears() {
    let dir = TempDir::new("attention-message-origin");
    let mut svc = service(&dir);
    let _rx = peer(&mut svc, "one", 1, true, 15);
    let info = svc
        .attention
        .as_mut()
        .unwrap()
        .store
        .add("key", "private canary", a::store::Target { session: 2, spawned_ms: 1, conversation: None, pid: None }, 1)
        .unwrap();
    for action in ["attention-message", "attention-cancel"] {
        assert!(svc.attention_route_target(action, &mut json!({"id":"vanished-remote","session":2})).is_err());
        let mut local = json!({"id":info.id});
        svc.attention_route_target(action, &mut local).unwrap();
        let mut unknown = json!({"id":"vanished-remote"});
        assert!(svc.attention_route_target(action, &mut unknown).is_err());
        let mut pinned = json!({"id":"vanished-remote","host":"one"});
        svc.attention_route_target(action, &mut pinned).unwrap();
        assert_eq!(pinned["host"], "one");
        svc.hosts.get_mut(&1).unwrap().state.as_mut().unwrap().attention.messages =
            vec![serde_json::from_value(json!({
                "id":info.id,"session":host_id(1,2),"state":"queued","created_at":1,"updated_at":1,"reason":""
            }))
            .unwrap()];
        assert!(svc.attention_route_target(action, &mut local).is_err(), "collision needs explicit origin");
        let mut here = json!({"id":info.id,"host":"@here"});
        svc.attention_route_target(action, &mut here).unwrap();
        assert!(here.get("host").is_none());
        svc.hosts.get_mut(&1).unwrap().state = None;
        svc.attention_route_target(action, &mut pinned).unwrap();
        svc.hosts.get_mut(&1).unwrap().state = Some(DeckState::default());
    }
}

#[test]
fn remote_fleet_results_project_all_attention_identifiers_without_touching_receipts() {
    let data = json!({"attention":fixture(),"supervisor":fixture().supervisor,
        "message":{"session":2,"id":"uuid"},"messages":[{"session":2,"id":"uuid"}],
        "decision":{"id":"decision","recipients":[2],"targets":[{"session":2,"pid":45,"spawned_ms":99}]},
        "targets":[{"session":2,"pid":45,"spawned_ms":99}],"recipients":[2,{"session":2}],
        "job":{"id":3},"jobs":[{"id":3}],"key":"opaque-key"});
    let ServerMsg::FleetResult { request, data, .. } =
        fed::inbound(4, "peer", "peer", ServerMsg::FleetResult { request: 17, data, error: None })
    else {
        panic!("result")
    };
    let session = host_id(4, 2);
    let job = host_id(4, 3);
    for value in [
        &data["attention"]["messages"][0]["session"],
        &data["attention"]["supervisor"]["audit"][0]["session"],
        &data["supervisor"]["audit"][0]["session"],
        &data["message"]["session"],
        &data["messages"][0]["session"],
        &data["decision"]["recipients"][0],
        &data["targets"][0]["session"],
        &data["decision"]["targets"][0]["session"],
        &data["recipients"][0],
        &data["recipients"][1]["session"],
    ] {
        assert_eq!(value.as_u64(), Some(session));
    }
    assert_eq!(data["attention"]["queued"][session.to_string()], 4);
    assert_eq!(data["job"]["id"], job);
    assert_eq!(data["jobs"][0]["id"], job);
    assert_eq!(data["message"]["host"], "peer");
    assert_eq!(data["attention"]["messages"][0]["host"], "peer");
    assert_eq!(data["targets"][0]["pid"], 45);
    assert_eq!(data["targets"][0]["spawned_ms"], 99);
    assert_eq!(data["key"], "opaque-key");
    assert_eq!(data["message"]["id"], "uuid");
    assert_eq!(request, 17);
}

#[test]
fn fleet_status_merges_peer_attention_and_audit_and_excludes_offline_cache() {
    let dir = TempDir::new("attention-status-fleet");
    let mut svc = service(&dir);
    let _rx = peer(&mut svc, "one", 1, true, 15);
    let state = DeckState { attention: fixture(), attention_capable: true, ..Default::default() };
    svc.on_host_msg(1, ServerMsg::State(state));
    let snapshot = svc.attention_fleet_snapshot();
    assert_eq!(snapshot.supervisor.audit[0].session, host_id(1, 2));
    assert_eq!(snapshot.queued.get(&host_id(1, 2)), Some(&4));
    assert_eq!(snapshot.messages[0].host.as_deref(), Some("one"));
    assert_eq!(snapshot, svc.build_state().attention);
    assert!(svc.attention_snapshot().messages.is_empty(), "local snapshot must not recurse");
    svc.hosts.get_mut(&1).unwrap().out = None;
    assert!(svc.attention_fleet_snapshot().messages.is_empty());
    assert!(svc.attention_fleet_snapshot().supervisor.audit.is_empty());
}

#[test]
fn registered_repository_discovery_and_send_pin_exact_live_authenticated_mapping() {
    let dir = TempDir::new("attention-repository-peer");
    let mut svc = service(&dir);
    let rx = peer(&mut svc, "eligible", 1, true, 15);
    let peers = svc.attention_repository_peers(dir.path()).unwrap();
    assert_eq!(peers.len(), 1);
    svc.scheduler.error = Some("fixture recovery required".into());
    assert!(svc.attention_repository_peers(dir.path()).is_err());
    svc.scheduler.error = None;
    assert!(svc.attention_repository_peers(&dir.path().join("missing")).is_err());
    let binding = &peers[0];
    assert_eq!(binding.host.name, "eligible");
    assert_eq!(binding.global_session(9).unwrap(), host_id(1, 9));
    assert!(binding.global_session(host_id(2, 9)).is_err());
    assert!(binding.global_session(0).is_err());
    for case in ["offline", "old", "stale", "unavailable", "unknown"] {
        let _other = peer(&mut svc, case, 2, case != "offline", if case == "old" { 14 } else { 15 });
        match case {
            "stale" => svc.hosts.get_mut(&2).unwrap().state.as_mut().unwrap().scheduler.headroom.at = 0,
            "unavailable" => svc.hosts.get_mut(&2).unwrap().state.as_mut().unwrap().attention_capable = false,
            "unknown" => svc.hosts.get_mut(&2).unwrap().state = None,
            _ => {}
        }
        assert!(svc.attention_repository_peers(dir.path()).is_err(), "unavailable registration cannot mean empty");
        svc.hosts.remove(&2);
    }
    let _wrong = peer(&mut svc, "unrelated", 2, true, 15);
    svc.hosts.get_mut(&2).unwrap().state.as_mut().unwrap().scheduler.repositories.clear();
    assert_eq!(svc.attention_repository_peers(dir.path()).unwrap().len(), 1);
    let unregistered = TempDir::new("attention-unregistered");
    assert!(svc.attention_repository_peers(unregistered.path()).unwrap().is_empty());
    let request = (1u64 << 60) | 1;
    svc.attention_peer_send(binding, request, "attention-recipients", json!({"repository":"shared"})).unwrap();
    let (id, action, args) = fleet(&rx.try_recv().unwrap());
    assert_eq!(id, request);
    assert_eq!(action, "attention-recipients");
    assert_eq!(args, json!({"repository":"shared"}));
    assert!(svc.attention_peer_send(binding, request, "attention-send", json!({"repository":"other"})).is_err());
    svc.hosts.get_mut(&1).unwrap().config.fingerprint = "b".repeat(64);
    assert!(svc.attention_peer_send(binding, request, "attention-recipients", json!({"repository":"shared"})).is_err());
    svc.hosts.get_mut(&1).unwrap().config = binding.host.clone();
    svc.hosts
        .get_mut(&1)
        .unwrap()
        .state
        .as_mut()
        .unwrap()
        .scheduler
        .repositories
        .insert("shared".into(), "/srv/replaced".into());
    assert!(svc.attention_peer_send(binding, request, "attention-recipients", json!({"repository":"shared"})).is_err());
    assert!(rx.try_recv().is_err());
}

#[test]
fn fleet_off_retries_have_bounded_stable_ids_and_only_exact_host_generation_acknowledges() {
    let dir = TempDir::new("attention-off-bounded");
    let mut svc = service(&dir);
    let rx = peer(&mut svc, "one", 1, true, 15);
    let _offline = peer(&mut svc, "offline", 2, false, 15);
    svc.attention.as_mut().unwrap().store.saved.supervisor.revision = 1;
    svc.attention_fleet_off().unwrap();
    let (request, _, args) = fleet(&rx.try_recv().unwrap());
    for _ in 0..100 {
        svc.attention.as_mut().unwrap().rpc_last = None;
        svc.attention_sync_fleet_off();
        assert_eq!(fleet(&rx.try_recv().unwrap()).0, request);
        assert_eq!(svc.attention.as_ref().unwrap().pending_rpc.len(), 1);
    }
    let ack = json!({"supervisor":{"enabled":false},"off":{"origin":args["origin"],"generation":1}});
    assert!(!svc.attention_peer_result("offline", request, &ack, None));
    assert_eq!(svc.attention.as_ref().unwrap().pending_rpc.len(), 1);
    for wrong in [
        json!({"supervisor":{"enabled":false}}),
        json!({"supervisor":{"enabled":true},"off":ack["off"]}),
        json!({"supervisor":{"enabled":false},"off":{"origin":args["origin"],"generation":2}}),
    ] {
        assert!(svc.attention_peer_result("one", request, &wrong, None));
        assert_eq!(svc.attention.as_ref().unwrap().store.saved.supervisor.pending.len(), 2);
    }
    assert!(svc.attention_peer_result("one", request, &ack, None));
    assert_eq!(svc.attention.as_ref().unwrap().store.saved.supervisor.pending, vec!["offline"]);
    assert!(svc.attention.as_ref().unwrap().pending_rpc.is_empty());
    svc.attention_fleet_off().unwrap();
    assert!(rx.try_recv().is_err(), "same-generation replay cannot reset a confirmed peer");
    assert_eq!(svc.attention.as_ref().unwrap().store.saved.supervisor.pending, vec!["offline"]);
    svc.attention.as_mut().unwrap().store.saved.supervisor.revision = 0;
    assert!(svc.attention_fleet_off().is_err(), "an older revision cannot replace a newer journal");
}

#[test]
fn fleet_off_restart_reuses_identity_and_old_ack_cannot_clear_newer_batch_or_revision() {
    let dir = TempDir::new("attention-off-generation");
    let mut svc = service(&dir);
    let rx = peer(&mut svc, "one", 1, true, 15);
    svc.attention.as_mut().unwrap().store.saved.supervisor.revision = 1;
    svc.attention_fleet_off().unwrap();
    let (old, _, oldargs) = fleet(&rx.try_recv().unwrap());
    drop(svc);
    let mut svc = service(&dir);
    let rx = peer(&mut svc, "one", 1, true, 15);
    svc.attention_sync_fleet_off();
    assert_eq!(fleet(&rx.try_recv().unwrap()).0, old);
    svc.attention.as_mut().unwrap().store.saved.supervisor.revision = 2;
    svc.attention_fleet_off().unwrap();
    let (new, _, args) = fleet(&rx.try_recv().unwrap());
    assert_ne!(new, old);
    let oldack = json!({"supervisor":{"enabled":false},"off":{"origin":oldargs["origin"],"generation":1}});
    assert!(!svc.attention_peer_result("one", old, &oldack, None));
    assert_eq!(svc.attention.as_ref().unwrap().store.saved.supervisor.pending, vec!["one"]);
    svc.attention.as_mut().unwrap().store.saved.supervisor.revision = 3;
    svc.attention.as_mut().unwrap().rpc_last = None;
    svc.attention_sync_fleet_off();
    assert!(rx.try_recv().is_err());
    assert!(svc.attention.as_ref().unwrap().pending_rpc.is_empty());
    let ack = json!({"supervisor":{"enabled":false},"off":{"origin":args["origin"],"generation":2}});
    assert!(!svc.attention_peer_result("one", new, &ack, None));
    assert_eq!(svc.attention.as_ref().unwrap().store.saved.supervisor.pending, vec!["one"]);
}
