use super::*;

#[test]
fn shared_phone_content_follows_native_source_in_three_host_registry() {
    use crate::hosts::{self, peer::Peer};
    use crate::remote::{auth, server, store};
    use std::net::TcpListener;
    let dir = crate::testsupport::TempDir::new("shared-phone-content");
    let coordinator_state = dir.path().join("coordinator/state");
    let viewer_state = dir.path().join("viewer/state");
    let hub_key = hosts::hub_key(&coordinator_state).unwrap();
    let viewer_key = hosts::hub_key(&viewer_state).unwrap();
    let conversation = "33333333-3333-4333-8333-333333333333";
    let peer = |name: &str, state: &std::path::Path, key: &str, config: Config| {
        let repo = state.join("repo");
        let claude = state.join("claude");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(state.join("codex")).unwrap();
        std::fs::write(state.join("codex/config.toml"), format!("model = \"model-{name}\"\n")).unwrap();
        std::fs::create_dir_all(claude.join("commands")).unwrap();
        std::fs::write(claude.join(format!("commands/{name}-only.md")), "---\ndescription: native\n---\nDo it.")
            .unwrap();
        let transcript = crate::claude::transcript_path(&claude, &repo.to_string_lossy(), conversation);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let line = json!({"type":"user","message":{"role":"user","content":[
            {"type":"text","text":format!("fleet needle from {name}")},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}
        ]}});
        std::fs::write(transcript, format!("{line}\n")).unwrap();
        let peer_dir = state.join("peer");
        let identity = store::load_or_create_identity(&peer_dir, "127.0.0.1").unwrap();
        hosts::peer::record_hub_key(&peer_dir, key).unwrap();
        let tls = server::tls_config(&identity.cert_der, &identity.key_der).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let peer = Arc::new(
            Peer::new(peer_dir, hosts::parse_cidrs(&["127.0.0.0/8".into()]), name.into(), repo.clone(), None)
                .with_logins(hosts::logins::LoginPaths { claude_dir: claude, codex_dir: state.join("codex") })
                .with_config(config),
        );
        thread::spawn(move || hosts::peer::accept_all(&peer, &tls, listener));
        (
            HostConfig {
                role: Default::default(),
                name: name.into(),
                label: Some(name.into()),
                tag: 1,
                addresses: vec![address],
                ssh: None,
                fingerprint: identity.fingerprint,
            },
            repo,
        )
    };
    let fedora_state = dir.path().join("fedora/state");
    let ubuntu_state = dir.path().join("ubuntu/state");
    let (fedora, fedora_repo) = peer("fedora", &fedora_state, &hub_key, Config::default());
    let (mut ubuntu, ubuntu_repo) = peer("ubuntu", &ubuntu_state, &hub_key, Config::default());
    ubuntu.tag = 2;
    let config = Config { hosts: vec![fedora, ubuntu], ..Config::default() };
    let (coordinator, coordinator_repo) = peer("omarchy", &coordinator_state, &viewer_key, config);
    let mut opts = crate::remote::tests::hub_options(&dir, vec![]);
    opts.hub_key = Some(viewer_key);
    // This interface has no Fedora/Ubuntu trust inventory and no source logs.
    opts.hosts.clear();
    let order_file = opts.order_file.clone();
    let mut hub = Hub::new(opts, "e".into());
    hub.links = hub.links.via(coordinator);
    let hub = Arc::new(hub);
    for version in [12, 13] {
        hub.on_service(ServerMsg::Welcome { version, pid: 1, start_dir: String::new() });
        assert!(hub.reorder(&[]).is_err(), "an older shared coordinator cannot accept canonical order");
        assert!(!order_file.exists(), "an interface must not create a competing local order");
    }
    hub.on_service(ServerMsg::Welcome { version: 14, pid: 1, start_dir: String::new() });
    let mut sessions = Vec::new();
    let sources = [
        (None, 1, coordinator_repo),
        (Some("fedora"), crate::protocol::host_id(1, 1), fedora_repo),
        (Some("ubuntu"), crate::protocol::host_id(2, 1), ubuntu_repo),
    ];
    hub.on_service(ServerMsg::Hosts {
        hosts: sources
            .iter()
            .filter_map(|(name, _, repo)| {
                Some(HostInfo {
                    role: Default::default(),
                    name: (*name)?.into(),
                    label: (*name)?.into(),
                    online: true,
                    reason: None,
                    start_dir: repo.to_string_lossy().into(),
                    stats: None,
                })
            })
            .collect(),
    });
    for (name, id, repo) in &sources {
        let mut session = search_session(*id, Tool::Claude, Some(conversation), *name);
        session.cwd = Some(repo.to_string_lossy().into());
        sessions.push(session);
    }
    hub.on_service(ServerMsg::State(DeckState { sessions, ..Default::default() }));
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("pixel", tx.clone(), None, None);
    for (index, (name, id, _)) in sources.iter().enumerate() {
        let native = name.unwrap_or("omarchy");
        let rid = 10 + index as u64 * 10;
        hub.handle(conn, Req::Chat { rid, id: *id, before: None, limit: None }, &tx);
        let chat = reply(&rx, rid);
        assert_eq!(chat["source"], "log", "{chat}");
        let message = chat["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["text"] == format!("fleet needle from {native}"))
            .unwrap();
        let key = message["images"][0].as_str().unwrap().to_string();
        hub.handle(conn, Req::Image { rid: rid + 1, id: *id, key }, &tx);
        assert_eq!(reply(&rx, rid + 1)["data"], "iVBORw0KGgo=");
        hub.handle(conn, Req::Commands { rid: rid + 2, id: *id }, &tx);
        let commands = reply(&rx, rid + 2);
        assert!(
            commands["entries"].as_array().unwrap().iter().any(|entry| entry["name"] == format!("/{native}-only")),
            "{commands}"
        );
        let contents = format!("native attachment for {native}").into_bytes();
        hub.handle(
            conn,
            Req::Upload {
                rid: rid + 3,
                id: *id,
                name: format!("{native}.txt"),
                size: contents.len() as u64,
                sha256: auth::sha256_hex(&contents),
            },
            &tx,
        );
        let token = reply(&rx, rid + 3)["upload"].as_str().unwrap().to_string();
        hub.piece(conn, Piece { rid: rid + 5, upload: token.clone(), offset: 0 }, &contents, &tx);
        assert_eq!(reply(&rx, rid + 5)["t"], "ok");
        hub.handle(
            conn,
            Req::Send {
                client_id: None,
                rid: rid + 4,
                id: *id,
                text: "see source attachment".into(),
                files: vec![token],
                relocate: false,
            },
            &tx,
        );
        assert_eq!(reply(&rx, rid + 4)["error"], "delivery status is unknown; check the conversation before retrying");
        let state = match *name {
            None => &coordinator_state,
            Some("fedora") => &fedora_state,
            _ => &ubuntu_state,
        };
        assert_eq!(std::fs::read(state.join(format!("peer/uploads/{native}.txt"))).unwrap(), contents);
    }
    hub.handle(conn, Req::Search { rid: 100, q: "fleet needle".into() }, &tx);
    let found = reply(&rx, 100);
    let mut ids = found["hits"].as_array().unwrap().iter().map(|hit| hit["id"].as_u64().unwrap()).collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, sources.iter().map(|(_, id, _)| *id).collect::<Vec<_>>());
    hub.handle(conn, Req::Models { rid: 101 }, &tx);
    let models = reply(&rx, 101);
    let codex = models["tools"].as_array().unwrap().iter().find(|tool| tool["tool"] == "codex").unwrap();
    assert_eq!(codex["default_model"], "model-omarchy", "shared defaults belong to the coordinator");

    assert!(hub.control("scans", json!({"host":"fedora"})).is_ok(), "shared inventory validates control host");
}
