use super::*;
use crate::protocol::{host_id, ScanInfo};
use crate::testsupport::TempDir;

fn service(dir: &TempDir) -> Service {
    let (tx, _) = std::sync::mpsc::channel();
    Service::new(crate::service::tests::options(dir, "true"), tx)
}
fn host() -> HostConfig {
    HostConfig {
        role: Default::default(),
        name: "fedora".into(),
        label: Some("Fedora".into()),
        tag: 1,
        addresses: vec![],
        ssh: None,
        fingerprint: String::new(),
    }
}

fn add_host(svc: &mut Service, config: HostConfig) {
    let outage = svc.take_restored_host_outage(&config);
    svc.hosts.insert(
        config.tag,
        HostSlot {
            config,
            stop: Arc::new(AtomicBool::new(false)),
            out: None,
            welcome: None,
            state: None,
            reason: outage.reason.clone().or_else(|| Some("connecting".into())),
            stats: None,
            reached: false,
            outage,
        },
    );
}

fn connected(svc: &mut Service, tag: u16, now: Instant) {
    let (out, _) = std::sync::mpsc::channel();
    svc.on_host_at(HostEv::Up { tag, out, welcome: Welcome { version: PROTOCOL_VERSION, ..Welcome::default() } }, now);
}

fn disconnected(svc: &mut Service, tag: u16, now: Instant, reason: &str) {
    svc.on_host_at(HostEv::Down { tag, reason: reason.into() }, now);
}

fn client(svc: &mut Service, cid: u64) -> std::sync::mpsc::Receiver<Out> {
    let (out, rx) = std::sync::mpsc::channel();
    svc.handle(super::super::Ev::ClientNew { id: cid, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    rx
}

fn replies(rx: &std::sync::mpsc::Receiver<Out>) -> Vec<ServerMsg> {
    rx.try_iter()
        .filter_map(|out| match out {
            Out::Msg(bytes) => crate::protocol::read_msg(&mut bytes.as_slice()).unwrap(),
            _ => None,
        })
        .collect()
}

// A deck's scripts load with its first state: with the hosts already there,
// the unmerged-branch count is never made without them (it flashed 15 then
// 51, owner report, 2026-11-08).
#[test]
fn a_deck_gets_the_hosts_before_its_first_state() {
    let dir = TempDir::new("hosts-before-state");
    let mut svc = service(&dir);
    add_host(&mut svc, host());
    let rx = client(&mut svc, 9);
    svc.handle_client(9, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Tui });
    let order: Vec<&str> = replies(&rx)
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Hosts { .. } => Some("hosts"),
            ServerMsg::State(_) => Some("state"),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["hosts", "state"]);
}

#[test]
fn a_build_host_is_online_while_its_stats_arrive_and_starts_nothing() {
    // Owner, 2026-11-08: Ubuntu and WSL are build hosts that only report
    // CPU, RAM and SSD.
    let dir = TempDir::new("build-host");
    let mut svc = service(&dir);
    let rx = client(&mut svc, 9);
    svc.handle_client(9, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Tui });
    add_host(&mut svc, HostConfig { role: crate::config::HostRole::Build, ..host() });
    let info = |svc: &Service| svc.host_infos().into_iter().next().unwrap();
    assert!(!info(&svc).online);
    let now = Instant::now();
    disconnected(&mut svc, 1, now, "connection refused");
    svc.host_outages_due(now + HOST_OFFLINE_GRACE);
    assert!(svc.hosts[&1].outage.reported, "an unreachable build host is reported like any host");
    let stats = HostStats { memory: Some(40.0), ..HostStats::default() };
    svc.on_host_at(HostEv::Stats { tag: 1, stats: stats.clone() }, now);
    let shown = info(&svc);
    assert!(shown.online && shown.reason.is_none());
    assert!(svc.build_state().errors.is_empty(), "a build host's first stats retire its outage error");
    assert_eq!((shown.role, shown.stats), (crate::config::HostRole::Build, Some(stats)));
    assert!(replies(&rx).iter().any(|m| matches!(m, ServerMsg::Notice { text, .. } if text == "Fedora reconnected.")));
    let create = ClientMsg::Create {
        tool: crate::protocol::Tool::Claude,
        model: None,
        effort: None,
        host: Some("fedora".into()),
        cwd: None,
        command: vec![],
        name: None,
        request: None,
    };
    assert!(svc.route_client(9, create).is_none(), "nothing starts on a build host");
    assert!(svc.sessions.is_empty());
    disconnected(&mut svc, 1, now, "connection reset");
    assert!(!info(&svc).online);
}

