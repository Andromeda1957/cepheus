//! A paired phone exercises controls against an isolated real service.
#[path = "remote_stop.rs"]
mod stop;

use super::*;
use crate::protocol::{BranchInfo, DeckState, HostInfo, HostStats, SessionInfo, Status, Tool};

fn control(ws: &mut PhoneWs, rid: u64, action: &str, mut args: Value) -> Value {
    args["t"] = json!("control");
    args["rid"] = json!(rid);
    args["action"] = json!(action);
    send(ws, args);
    reply(ws, rid)
}

#[test]
fn a_paired_phone_uses_desktop_controls() {
    // Installer and skill-sync deliberately exercise their real per-user roots.
    // Run this test with a disposable home, never a real account's home.
    assert_eq!(
        std::env::var("CEPHEUS_TEST_DISPOSABLE_HOME").as_deref(),
        Ok("1"),
        "run with a disposable HOME and CEPHEUS_TEST_DISPOSABLE_HOME=1"
    );
    fs::create_dir_all(crate::paths::home().join(".claude")).unwrap();
    let dir = TempDir::new("phone-controls-e2e");
    let script =
        r"printf '\033[?2004hready\nthinking (esc to interrupt)\n'; while IFS= read -r x; do echo received:$x; done";
    fs::write(dir.path().join("README.md"), "fixture repository\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "README.md"],
        vec!["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-q", "-m", "fixture"],
    ] {
        let output = std::process::Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    let mut opts = service_options(&dir, script);
    opts.config.scans_dir = Some(dir.path().join("scans"));
    opts.config.shell_command = vec!["sh".into()];
    let socket = opts.socket.clone();
    let service = thread::spawn(move || crate::service::run(opts));
    let mut hub_opts = hub_options(&dir, vec![PHONE]);
    let config_file = dir.path().join("config.json");
    fs::write(
        &config_file,
        json!({"scans_dir":dir.path().join("scans"),"scheduler":{"probe":{"root":dir.path().join("proc")}}})
            .to_string(),
    )
    .unwrap();
    hub_opts.config_file = Some(config_file);
    let identity = store::load_or_create_identity(&hub_opts.dir, "127.0.0.1").unwrap();
    let pin = identity.fingerprint.clone();
    let tls = server::tls_config(&identity.cert_der, &identity.key_der).unwrap();
    store::write_pairing(&hub_opts.dir, "ABCD2345", util::now_secs()).unwrap();
    let hub = Arc::new(Hub::new(hub_opts, "controls".into()));
    let bridge = Arc::clone(&hub);
    thread::spawn(move || bridge.bridge());
    let addr = listen(&hub, &tls);
    let mut ws = phone(addr, &pin, "/v1").unwrap();
    send(&mut ws, json!({"t":"pair","code":"ABCD2345","name":"fixture phone"}));
    expect(&mut ws, "paired", |v| v["t"] == "paired");
    expect(&mut ws, "service state", |v| v["t"] == "state");
    let overview = control(&mut ws, 1, "overview", json!({}));
    assert_eq!(overview["t"], "control", "{overview}");
    assert!(overview["data"]["capabilities"].as_array().unwrap().contains(&json!("terminal")));
    let terminal = control(
        &mut ws,
        2,
        "terminal",
        json!({"cwd":dir.path(),"name":"Phone terminal","command":["sh","-c","echo terminal-ready:$PWD; while IFS= read -r x; do echo typed:$x; done"]}),
    );
    assert_eq!(terminal["t"], "control", "{terminal}");
    let tid = terminal["data"]["id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let overview = control(&mut ws, 30, "overview", json!({}));
        if overview["data"]["terminals"].as_array().is_some_and(|ss| ss.iter().any(|s| s["id"] == tid)) {
            break;
        }
        assert!(Instant::now() < deadline, "terminal did not enter overview");
        thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(control(&mut ws, 3, "terminal_input", json!({"id":tid,"text":"hello literal"}))["t"], "control");
    assert_eq!(control(&mut ws, 4, "terminal_input", json!({"id":tid,"key":"enter"}))["t"], "control");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let screen = control(&mut ws, 5, "terminal_screen", json!({"id":tid}));
        assert_eq!(screen["t"], "control", "{screen}");
        if screen["data"]["text"].as_str().unwrap().contains("typed:hello literal") {
            break;
        }
        assert!(Instant::now() < deadline, "terminal did not receive literal text: {screen}");
        thread::sleep(Duration::from_millis(40));
    }
    assert_eq!(control(&mut ws, 6, "terminal_resize", json!({"id":tid,"cols":90,"rows":24}))["t"], "control");

    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.screen(tid).unwrap().0.len() != 24 {
        assert!(Instant::now() < deadline, "terminal did not resize");
        thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(control(&mut ws, 7, "rename", json!({"id":tid,"name":"Renamed terminal"}))["t"], "control");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = control(&mut ws, 37, "terminal_screen", json!({"id":tid}));
        if screen["data"]["title"] == "Renamed terminal" {
            break;
        }
        assert!(Instant::now() < deadline, "renamed title not delivered: {screen}");
        thread::sleep(Duration::from_millis(30));
    }

    let overview = control(&mut ws, 8, "overview", json!({}));
    assert_eq!(overview["data"]["terminals"][0]["id"], tid);
    for (rid, action, args) in [
        (10, "terminal_input", json!({"id":tid,"text":"\u{1b}[31m"})),
        (11, "terminal_input", json!({"id":tid,"key":"unknown"})),
        (12, "terminal_resize", json!({"id":tid,"cols":0,"rows":24})),
        (13, "rename", json!({"id":999999,"name":"gone"})),
        (14, "service", json!({"operation":"stop"})),
        (15, "dev", json!({"operation":"off"})),
        (16, "create", json!({"tool":"claude","host":"unknown"})),
        (17, "move_in", json!({"pid":0})),
        (18, "scan_report", json!({"id":"../private"})),
        (19, "overview", json!({"unexpected":true})),
    ] {
        assert_eq!(control(&mut ws, rid, action, args)["t"], "err", "{action}");
    }
    let scratch = control(&mut ws, 31, "terminal", json!({"scratch":true}));
    let scratch_id = scratch["data"]["id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.session(scratch_id).is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(30));
    }
    let reused = control(&mut ws, 32, "terminal", json!({"scratch":true}));
    assert_eq!(reused["data"]["id"], scratch_id);
    let mut agents = Vec::new();
    for rid in [33, 34] {
        let created =
            control(&mut ws, rid, "create", json!({"tool":"claude","cwd":dir.path(),"name":format!("Agent {rid}")}));
        let id = created["data"]["id"].as_u64().expect("an agent id");
        let deadline = Instant::now() + Duration::from_secs(8);
        while !screen_shows(&hub, id, "ready") {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(40));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while hub.session(id).is_none_or(|s| s.status != Status::Working) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(40));
        }
        agents.push(id);
    }
    assert_eq!(control(&mut ws, 35, "terminal_input", json!({"id":agents[0],"text":"no"}))["t"], "err");
    let broadcast = control(&mut ws, 36, "broadcast", json!({"ids":agents,"text":"broadcast fixture"}));
    assert_eq!(broadcast["data"]["accepted"], json!(agents), "{broadcast}");
    for id in &agents {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !screen_shows(&hub, *id, "received:broadcast fixture") {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(40));
        }
    }
    let mut hook = UnixStream::connect(&socket).unwrap();
    write_msg(&mut hook, &ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Hook }).unwrap();
    write_msg(
        &mut hook,
        &ClientMsg::Hook { session: agents[0], tool: Tool::Claude, payload: json!({"hook_event_name":"Stop"}) },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.session(agents[0]).is_none_or(|s| s.status != Status::Finished) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(40));
    }
    let skipped = control(&mut ws, 80, "broadcast", json!({"ids":[agents[0]],"text":"leave idle work alone"}));
    assert_eq!(skipped["data"]["accepted"], json!([]), "{skipped}");
    assert!(
        skipped["data"]["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("no longer working; queued broadcast was not sent"),
        "{skipped}"
    );
    let skill = dir.path().join("fixture-skill");
    fs::create_dir(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), "---\nname: fixture-skill\ndescription: fixture\n---\nRead only.\n").unwrap();
    let installed = control(&mut ws, 23, "install", json!({"source":skill,"agents":["claude"],"dry_run":true}));
    assert_eq!(installed["t"], "control", "{installed}");
    assert_eq!(installed["data"]["ok"], true, "{installed}");
    assert!(!installed["data"]["lines"].as_array().unwrap().is_empty(), "{installed}");
    assert_eq!(fs::read_dir(&skill).unwrap().count(), 1, "a dry run preserves its source");
    let synced = control(&mut ws, 24, "skills_sync", json!({}));
    assert_eq!(synced["t"], "control", "{synced}");
    assert_eq!(control(&mut ws, 25, "dev", json!({"operation":"on"}))["t"], "control");
    assert_eq!(control(&mut ws, 26, "dev", json!({"operation":"off","confirm":true}))["t"], "control");
    assert_eq!(control(&mut ws, 27, "scans", json!({}))["data"]["items"], json!([]));
    let scan = control(&mut ws, 40, "scan", json!({"scope":"README.md"}));
    assert_eq!(scan["t"], "control", "{scan}");
    assert!(scan["data"]["id"].as_u64().is_some());
    let deadline = Instant::now() + Duration::from_secs(8);
    let saved = loop {
        let scans = control(&mut ws, 41, "scans", json!({}));
        if let Some(item) = scans["data"]["items"].as_array().and_then(|items| items.first()) {
            break item.clone();
        }
        assert!(Instant::now() < deadline, "scan not listed: {scans}");
        thread::sleep(Duration::from_millis(50));
    };
    let report = PathBuf::from(saved["dir"].as_str().unwrap()).join("report/report.md");
    fs::write(report, "# Security fixture\nSaved findings.\n").unwrap();
    let report = control(&mut ws, 42, "scan_report", json!({"id":saved["id"]}));
    assert_eq!(report["data"]["text"], "# Security fixture\nSaved findings.\n", "{report}");
    send(&mut ws, json!({"t":"close","rid":43,"id":scan["data"]["id"]}));
    assert_eq!(reply(&mut ws, 43)["t"], "ok");
    let scan_src = PathBuf::from(saved["dir"].as_str().unwrap()).join("src");
    let deadline = Instant::now() + Duration::from_secs(15);
    while scan_src.exists() {
        assert!(Instant::now() < deadline, "scan worktree was not removed");
        thread::sleep(Duration::from_millis(50));
    }
    crate::testsupport::close_fixture_sessions(&socket).unwrap();
    assert_eq!(control(&mut ws, 28, "service", json!({"operation":"stop","confirm":true}))["t"], "control");
    service.join().unwrap().unwrap();
}

