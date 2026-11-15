use super::*;

/// Owner, 2026-10-31: "give me a alt+ key to not create a new session but to
/// resume a existing session ... where you pick from a list of sessions",
/// then "and the android app needs that implemented as well".
#[test]
fn the_phone_reads_a_session_on_another_host_from_that_host() {
    use crate::protocol::{host_id, DeckState, HostInfo, HostStats, Status, Tool};
    use crate::remote::wire::Req;
    let dir = TempDir::new("remote-hosts");
    let host_repo = dir.path().join("host/repo");
    let host_claude = dir.path().join("host/claude");
    fs::create_dir_all(&host_repo).unwrap();
    // Only the host has this conversation's transcript and this command.
    let conversation = "33333333-3333-4333-8333-333333333333";
    let transcript = crate::claude::transcript_path(&host_claude, &host_repo.to_string_lossy(), conversation);
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, transcript_line("user", "hello fedora") + &transcript_line("assistant", "hi from fedora"))
        .unwrap();
    fs::create_dir_all(host_claude.join("commands")).unwrap();
    fs::write(host_claude.join("commands/only-on-fedora.md"), "---\ndescription: Fedora's own\n---\nDo it.").unwrap();
    let logins = crate::hosts::logins::LoginPaths { claude_dir: host_claude, codex_dir: dir.path().join("host/codex") };
    let (host, key) = crate::testsupport::loopback_peer(
        dir.path().join("host/peer"),
        &dir.path().join("hub/state"),
        host_repo.clone(),
        logins,
        None,
    );

    let mut opts = hub_options(&dir, vec![PHONE]);
    opts.hosts = vec![host];
    opts.hub_key = Some(key);
    let hub = Arc::new(Hub::new(opts, "e".into()));
    let stats = HostStats {
        cpu: Some(12.5),
        memory: Some(40.0),
        disk_used: Some(1),
        disk_available: Some(3),
        ..Default::default()
    };
    hub.on_service(ServerMsg::Hosts {
        hosts: vec![
            HostInfo {
                role: Default::default(),
                name: "fedora".into(),
                label: "Fedora".into(),
                online: true,
                reason: None,
                start_dir: host_repo.to_string_lossy().into(),
                stats: Some(stats),
            },
            HostInfo {
                role: Default::default(),
                name: "ctl".into(),
                label: "Controller".into(),
                online: false,
                reason: Some("unreachable".into()),
                start_dir: String::new(),
                stats: None,
            },
        ],
    });
    let id = host_id(1, 3);
    let mut remote = crate::tui::tests::session(id, Tool::Claude, Status::Idle, true);
    remote.conversation = Some(conversation.into());
    remote.host = Some("fedora".into());
    let local = crate::tui::tests::session(2, Tool::Claude, Status::Idle, true);
    hub.on_service(ServerMsg::State(DeckState {
        processes: Vec::new(),
        sessions: vec![remote, local],
        ..Default::default()
    }));

    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("pixel", tx.clone(), None, None);
    let state = last_state(&rx);
    let listed: Vec<(u64, Value)> = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_u64().unwrap(), s["host_label"].clone()))
        .collect();
    assert_eq!(
        listed,
        [(2, Value::Null), (id, json!("Fedora"))],
        "this machine's first, then each host's under its label"
    );
    assert_eq!(state["here"], "ThinkPad");
    assert_eq!(
        state["hosts"][1],
        json!({"name": "ctl", "label": "Controller", "online": false, "reason": "unreachable", "agents": true})
    );

    hub.handle(conn, Req::Chat { rid: 1, id, before: None, limit: None }, &tx);
    let page = answer(&rx, 1);
    assert_eq!(page["source"], "log", "{page}");
    let texts: Vec<&str> = page["items"].as_array().unwrap().iter().filter_map(|i| i["text"].as_str()).collect();
    assert_eq!(texts, ["hello fedora", "hi from fedora"]);

    thread::sleep(Duration::from_millis(750));
    let mut more = fs::OpenOptions::new().append(true).open(&transcript).unwrap();
    std::io::Write::write_all(&mut more, transcript_line("assistant", "and more").as_bytes()).unwrap();
    hub.handle(conn, Req::Chat { rid: 2, id, before: None, limit: None }, &tx);
    let page = answer(&rx, 2);
    assert_eq!((page["total"].as_u64(), page["items"][2]["text"].as_str()), (Some(3), Some("and more")));

    hub.handle(conn, Req::Commands { rid: 3, id }, &tx);
    let commands = answer(&rx, 3);
    let names: Vec<&str> = commands["entries"].as_array().unwrap().iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"/only-on-fedora"), "the host's own commands: {names:?}");
    hub.handle(conn, Req::Commands { rid: 4, id: 2 }, &tx);
    let here = answer(&rx, 4);
    assert!(!here["entries"].as_array().unwrap().iter().any(|e| e["name"] == "/only-on-fedora"), "not this machine's");

    hub.handle(conn, Req::Stats { rid: 5 }, &tx);
    let stats = answer(&rx, 5);
    assert_eq!(stats["label"], "ThinkPad");
    assert_eq!(stats["hosts"][0]["cpu"], 12.5);
    assert_eq!(stats["hosts"][0]["disk"], json!({"used": 1, "available": 3}));
    assert_eq!(stats["hosts"][1]["online"], false);

    // A screenshot sent in it is fetched from that host's log (owner,
    // 2026-11-03: the phone showed "[image]" where the Claude app shows it).
    let shot = json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "text", "text": "this one"},
        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}]}});
    thread::sleep(Duration::from_millis(750));
    std::io::Write::write_all(&mut more, format!("{shot}\n").as_bytes()).unwrap();
    hub.handle(conn, Req::Chat { rid: 20, id, before: None, limit: None }, &tx);
    let page = answer(&rx, 20);
    let sent = page["items"].as_array().unwrap().iter().find(|i| i["text"] == "this one").cloned();
    let key = sent.as_ref().and_then(|i| i["images"][0].as_str()).unwrap_or_else(|| panic!("{page}")).to_string();
    hub.handle(conn, Req::Image { rid: 21, id, key: key.clone() }, &tx);
    let image = answer(&rx, 21);
    assert_eq!((image["media"].as_str(), image["data"].as_str()), (Some("image/png"), Some("iVBORw0KGgo=")), "{image}");
    hub.handle(conn, Req::Image { rid: 22, id, key: "0.0".into() }, &tx);
    assert!(answer(&rx, 22)["error"].as_str().is_some_and(|e| e.contains("no longer in the log")));

    // A file attached to it is copied to that host before the message is
    // typed; this gateway has no service to type it into.
    let empty = crate::remote::auth::sha256_hex(b"");
    hub.handle(conn, Req::Upload { rid: 6, id, name: "plan.txt".into(), size: 0, sha256: empty }, &tx);
    let token = answer(&rx, 6)["upload"].as_str().expect("an upload id").to_string();
    hub.handle(
        conn,
        Req::Send { client_id: None, rid: 7, id, text: "see the plan".into(), files: vec![token], relocate: false },
        &tx,
    );
    assert_eq!(answer(&rx, 7)["error"], "the Cepheus service is not running");
    assert!(dir.path().join("host/peer/uploads/plan.txt").is_file(), "the host keeps its own copy");

    // A host that is down takes no new session.
    hub.handle(
        conn,
        Req::New { rid: 8, tool: Tool::Claude, model: None, effort: None, text: None, host: Some("ctl".into()) },
        &tx,
    );
    let refused = answer(&rx, 8);
    assert_eq!(refused["error"], "Controller is not reachable now", "{refused}");
}