#[test]
fn native_owner_messages_refuse_old_source_hosts_without_relaying() {
    let dir = TempDir::new("native-message-version");
    let mut svc = service(&dir);
    let rx = client(&mut svc, 9);
    add_host(&mut svc, host());
    connected(&mut svc, 1, Instant::now());
    svc.hosts.get_mut(&1).unwrap().welcome.as_mut().unwrap().version = 18;
    let (out, incoming) = std::sync::mpsc::channel();
    svc.clients.get_mut(&9).unwrap().upstreams.insert(1, out);
    replies(&rx);
    let id = host_id(1, 100);
    assert!(svc
        .route_client(9, ClientMsg::SendMessage { id, request: 402, text: "native only".into(), attachments: false })
        .is_none());
    assert!(svc.route_client(9, ClientMsg::SendNow { id }).is_none());
    assert!(incoming.try_recv().is_err(), "an older source host must receive no owner input");
    let result = replies(&rx);
    assert!(
        matches!(&result[..], [ServerMsg::MessageSent { id: target, request: 402, error: Some(error), .. }, ServerMsg::SentNow { id: now_target, error: Some(now_error) }] if *target == id && *now_target == id && error.contains("protocol 19") && now_error.contains("protocol 19")),
        "{result:?}"
    );
    for (request, name, offline) in [(501, "fedora", false), (502, "fedora", true), (503, "unknown", false)] {
        if offline {
            let slot = svc.hosts.get_mut(&1).unwrap();
            slot.welcome.as_mut().unwrap().version = PROTOCOL_VERSION;
            slot.out = None;
        }
        assert!(svc
            .route_client(
                9,
                ClientMsg::Plugin {
                    request,
                    plugin: "provider-codex".into(),
                    command: "target".into(),
                    args: serde_json::json!({"host":name,"id":100,"operation":"send","text":"native only"})
                }
            )
            .is_none());
        assert!(
            matches!(&replies(&rx)[..], [ServerMsg::PluginResult { request: got, error: Some(_), .. }] if *got == request)
        );
        assert!(incoming.try_recv().is_err());
    }
    svc.hosts.get_mut(&1).unwrap().welcome.as_mut().unwrap().version = 18;
    assert!(svc
        .route_client(9, ClientMsg::Broadcast { ids: vec![id], text: "native only".into(), running_only: false })
        .is_none());
    assert!(incoming.try_recv().is_err(), "legacy owner broadcasts cannot reach an old PTY fallback");
}

#[test]
fn a_fleet_broadcast_reaches_three_hosts_and_waits_for_the_last_delivery() {
    use crate::protocol::{Status, Tool};
    use crate::service::native_message_tests::{fixture, until};
    let mut dirs = Vec::new();
    let mut services = Vec::new();
    let mut events = Vec::new();
    let mut servers = Vec::new();
    for _ in 0..3 {
        let (dir, mut svc, event, _, id, server) = fixture();
        svc.sessions.get_mut(&id).unwrap().tracker.status = Status::Working;
        let idle = svc.create(Tool::Codex, None).unwrap();
        svc.sessions.get_mut(&idle).unwrap().tracker.status = Status::Idle;
        dirs.push(dir);
        services.push(svc);
        events.push(event);
        servers.push(server);
    }
    let receivers: Vec<_> = services.iter_mut().map(|svc| client(svc, 9)).collect();
    let mut channels = Vec::new();
    for tag in 1..=2 {
        let config = HostConfig { role: Default::default(), tag, name: format!("host{tag}"), label: None, ..host() };
        add_host(&mut services[0], config.clone());
        connected(&mut services[0], tag, Instant::now());
        let state = fed::state_from(tag, &config.name, services[tag as usize].build_state());
        // Host 2 has reconnected but has not published its first snapshot yet.
        if tag == 1 {
            services[0].hosts.get_mut(&tag).unwrap().state = Some(state);
        }
        let (out, rx) = std::sync::mpsc::channel();
        services[0].clients.get_mut(&9).unwrap().upstreams.insert(tag, out);
        channels.push(rx);
    }
    replies(&receivers[0]);
    services[0].handle_client(
        9,
        ClientMsg::Broadcast { running_only: true, ids: vec![], text: "Keep your current assignment".into() },
    );
    until(&mut services[0], &events[0], |svc| !svc.messages.active(1));
    assert!(replies(&receivers[0]).is_empty(), "local delivery cannot finish the fleet request");
    for tag in 1..=2 {
        let bytes = channels[tag - 1].try_recv().expect("every remote host gets a request");
        let msg: ClientMsg = crate::protocol::read_msg(&mut bytes.as_slice()).unwrap().unwrap();
        assert!(
            matches!(&msg, ClientMsg::BroadcastPart { ids: None, .. }),
            "the destination selects its current working agents"
        );
        services[tag].handle_client(9, msg);
        until(&mut services[tag], &events[tag], |svc| !svc.messages.active(1));
        assert!(
            !crate::screen::screen_text(&services[tag].sessions[&2].hosted.as_ref().unwrap().lock().term)
                .join("\n")
                .contains("Owner broadcast"),
            "idle agents stay excluded"
        );
        let result = replies(&receivers[tag])
            .into_iter()
            .find(|m| matches!(m, ServerMsg::BroadcastDone { .. }))
            .expect("delivery receipt");
        services[0].on_upstream_msg(9, tag as u16, result);
        if tag == 1 {
            assert!(replies(&receivers[0]).is_empty(), "the first remote reply cannot finish the broadcast");
        }
    }
    let done = replies(&receivers[0]);
    for svc in &services {
        for session in svc.sessions.values() {
            session.hosted.as_ref().unwrap().signal(libc::SIGKILL);
        }
    }
    for dir in &dirs {
        let inbox = std::fs::read_to_string(dir.path().join("inbox")).unwrap();
        let messages: Vec<String> = inbox.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(messages.len(), 1, "each host admits only its working target");
        assert!(messages[0].contains("Keep your current assignment"));
        assert!(!dir.path().join("typed").exists(), "fleet owner messages never write PTY bytes");
    }
    assert!(
        matches!(&done[..], [ServerMsg::Notice { error: false, text }] if text.contains("3 agents across 3 hosts")),
        "{done:?}"
    );
}

