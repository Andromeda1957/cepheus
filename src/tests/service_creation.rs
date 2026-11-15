use super::*;

/// A host: its own service behind a peer listener on loopback that admits
/// the hub key in `hub_state`. Returns the host's service thread, socket
/// and the hub's config entry for it.
fn start_host(
    dir: &TempDir,
    hub_state: &Path,
    script: &str,
) -> (thread::JoinHandle<Result<()>>, PathBuf, crate::config::HostConfig) {
    let mut opts = options(dir, script);
    // This journey exercises the session channel; native replication has its own
    // real Git/LFS fixtures and cannot allocate nested Podman storage here.
    let socket = opts.socket.clone();
    let service = thread::spawn(move || run(opts));
    let logins =
        crate::hosts::logins::LoginPaths { claude_dir: dir.path().join("claude"), codex_dir: dir.path().join("codex") };
    let (host, _) = crate::testsupport::loopback_peer(
        dir.path().join("peer"),
        hub_state,
        dir.path().to_path_buf(),
        logins,
        Some(socket.clone()),
    );
    (service, socket, host)
}

#[test]
fn a_session_on_another_host_is_started_typed_into_watched_and_closed_from_the_hub() {
    // Owner, 2026-10-31: "i can launch agents on the fedora machine through
    // the agent deck on this thinkpad and control them ... you only notice by
    // where the session is listed under".
    let host_dir = TempDir::new("fed-host");
    let hub_dir = TempDir::new("fed-hub");
    let mut hub_opts = options(&hub_dir, "echo hub ready; read x");
    let (host_service, host_socket, host) =
        start_host(&host_dir, &hub_opts.state_dir, "echo host ready; read x; echo \"got $x\"; read y");
    hub_opts.config.hosts = vec![host];
    let hub_socket = hub_opts.socket.clone();
    let hub_service = thread::spawn(move || run(hub_opts));

    let mut deck = TestClient::connect_with(&hub_socket, PROTOCOL_VERSION, ClientRole::Tui);
    deck.wait("the host comes online", |m, _| match m {
        ServerMsg::Hosts { hosts } => hosts.iter().any(|h| h.name == "fedora" && h.online).then_some(()),
        _ => None,
    });
    deck.send(ClientMsg::Create {
        tool: Tool::Claude,
        model: None,
        effort: None,
        host: Some("fedora".into()),
        cwd: None,
        command: Vec::new(),
        name: None,
        request: Some(crate::protocol::host_id(2, 17)),
    });
    let id = deck.wait("created on the host", |m, _| match m {
        ServerMsg::Created { id, request: Some(token) } if *token == crate::protocol::host_id(2, 17) => Some(*id),
        ServerMsg::Notice { text, error: true } => panic!("{text}"),
        _ => None,
    });
    assert_eq!(crate::protocol::split_id(id).0, 1, "the id carries the host's tag");
    deck.wait_state("listed under the host", |s| {
        s.sessions.iter().any(|x| x.id == id && x.host.as_deref() == Some("fedora"))
    });
    deck.wait_screen(id, "host ready");
    deck.send(ClientMsg::Shutdown);
    deck.wait("Stop refuses a live remote session", |msg, _| {
        matches!(msg, ServerMsg::Notice { text, error: true } if text == crate::protocol::STOP_REFUSED_NOTICE)
            .then_some(())
    });
    deck.send(ClientMsg::Input { id, data: "over the network\r".into() });
    deck.wait_screen(id, "got over the network");
    deck.send(ClientMsg::Attach { id: Some(id), cols: 100, rows: 30 });
    deck.wait("a frame of the remote session", |m, _| match m {
        ServerMsg::Frame { id: got, .. } if *got == id => Some(()),
        _ => None,
    });
    // The host's own deck sees the session under its own id.
    let mut local = TestClient::connect(&host_socket);
    local.wait_state("the host lists it", |s| s.sessions.iter().any(|x| x.id == crate::protocol::split_id(id).1));
    deck.send(ClientMsg::ListConversations { request: None, query: String::new() });
    deck.wait("one merged list", |m, _| matches!(m, ServerMsg::Conversations { .. }).then_some(()));
    deck.send(ClientMsg::Create {
        tool: Tool::Claude,
        model: None,
        effort: None,
        host: Some("nowhere".into()),
        cwd: None,
        command: Vec::new(),
        name: None,
        request: None,
    });
    let refused = deck.wait("an unknown host is refused", |m, _| match m {
        ServerMsg::Notice { text, error: true } => Some(text.clone()),
        _ => None,
    });
    assert!(refused.contains("nowhere"), "{refused}");
    deck.send(ClientMsg::Close { id });
    deck.wait_state("closed on the host", |s| s.sessions.iter().all(|x| x.id != id));
    deck.send(ClientMsg::Shutdown);
    hub_service.join().unwrap().unwrap();
    local.send(ClientMsg::Shutdown);
    host_service.join().unwrap().unwrap();
}