#[test]
fn phone_branches_include_hosts_and_their_agent_rows() {
    let dir = TempDir::new("phone-branch-hosts");
    let hub = Arc::new(Hub::new(hub_options(&dir, vec![]), "branches".into()));
    let agent = |id, tool, host: Option<&str>, alive| SessionInfo {
        spawned_ms: 1000 + id,
        account_root: None,
        metadata: Default::default(),
        id,
        tool,
        title: format!("Agent {id}"),
        status: Status::Idle,
        detail: String::new(),
        pid: None,
        conversation: None,
        worktree: None,
        containers: Vec::new(),
        review: false,
        alive,
        has_screen: true,
        created_at: 0,
        status_since: 0,
        subagents: Vec::new(),
        activity: None,
        last_active: 0,
        cwd: Some("/work/tree".into()),
        job: None,
        host: host.map(str::to_string),
    };
    let branch = BranchInfo {
        branch: "feature".into(),
        path: "/work/tree".into(),
        ahead: Some(3),
        behind: Some(2),
        ..BranchInfo::default()
    };
    hub.on_service(ServerMsg::State(DeckState {
        branches: vec![BranchInfo { main: true, ..branch.clone() }],
        sessions: vec![
            agent(1, Tool::Claude, None, true),
            agent(2, Tool::Shell, None, true),
            agent(3, Tool::Codex, Some("fedora"), true),
            agent(4, Tool::Claude, None, false),
        ],
        ..DeckState::default()
    }));
    hub.on_service(ServerMsg::Hosts {
        hosts: vec![HostInfo {
            role: Default::default(),
            name: "fedora".into(),
            label: "Fedora".into(),
            online: true,
            stats: Some(HostStats { branches: vec![branch], ..HostStats::default() }),
            ..HostInfo::default()
        }],
    });
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("fixture", tx.clone(), None, None);
    hub.handle(conn, serde_json::from_value(json!({"t":"control","rid":1,"action":"branches"})).unwrap(), &tx);
    let result = loop {
        let v: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).unwrap()).unwrap();
        if v["rid"] == 1 {
            break v;
        }
    };
    assert_eq!(result["data"]["items"][0]["ahead"], 3);
    assert_eq!(result["data"]["items"][0]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(result["data"]["items"][0]["sessions"][0]["id"], 1);
    assert_eq!(result["data"]["items"][1]["sessions"][0]["id"], 3);
    assert_eq!(result["data"]["items"][1]["host"], "fedora");
    assert_eq!(result["data"]["items"][1]["host_label"], "Fedora");
    hub.remove_conn(conn);
    hub.handle(conn, serde_json::from_value(json!({"t":"control","rid":2,"action":"overview"})).unwrap(), &tx);
    let denied: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).unwrap()).unwrap();
    assert_eq!(denied["t"], "err");
    assert!(denied["error"].as_str().unwrap().contains("not signed in"));
}