#[test]
fn broadcast_reports_old_offline_stale_and_unconfirmed_hosts_together() {
    use crate::protocol::{Status, Tool};
    let dir = TempDir::new("broadcast-partial");
    let mut svc = service(&dir);
    let rx = client(&mut svc, 9);
    let mut requests = Vec::new();
    for tag in 1..=4 {
        add_host(
            &mut svc,
            HostConfig { role: Default::default(), tag, name: format!("host{tag}"), label: None, ..host() },
        );
        if tag == 3 {
            continue;
        }
        connected(&mut svc, tag, Instant::now());
        let slot = svc.hosts.get_mut(&tag).unwrap();
        if tag == 2 {
            slot.welcome.as_mut().unwrap().version = 14;
        }
        slot.state = Some(DeckState {
            sessions: vec![crate::tui::tests::session(host_id(tag, 1), Tool::Codex, Status::Working, true)],
            ..DeckState::default()
        });
        let (out, incoming) = std::sync::mpsc::channel();
        svc.clients.get_mut(&9).unwrap().upstreams.insert(tag, out);
        requests.push((tag, incoming));
    }
    replies(&rx);
    svc.handle_client(9, ClientMsg::Broadcast { running_only: true, ids: vec![], text: "Continue".into() });
    let mut request = 0;
    for (tag, incoming) in requests {
        if tag == 2 {
            assert!(incoming.try_recv().is_err(), "old hosts receive no unsupported request");
            continue;
        }
        let bytes = incoming.try_recv().unwrap();
        let Some(ClientMsg::BroadcastPart { request: token, .. }) =
            crate::protocol::read_msg(&mut bytes.as_slice()).unwrap()
        else {
            panic!()
        };
        request = token;
    }
    svc.on_upstream_msg(9, 1, ServerMsg::BroadcastDone { request: request + 1, sent: vec![1], failed: vec![] });
    assert!(replies(&rx).is_empty(), "unrelated receipts cannot complete this request");
    svc.on_upstream_msg(
        9,
        1,
        ServerMsg::BroadcastDone { request, sent: vec![1], failed: vec![(2, "no longer working".into())] },
    );
    assert!(replies(&rx).is_empty(), "partial failure still waits for other hosts");
    svc.broadcasts_due(Instant::now() + Duration::from_secs(46));
    let done = replies(&rx);
    assert!(
        matches!(&done[..], [ServerMsg::Notice { error: true, text }] if text.contains("1 agent across 1 host") && text.contains("host1") && text.contains("no longer working") && text.contains("host2") && text.contains("update") && text.contains("host3") && text.contains("offline") && text.contains("host4") && text.contains("timed out")),
        "{done:?}"
    );
    // The summary alone, as the footer and phone lead with it, admits the shortfall.
    let [ServerMsg::Notice { text, .. }] = &done[..] else { unreachable!() };
    assert_eq!(
        text.lines().next(),
        Some("Sent to 1 agent across 1 host; 1 agent not delivered; 3 hosts did not confirm delivery")
    );
}

type BroadcastReceipt = (u16, Vec<u64>, Vec<(u64, String)>);

/// One all-hosts broadcast from deck 9, answered by `receipts`.
fn fleet_broadcast(
    svc: &mut Service,
    incoming: &[std::sync::mpsc::Receiver<Vec<u8>>],
    rx: &std::sync::mpsc::Receiver<Out>,
    receipts: Vec<BroadcastReceipt>,
) -> (String, bool) {
    svc.handle_client(9, ClientMsg::Broadcast { running_only: true, ids: vec![], text: "Continue".into() });
    let mut request = None;
    for requests in incoming {
        let bytes = requests.try_recv().expect("every connected host is asked");
        let Some(ClientMsg::BroadcastPart { request: token, ids: None, .. }) =
            crate::protocol::read_msg(&mut bytes.as_slice()).unwrap()
        else {
            panic!("each host selects its own working agents")
        };
        assert!(request.is_none_or(|request| request == token));
        request = Some(token);
    }
    for (tag, sent, failed) in receipts {
        svc.on_upstream_msg(9, tag, ServerMsg::BroadcastDone { request: request.unwrap(), sent, failed });
    }
    match &replies(rx)[..] {
        [ServerMsg::Notice { text, error }] => (text.clone(), *error),
        other => panic!("{other:?}"),
    }
}