#[test]
fn remote_scan_reports_are_listed_and_read_only_for_a_signed_in_known_host() {
    use crate::hosts::{self, peer};
    use crate::protocol::{host_id, ScanInfo};
    let dir = TempDir::new("phone-remote-reports");
    let scans = dir.path().join("host-scans");
    let scan_id = "20261103-123000-abcd";
    let report = scans.join(scan_id).join("report/report.md");
    fs::create_dir_all(report.parent().unwrap()).unwrap();
    fs::write(&report, "# Remote saved report\nVerified peer content.").unwrap();
    let peer_dir = dir.path().join("peer");
    let identity = store::load_or_create_identity(&peer_dir, "127.0.0.1").unwrap();
    let key = hosts::hub_key(&dir.path().join("hub-state")).unwrap();
    peer::record_hub_key(&peer_dir, &key).unwrap();
    let config = server::tls_config(&identity.cert_der, &identity.key_der).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let peer = Arc::new(
        peer::Peer::new(
            peer_dir,
            hosts::parse_cidrs(&["127.0.0.0/8".into()]),
            "Fedora".into(),
            dir.path().into(),
            None,
        )
        .with_scans_dir(scans),
    );
    thread::spawn(move || peer::accept_all(&peer, &config, listener));
    let host = crate::config::HostConfig {
        role: Default::default(),
        name: "fedora".into(),
        label: Some("Fedora".into()),
        tag: 1,
        addresses: vec![address],
        ssh: None,
        fingerprint: identity.fingerprint,
    };
    let mut opts = hub_options(&dir, vec![]);
    opts.hosts = vec![host];
    opts.hub_key = Some(key);
    let hub = Arc::new(Hub::new(opts, "reports".into()));
    hub.on_service(ServerMsg::Hosts {
        hosts: vec![HostInfo {
            role: Default::default(),
            name: "fedora".into(),
            label: "Fedora".into(),
            online: true,
            ..HostInfo::default()
        }],
    });
    let remote = crate::hosts::fed::state_from(
        1,
        "fedora",
        DeckState {
            scans: vec![ScanInfo {
                id: scan_id.into(),
                state: "saved".into(),
                session: Some(7),
                ..ScanInfo::default()
            }],
            ..DeckState::default()
        },
    );
    hub.on_service(ServerMsg::State(remote));
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("fixture-phone", tx.clone(), None, None);
    let call = |rid, action, args: Value| {
        hub.handle(conn, crate::remote::wire::Req::Control { rid, action: String::from(action), args }, &tx);
        answer(&rx, rid)
    };
    assert_eq!(call(1, "scans", json!({}))["data"]["items"], json!([]), "absent host is local");
    let all = call(2, "scans", json!({"host":"all"}));
    assert_eq!(all["data"]["items"][0]["host"], "fedora");
    assert_eq!(all["data"]["items"][0]["session"], host_id(1, 7));
    let report = call(3, "scan_report", json!({"host":"fedora","id":scan_id}));
    assert_eq!(report["data"]["text"], "# Remote saved report\nVerified peer content.", "{report}");
    for (rid, args) in [
        (4, json!({"id":scan_id})),
        (5, json!({"host":"unknown","id":scan_id})),
        (6, json!({"host":"fedora","id":"../private"})),
    ] {
        assert_eq!(call(rid, "scan_report", args)["t"], "err");
    }
    hub.on_service(ServerMsg::Hosts {
        hosts: vec![HostInfo { role: Default::default(), name: "fedora".into(), online: false, ..HostInfo::default() }],
    });
    assert_eq!(call(7, "scan_report", json!({"host":"fedora","id":scan_id}))["t"], "err");
    hub.remove_conn(conn);
    assert_eq!(call(8, "scans", json!({"host":"all"}))["t"], "err");
}

#[test]
fn phone_move_in_lets_the_owning_service_resolve_a_newly_identified_conversation() {
    use crate::protocol::read_msg;
    use std::io::BufReader;
    use std::os::unix::net::UnixListener;
    let dir = TempDir::new("phone-move-in-current-identity");
    let opts = hub_options(&dir, vec![]);
    fs::create_dir_all(opts.socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&opts.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let service = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return false,
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut reader = BufReader::new(stream);
        assert!(matches!(read_msg::<_, ClientMsg>(&mut reader).unwrap(), Some(ClientMsg::Hello { .. })));
        assert!(matches!(
            read_msg::<_, ClientMsg>(&mut reader).unwrap(),
            Some(ClientMsg::MoveIn { tool: Tool::Claude, pid: 77, conversation: None })
        ));
        write_msg(
            reader.get_mut(),
            &ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: "/fixture".into() },
        )
        .unwrap();
        // The owning service has identified this conversation since the gateway
        // snapshot. Its normal identity checks, exercised by service tests, apply.
        write_msg(reader.get_mut(), &ServerMsg::Created { id: 88, request: None }).unwrap();
        true
    });
    let hub = Arc::new(Hub::new(opts, "move-in".into()));
    hub.on_service(ServerMsg::State(DeckState {
        outside: vec![crate::tui::tests::outside(77, None)],
        ..DeckState::default()
    }));
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("fixture-phone", tx.clone(), None, None);
    hub.handle(
        conn,
        crate::remote::wire::Req::Control { rid: 1, action: "move_in".into(), args: json!({"pid":77}) },
        &tx,
    );
    let result = answer(&rx, 1);
    let dispatched = service.join().unwrap();
    assert!(dispatched, "The gateway rejected a cached unknown identity before service validation: {result:?}");
    assert_eq!(result["data"]["id"], 88);
    hub.remove_conn(conn);
}