#[test]
fn broadcast_results_group_failures_and_leave_agents_that_stopped_working_alone() {
    use crate::protocol::{Status, Tool};
    let dir = TempDir::new("broadcast-grouped");
    let mut svc = service(&dir);
    let rx = client(&mut svc, 9);
    let mut incoming = Vec::new();
    for tag in 1..=3 {
        add_host(&mut svc, HostConfig { tag, name: format!("host{tag}"), label: None, ..host() });
        connected(&mut svc, tag, Instant::now());
        let (out, requests) = std::sync::mpsc::channel();
        svc.clients.get_mut(&9).unwrap().upstreams.insert(tag, out);
        incoming.push(requests);
    }
    let agent = |n: u64| crate::protocol::SessionInfo {
        title: format!("agent {n}"),
        ..crate::tui::tests::session(host_id(1, n), Tool::Codex, Status::Working, true)
    };
    svc.hosts.get_mut(&1).unwrap().state =
        Some(DeckState { sessions: (1..=4).map(agent).collect(), ..DeckState::default() });
    replies(&rx);
    let stopped = "no longer working; queued broadcast was not sent".to_string();

    // An agent that finished between selection and delivery was rightly
    // left alone, and a host with no working agent has nothing to report.
    let outcome = fleet_broadcast(
        &mut svc,
        &incoming,
        &rx,
        vec![(1, vec![1, 2], vec![(3, stopped.clone())]), (2, vec![], vec![]), (3, vec![7], vec![])],
    );
    assert_eq!(
        outcome,
        (
            "Sent to 3 agents across 2 hosts; 1 agent stopped working before delivery and was left alone\n\
             host1: 2 delivered, 1 stopped working first\n\
             host3: 1 delivered"
                .to_string(),
            false
        )
    );

    // Real failures stay errors, grouped by reason under their host. A source
    // connection older than protocol 19 is the host's outcome, not a session's.
    let failed = "native delivery failed; check the conversation before retrying".to_string();
    let old = "Update needed: source connection must support protocol 19 native owner messages; nothing was sent";
    let outcome = fleet_broadcast(
        &mut svc,
        &incoming,
        &rx,
        vec![
            (1, vec![1], vec![(2, failed.clone()), (3, failed), (4, "session is closing".into())]),
            (2, vec![], vec![(0, old.into())]),
            (3, vec![], vec![]),
        ],
    );
    assert_eq!(
        outcome,
        (
            format!(
                "Sent to 1 agent across 1 host; 3 agents not delivered; 1 host did not confirm delivery\n\
                 host1: 1 delivered, 3 not delivered\n  \
                 Codex \u{201c}agent 2\u{201d}, Codex \u{201c}agent 3\u{201d}: native delivery failed; check the \
                 conversation before retrying\n  \
                 Codex \u{201c}agent 4\u{201d}: session is closing\n\
                 host2: {old}."
            ),
            true
        )
    );

    // With nobody working anywhere, the summary alone says so.
    let outcome =
        fleet_broadcast(&mut svc, &incoming, &rx, vec![(1, vec![], vec![]), (2, vec![], vec![]), (3, vec![], vec![])]);
    assert_eq!(outcome, ("Sent to no session; no agent is working on any connected host".to_string(), true));
}