#[test]
fn phone_starts_name_a_known_online_host_and_auto_is_not_one() {
    // Owner, 2026-11-08: there is no automatic placement.
    use std::os::unix::net::UnixListener;
    for advanced in [false, true] {
        for (version, host, expected) in [
            (PROTOCOL_VERSION, "auto", if advanced { "unknown host" } else { "auto is not reachable" }),
            (PROTOCOL_VERSION, "@auto", if advanced { "unknown host" } else { "@auto is not reachable" }),
            (12, "unknown", "unknown"),
            (12, "worker", "not reachable"),
        ] {
            let dir = TempDir::new("phone-auto-update");
            let mut opts = hub_options(&dir, vec![]);
            opts.hosts.push(crate::config::HostConfig {
                role: Default::default(),
                name: "worker".into(),
                label: Some("Worker".into()),
                tag: 1,
                addresses: vec![],
                ssh: None,
                fingerprint: String::new(),
            });
            fs::create_dir_all(opts.socket.parent().unwrap()).unwrap();
            let listener = UnixListener::bind(&opts.socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let hub = Arc::new(Hub::new(opts, "auto-update".into()));
            hub.on_service(ServerMsg::Welcome { version, pid: 1, start_dir: "/fixture".into() });
            hub.on_service(ServerMsg::State(DeckState::default()));
            hub.on_service(ServerMsg::Hosts {
                hosts: vec![HostInfo {
                    role: Default::default(),
                    name: "worker".into(),
                    label: "Worker".into(),
                    online: false,
                    ..Default::default()
                }],
            });
            let (tx, rx) = mpsc::channel();
            let conn = hub.add_conn("auto-phone", tx.clone(), None, None);
            let request = if advanced {
                crate::remote::wire::Req::Control {
                    rid: 1,
                    action: "create".into(),
                    args: json!({"tool":"grok","host":host}),
                }
            } else {
                crate::remote::wire::Req::New {
                    rid: 1,
                    tool: Tool::Grok,
                    model: None,
                    effort: None,
                    text: None,
                    host: Some(host.into()),
                }
            };
            hub.handle(conn, request, &tx);
            let result = answer(&rx, 1);
            assert!(
                result["error"].as_str().is_some_and(|text| text.contains(expected)),
                "advanced={advanced}: {result}"
            );
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                "rejected starts must not reach an old service or offline host"
            );
            hub.remove_conn(conn);
        }
    }
}