#[test]
fn a_brief_peer_relaunch_updates_status_and_action_feedback_without_an_outage_error() {
    let dir = TempDir::new("peer-relaunch-grace");
    let mut svc = service(&dir);
    add_host(&mut svc, host());
    let start = Instant::now();
    connected(&mut svc, 1, start);
    disconnected(&mut svc, 1, start, "peer restarting");
    assert!(!svc.host_infos()[0].online, "status must reflect the closed channel immediately");
    svc.host_outages_due(start + Duration::from_secs(1));
    assert!(svc.build_state().errors.is_empty(), "brief reconnects must not become durable errors");

    let (out, replies) = std::sync::mpsc::channel();
    svc.handle(super::super::Ev::ClientNew { id: 9, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.upstream_send(9, 1, &ClientMsg::Resize { cols: 90, rows: 24 });
    assert!(
        replies.try_iter().any(|out| match out {
            Out::Msg(bytes) => matches!(
                crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap(),
                Some(ServerMsg::Notice { error: true, text }) if text.contains("offline")
            ),
            _ => false,
        }),
        "a requested action still explains immediately why it cannot run"
    );
    assert_eq!(svc.build_state().errors.len(), 1);

    connected(&mut svc, 1, start + Duration::from_secs(20));
    svc.host_outages_due(start + Duration::from_secs(90));
    assert!(svc.host_infos()[0].online);
    assert!(
        svc.build_state().errors.is_empty(),
        "recovery cancels the pending outage and retires the offline explanation"
    );
}

#[test]
fn a_sustained_startup_loss_reports_from_the_first_failure_once_and_rearms_on_recovery() {
    let dir = TempDir::new("startup-outage-grace");
    let mut svc = service(&dir);
    add_host(&mut svc, host());
    let start = Instant::now();
    disconnected(&mut svc, 1, start, "first failure");
    disconnected(&mut svc, 1, start + Duration::from_secs(40), "still unreachable");
    svc.host_outages_due(start + Duration::from_secs(59));
    assert!(svc.build_state().errors.is_empty());
    svc.host_outages_due(start + Duration::from_secs(61));
    let state = svc.build_state();
    assert_eq!(state.errors.len(), 1, "a retry cannot reset the first-loss deadline");
    assert!(state.errors[0].text.contains("still unreachable"));
    disconnected(&mut svc, 1, start + Duration::from_secs(70), "another retry");
    svc.host_outages_due(start + Duration::from_secs(130));
    assert_eq!(svc.build_state().errors.len(), 1, "one continuous outage produces one error");
    connected(&mut svc, 1, start + Duration::from_secs(131));
    assert!(svc.build_state().errors.is_empty(), "reconnecting retires the outage error");
    disconnected(&mut svc, 1, start + Duration::from_secs(132), "a separate outage");
    svc.host_outages_due(start + Duration::from_secs(193));
    let state = svc.build_state();
    assert_eq!(state.errors.len(), 1, "a later genuine outage must report again");
    assert!(state.errors[0].text.contains("a separate outage"));
}

#[test]
fn hub_handover_keeps_pending_and_reported_outages_while_reconnects_still_cancel_them() {
    let dir = TempDir::new("hub-outage-handover");
    let mut old = service(&dir);
    let configs: Vec<_> = (1..=3)
        .map(|tag| HostConfig { role: Default::default(), tag, name: format!("host{tag}"), label: None, ..host() })
        .collect();
    for config in &configs {
        add_host(&mut old, config.clone());
    }
    let start = Instant::now();
    disconnected(&mut old, 1, start, "reported outage");
    disconnected(&mut old, 2, start + Duration::from_secs(40), "pending outage");
    disconnected(&mut old, 3, start + Duration::from_secs(40), "transient reconnect");
    old.host_outages_due(start + Duration::from_secs(61));
    assert_eq!(old.build_state().errors.len(), 1);
    let handover = old
        .prepare_handover_at(super::super::Fds { listener: 100, lock: 101 }, start + Duration::from_secs(61))
        .unwrap();
    let bytes = serde_json::to_vec(&handover).unwrap();
    let mut new = service(&dir);
    new.adopt_at(serde_json::from_slice(&bytes).unwrap(), start + Duration::from_secs(61));
    for config in &configs {
        add_host(&mut new, config.clone());
    }
    connected(&mut new, 3, start + Duration::from_secs(70));
    new.host_outages_due(start + Duration::from_secs(99));
    assert_eq!(new.build_state().errors.len(), 1);
    new.host_outages_due(start + Duration::from_secs(101));
    assert_eq!(new.build_state().errors.len(), 2, "handover retains the pending outage age");
    new.host_outages_due(start + Duration::from_secs(150));
    assert_eq!(new.build_state().errors.len(), 2, "the already-reported outage stays deduplicated");
    assert!(new.host_infos().iter().find(|h| h.name == "host3").unwrap().online);
    connected(&mut new, 1, start + Duration::from_secs(151));
    let left: Vec<String> = new.build_state().errors.into_iter().map(|e| e.text).collect();
    assert!(
        matches!(&left[..], [only] if only.starts_with("host2 is unreachable")),
        "reconnecting retires the error the earlier build reported and keeps the other host's: {left:?}"
    );
    disconnected(&mut new, 1, start + Duration::from_secs(152), "new loss after recovery");
    new.host_outages_due(start + Duration::from_secs(213));
    assert_eq!(new.build_state().errors.len(), 2, "restored outages rearm after recovery");
}

/// Every deck sorting on its own sends the order it computed; the hub keeps
/// live sessions of every host, drops ended ones and saves only a change.
#[test]
fn a_session_order_keeps_live_remote_ids_and_an_unchanged_order_is_not_saved_again() {
    use crate::protocol::{Status, Tool, ORDER_SAVED_NOTICE};
    let dir = TempDir::new("federated-order");
    let mut svc = service(&dir);
    svc.sessions.insert(1, crate::service::tests::fake_session(1, Tool::Claude));
    add_host(&mut svc, host());
    connected(&mut svc, 1, Instant::now());
    let (live, ended) = (host_id(1, 5), host_id(1, 6));
    svc.hosts.get_mut(&1).unwrap().state = Some(DeckState {
        sessions: vec![
            crate::tui::tests::session(live, Tool::Codex, Status::Idle, true),
            crate::tui::tests::session(ended, Tool::Codex, Status::Ended, false),
        ],
        ..DeckState::default()
    });
    let rx = client(&mut svc, 9);
    svc.handle_client(9, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Tui });
    replies(&rx);
    let saved = |messages: &[ServerMsg]| {
        messages.iter().any(|m| matches!(m, ServerMsg::Notice { text, error: false } if text == ORDER_SAVED_NOTICE))
    };

    svc.handle_client(9, ClientMsg::SetOrder { ids: vec![live, 1, ended] });
    let messages = replies(&rx);
    assert!(saved(&messages), "{messages:?}");
    // A protocol-20 view gets the change as a patch of its state.
    assert!(messages
        .iter()
        .any(|m| matches!(m, ServerMsg::StatePatch(p) if p.session_order.as_deref() == Some(&[live, 1][..]))));
    assert_eq!(svc.saved_session_order(), [live, 1], "the ended remote session is dropped");

    let file = dir.path().join("state/order.json");
    let spaced = format!("[{live}, 1]");
    crate::util::write_atomic(&file, spaced.as_bytes()).unwrap();
    svc.handle_client(9, ClientMsg::SetOrder { ids: vec![live, 1] });
    let messages = replies(&rx);
    assert!(saved(&messages), "the phone still has its reply: {messages:?}");
    assert!(
        !messages.iter().any(|m| matches!(m, ServerMsg::State(_) | ServerMsg::StatePatch(_))),
        "nothing changed to send"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), spaced, "nor to write");
}

#[test]
fn a_remote_scan_is_merged_with_its_host_and_encoded_session_id() {
    let dir = TempDir::new("federated-scans");
    let mut svc = service(&dir);
    let (out, _requests) = std::sync::mpsc::channel();
    svc.hosts.insert(
        1,
        HostSlot {
            config: host(),
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: PROTOCOL_VERSION, ..Welcome::default() }),
            state: None,
            reason: None,
            stats: None,
            outage: HostOutage::default(),
            reached: false,
        },
    );
    let scan = ScanInfo {
        id: "saved".into(),
        session: Some(7),
        started_at: 100,
        state: "running".into(),
        ..ScanInfo::default()
    };
    svc.on_host(HostEv::Msg { tag: 1, msg: ServerMsg::State(DeckState { scans: vec![scan], ..DeckState::default() }) });
    let state = svc.build_state();
    assert_eq!(state.scans.len(), 1);
    assert_eq!(state.scans[0].host.as_deref(), Some("fedora"));
    assert_eq!(state.scans[0].session, Some(host_id(1, 7)));
    svc.on_host(HostEv::Down { tag: 1, reason: "gone".into() });
    assert!(svc.build_state().scans.is_empty(), "offline hosts must not retain live scan sessions");
}

#[test]
fn targeted_terminal_resize_routes_without_attaching_and_refuses_old_hosts() {
    let dir = TempDir::new("federated-resize");
    let mut svc = service(&dir);
    let (tx, rx) = std::sync::mpsc::channel();
    let (up, requests) = std::sync::mpsc::channel();
    svc.handle(super::super::Ev::ClientNew { id: 9, out: tx, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(9, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
    svc.clients.get_mut(&9).unwrap().upstreams.insert(1, up);
    let (out, _) = std::sync::mpsc::channel();
    svc.hosts.insert(
        1,
        HostSlot {
            config: host(),
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: 10, ..Welcome::default() }),
            state: None,
            reason: None,
            stats: None,
            outage: HostOutage::default(),
            reached: false,
        },
    );
    assert!(svc.route_client(9, ClientMsg::ResizeTerminal { id: host_id(1, 7), cols: 90, rows: 24 }).is_none());
    let msg = crate::protocol::read_msg::<_, ClientMsg>(&mut requests.recv().unwrap().as_slice()).unwrap().unwrap();
    assert_eq!(msg, ClientMsg::ResizeTerminal { id: 7, cols: 90, rows: 24 });
    assert_eq!(svc.clients[&9].attached, None);
    svc.hosts.get_mut(&1).unwrap().welcome.as_mut().unwrap().version = 9;
    svc.route_client(9, ClientMsg::ResizeTerminal { id: host_id(1, 7), cols: 91, rows: 25 });
    assert!(requests.try_recv().is_err());
    assert!(rx.try_iter().any(|m| match m {
        Out::Msg(bytes) => matches!(
            crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap(),
            Some(ServerMsg::Notice { error: true, .. })
        ),
        _ => false,
    }));
}

#[test]
fn clearing_errors_removes_cached_peer_errors_before_the_peer_answers() {
    let dir = TempDir::new("clear-peer-errors");
    let mut svc = service(&dir);
    let (out, _) = std::sync::mpsc::channel();
    svc.hosts.insert(
        1,
        HostSlot {
            config: host(),
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: PROTOCOL_VERSION, ..Welcome::default() }),
            state: Some(DeckState {
                errors: vec![crate::protocol::LogEntry::new(10, "old peer error")],
                ..DeckState::default()
            }),
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    assert_eq!(svc.build_state().errors.len(), 1);
    svc.handle_client(0, ClientMsg::ClearErrors);
    assert!(svc.build_state().errors.is_empty(), "Clear must remove cached peer errors immediately");
}

#[test]
fn cleared_peer_snapshots_stay_cleared_and_new_same_second_errors_are_kept() {
    let dir = TempDir::new("clear-peer-replay");
    let mut svc = service(&dir);
    let (out, _) = std::sync::mpsc::channel();
    svc.hosts.insert(
        1,
        HostSlot {
            config: host(),
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: PROTOCOL_VERSION, ..Welcome::default() }),
            state: None,
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    let old = DeckState { errors: vec![crate::protocol::LogEntry::new(10, "peer error")], ..DeckState::default() };
    svc.on_host(HostEv::Msg { tag: 1, msg: ServerMsg::State(old.clone()) });
    svc.handle_client(0, ClientMsg::ClearErrors);
    svc.on_host(HostEv::Msg { tag: 1, msg: ServerMsg::State(old.clone()) });
    assert!(svc.build_state().errors.is_empty(), "an in-flight snapshot must not resurrect cleared errors");
    let mut fresh = old;
    fresh.error_clears = svc.error_clears.clone();
    svc.on_host(HostEv::Msg { tag: 1, msg: ServerMsg::State(fresh) });
    assert_eq!(svc.build_state().errors.len(), 1, "a new occurrence after Clear must remain visible");
    let applied = svc.error_clears.clone();
    svc.handle_client(0, ClientMsg::ClearErrorsThrough { clears: applied });
    assert_eq!(svc.build_state().errors.len(), 1, "echoed clear events must not clear new errors again");
}

#[test]
fn clear_survives_restart_and_reaches_a_host_that_was_offline() {
    let dir = TempDir::new("clear-offline-peer");
    let mut svc = service(&dir);
    svc.handle_client(0, ClientMsg::ClearErrors);
    let applied = svc.error_clears.clone();
    drop(svc);
    let mut svc = service(&dir);
    assert_eq!(svc.error_clears, applied);
    svc.hosts.insert(
        1,
        HostSlot {
            config: host(),
            stop: Arc::new(AtomicBool::new(false)),
            out: None,
            welcome: None,
            state: None,
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    let (out, requests) = std::sync::mpsc::channel();
    svc.on_host(HostEv::Up { tag: 1, out, welcome: Welcome { version: PROTOCOL_VERSION, ..Welcome::default() } });
    let message = crate::protocol::read_msg::<_, ClientMsg>(&mut requests.recv().unwrap().as_slice()).unwrap().unwrap();
    assert_eq!(message, ClientMsg::ClearErrorsThrough { clears: applied });
    for version in [4, 11, 12, 13] {
        svc.hosts.get_mut(&1).unwrap().welcome.as_mut().unwrap().version = version;
        svc.sync_error_clear(1);
        let message =
            crate::protocol::read_msg::<_, ClientMsg>(&mut requests.recv().unwrap().as_slice()).unwrap().unwrap();
        assert_eq!(message, ClientMsg::ClearErrors, "protocol {version} uses the legacy clear request");
    }
}

#[test]
fn a_clear_on_a_peer_propagates_without_echoing_or_hiding_legacy_errors() {
    let dir = TempDir::new("clear-from-peer");
    let mut svc = service(&dir);
    let (out, requests) = std::sync::mpsc::channel();
    svc.hosts.insert(
        1,
        HostSlot {
            config: host(),
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: 11, ..Welcome::default() }),
            state: None,
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    svc.log_error("old local error");
    let clear = [("a".repeat(64), 1)].into_iter().collect();
    svc.on_host(HostEv::Msg {
        tag: 1,
        msg: ServerMsg::State(DeckState { error_clears: clear, ..DeckState::default() }),
    });
    assert!(svc.build_state().errors.is_empty());
    let message = crate::protocol::read_msg::<_, ClientMsg>(&mut requests.recv().unwrap().as_slice()).unwrap().unwrap();
    assert_eq!(message, ClientMsg::ClearErrors);
    svc.on_host(HostEv::Msg {
        tag: 1,
        msg: ServerMsg::State(DeckState {
            errors: vec![crate::protocol::LogEntry::new(11, "new legacy error")],
            ..DeckState::default()
        }),
    });
    assert_eq!(svc.build_state().errors.len(), 1);
}

#[test]
fn clear_errors_suppresses_old_peer_alerts_until_the_clear_is_applied() {
    use crate::protocol::{Alert, AlertKind};
    let dir = TempDir::new("clear-peer-alerts");
    let mut svc = service(&dir);
    let (client, replies) = std::sync::mpsc::channel();
    svc.handle(super::super::Ev::ClientNew { id: 1, out: client, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(1, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Remote });
    let (out, _) = std::sync::mpsc::channel();
    svc.hosts.insert(
        1,
        HostSlot {
            config: host(),
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: PROTOCOL_VERSION, ..Welcome::default() }),
            state: Some(DeckState::default()),
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    svc.handle_client(0, ClientMsg::ClearErrors);
    replies.try_iter().for_each(drop);
    let alert = |reason: &str| {
        ServerMsg::Alert(Alert {
            kind: AlertKind::Error,
            id: None,
            title: "peer error".into(),
            reason: reason.into(),
            desk_focused: false,
            message: None,
        })
    };
    svc.on_host(HostEv::Msg { tag: 1, msg: alert("old in-flight error") });
    assert!(replies.try_recv().is_err(), "old error notifications must not replay after Clear");
    svc.on_host(HostEv::Msg {
        tag: 1,
        msg: ServerMsg::State(DeckState { error_clears: svc.error_clears.clone(), ..DeckState::default() }),
    });
    replies.try_iter().for_each(drop);
    svc.on_host(HostEv::Msg { tag: 1, msg: alert("new error") });
    assert!(
        replies.try_iter().any(|out| match out {
            Out::Msg(bytes) => matches!(crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap(),
            Some(ServerMsg::Alert(alert)) if alert.reason == "new error"),
            _ => false,
        }),
        "new error notifications must still reach phones"
    );
}

#[test]
fn job_routes_fail_offline_disconnect_send_error_and_timeout_without_leaking_capacity() {
    let dir = TempDir::new("job-route-recovery");
    let mut svc = service(&dir);
    let (tx, rx) = std::sync::mpsc::channel();
    svc.handle(super::super::Ev::ClientNew { id: 9, out: tx, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(9, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
    svc.hosts.insert(
        1,
        HostSlot {
            config: host(),
            stop: Arc::new(AtomicBool::new(false)),
            out: None,
            welcome: Some(Welcome { version: 14, ..Welcome::default() }),
            state: None,
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    let request =
        |request| ClientMsg::Fleet { request, action: "job-log".into(), args: serde_json::json!({"job":host_id(1,7)}) };
    for id in 1..4100 {
        assert!(svc.route_client(9, request(id)).is_none());
    }
    assert!(svc.job_routes.is_empty(), "offline polls never reserve response slots");
    let (out, _host_rx) = std::sync::mpsc::channel();
    svc.hosts.get_mut(&1).unwrap().out = Some(out);
    let (up, _requests) = std::sync::mpsc::channel();
    svc.clients.get_mut(&9).unwrap().upstreams.insert(1, up);
    svc.route_client(9, request(5000));
    assert_eq!(svc.job_routes.len(), 1);
    svc.on_host(HostEv::UpGone { cid: 9, tag: 1, reason: "test disconnect".into() });
    assert!(svc.job_routes.is_empty());
    let (up, requests) = std::sync::mpsc::channel();
    svc.clients.get_mut(&9).unwrap().upstreams.insert(1, up);
    svc.route_client(9, request(5001));
    assert_eq!(svc.job_routes.len(), 1);
    svc.job_routes.get_mut(&(9, 5001)).unwrap().at = Instant::now() - Duration::from_secs(31);
    svc.expire_job_routes();
    assert!(svc.job_routes.is_empty());
    drop(requests);
    svc.route_client(9, request(5002));
    assert!(svc.job_routes.is_empty(), "failed upstream send releases its exact pending routes");
    let errors: Vec<_> = rx
        .try_iter()
        .filter_map(|out| {
            if let Out::Msg(bytes) = out {
                crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).ok().flatten()
            } else {
                None
            }
        })
        .filter_map(
            |msg| if let ServerMsg::FleetResult { request, error: Some(_), .. } = msg { Some(request) } else { None },
        )
        .collect();
    for request in [1, 4099, 5000, 5001, 5002] {
        assert!(errors.contains(&request), "matching error response for {request}");
    }
}

#[test]
fn job_remote_stop_is_forwarded_during_shutdown_even_after_unknown_failure() {
    use crate::job_sessions::{Executor, Spec};
    let dir = TempDir::new("job-stop-shutdown");
    let mut svc = service(&dir);
    let (out, commands) = std::sync::mpsc::channel();
    svc.hosts.insert(
        1,
        HostSlot {
            config: host(),
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: 14, ..Welcome::default() }),
            state: None,
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    let jobs = svc.job_sessions.as_mut().unwrap();
    let info = jobs
        .add(
            "remote-stop".into(),
            "test".into(),
            Spec { argv: vec!["true".into()], cwd: dir.path().into(), ..Spec::default() },
            "fedora".into(),
            None,
        )
        .unwrap();
    let record = jobs.records.get_mut(&info.id).unwrap();
    record.executor = Some(Executor { host: Some("fedora".into()), cwd: dir.path().into(), repository: None });
    record.info.status = "failed".into();
    record.info.reason = "supervisor lost; outcome unknown".into();
    svc.begin_shutdown(true);
    assert!(svc.job_sessions.as_ref().unwrap().busy());
    svc.jobs_tick();
    let mut stops = 0;
    for bytes in commands.try_iter() {
        if let Some(ClientMsg::Fleet { request, action, args }) =
            crate::protocol::read_msg::<_, ClientMsg>(&mut bytes.as_slice()).unwrap()
        {
            assert_eq!(action, "job-stop");
            assert_eq!(args["key"], "remote-stop");
            assert_ne!(request & (1u64 << 63), 0);
            stops += 1;
        }
    }
    assert_eq!(stops, 1, "explicit cancellation outranks shutdown and terminal display status");
    svc.job_peer_result("fedora",info.id|(1u64<<63),&serde_json::json!({"cancelled":true,"pending":false,"job":svc.job_sessions.as_ref().unwrap().records[&info.id].info}),None);
    assert!(!svc.job_sessions.as_ref().unwrap().busy(), "shutdown acknowledges exact remote cancellation");
}

#[test]
fn job_remote_acceptance_stops_resubmission_after_reload_but_keeps_progress_and_stop() {
    use crate::job_sessions::{self, Executor, Spec};
    let dir = TempDir::new("job-remote-accepted");
    let mut svc = service(&dir);
    let (out, commands) = std::sync::mpsc::channel();
    svc.hosts.insert(
        1,
        HostSlot {
            config: host(),
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: 14, ..Welcome::default() }),
            state: None,
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    let jobs = svc.job_sessions.as_mut().unwrap();
    let info = jobs
        .add(
            "immutable-remote".into(),
            "test".into(),
            Spec { argv: vec!["true".into()], cwd: dir.path().into(), ..Spec::default() },
            "fedora".into(),
            None,
        )
        .unwrap();
    let executor = Executor { host: Some("fedora".into()), cwd: dir.path().into(), repository: None };
    job_sessions::store::write(&jobs.dir(info.id).join("executor.json"), &executor).unwrap();
    jobs.records.get_mut(&info.id).unwrap().executor = Some(executor);
    let mut submissions = Vec::new();
    for _ in 0..2 {
        let jobs = svc.job_sessions.as_mut().unwrap();
        jobs.last_poll = 0;
        jobs.records.get_mut(&info.id).unwrap().attempted_at = 0;
        svc.jobs_tick();
        let bytes = commands.try_recv().expect("lost replies retry the immutable submission");
        let request = crate::protocol::read_msg::<_, ClientMsg>(&mut bytes.as_slice()).unwrap().unwrap();
        if let ClientMsg::Fleet { action, args, .. } = request {
            assert_eq!(action, "job-start");
            assert_eq!(args["key"], "immutable-remote");
            submissions.push(args);
        } else {
            panic!("expected job submission")
        }
    }
    assert_eq!(submissions[0], submissions[1]);
    assert!(!svc.job_sessions.as_ref().unwrap().remote_accepted(info.id).unwrap());
    svc.job_peer_result("foreign-host", info.id | (1u64 << 63), &serde_json::json!({"job":info}), None);
    assert!(!svc.job_sessions.as_ref().unwrap().remote_accepted(info.id).unwrap());
    svc.job_peer_result("fedora", info.id | (1u64 << 63), &serde_json::json!({"job":info}), None);
    svc.job_sessions = Some(job_sessions::Jobs::load(&svc.opts.state_dir).unwrap());
    assert!(svc.job_sessions.as_ref().unwrap().remote_accepted(info.id).unwrap());
    svc.jobs_tick();
    assert!(commands.try_recv().is_err(), "accepted queued work is never resubmitted after restart");
    let mut finished = info.clone();
    finished.status = "finished".into();
    finished.exit_code = Some(0);
    svc.hosts.get_mut(&1).unwrap().state = Some(DeckState { jobs: vec![finished], ..DeckState::default() });
    svc.job_sessions.as_mut().unwrap().last_poll = 0;
    svc.jobs_tick();
    assert_eq!(svc.job_sessions.as_ref().unwrap().records[&info.id].info.exit_code, Some(0));
    svc.job_sessions.as_mut().unwrap().stop(info.id).unwrap();
    svc.job_sessions.as_mut().unwrap().last_poll = 0;
    svc.job_sessions.as_mut().unwrap().records.get_mut(&info.id).unwrap().attempted_at = 0;
    svc.jobs_tick();
    let bytes = commands.try_recv().expect("acceptance never suppresses explicit cancellation");
    assert!(
        matches!(crate::protocol::read_msg::<_,ClientMsg>(&mut bytes.as_slice()).unwrap().unwrap(), ClientMsg::Fleet{action,..} if action=="job-stop")
    );
}
