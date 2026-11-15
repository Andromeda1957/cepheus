use super::*;

#[test]
fn phone_clear_opts_into_a_correlated_replacement_while_older_clients_keep_their_id() {
    for case in ["legacy", "fresh", "cross-host", "connection", "cached"] {
        let dir = crate::testsupport::TempDir::new("phone-clear-receipt");
        let hub = Arc::new(Hub::new(crate::remote::tests::hub_options(&dir, vec![]), "e".into()));
        hub.on_service(ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: String::new() });
        let id = if case == "cross-host" { crate::protocol::host_id(1, 91) } else { 91 };
        let replacement = SessionInfo {
            pid: Some(43),
            ..search_session(id, Tool::Codex, None, (case == "cross-host").then_some("other"))
        };
        if case == "cross-host" {
            hub.on_service(ServerMsg::Hosts {
                hosts: vec![HostInfo {
                    role: Default::default(),
                    name: "other".into(),
                    label: "Other".into(),
                    online: true,
                    ..Default::default()
                }],
            });
        }
        let original = SessionInfo { pid: Some(42), ..search_session(7, Tool::Codex, Some("old"), None) };
        let mut sessions = vec![original.clone()];
        if case == "cached" {
            sessions.push(replacement.clone());
        }
        hub.on_service(ServerMsg::State(DeckState { sessions, ..Default::default() }));
        let (socket, peer) = UnixStream::pair().unwrap();
        *lock(&hub.service) = Some(socket);
        let service = Arc::clone(&hub);
        let worker = thread::spawn(move || {
            let message = read_msg::<_, ClientMsg>(&mut BufReader::new(peer)).unwrap().unwrap();
            match &message {
                ClientMsg::Plugin { request, command, args, .. } if case != "legacy" => {
                    assert_eq!(command, "target");
                    assert_eq!(args["operation"], "clear", "the phone's Clear types nothing into the terminal");
                    assert!(args.get("text").is_none());
                    assert_eq!(args["id"], 7);
                    assert_eq!(
                        args["expected"],
                        json!(crate::attention::ExpectedTarget::from_session(&original).unwrap())
                    );
                    if case == "connection" {
                        service.on_service(ServerMsg::Welcome {
                            version: PROTOCOL_VERSION,
                            pid: 2,
                            start_dir: String::new(),
                        });
                    }
                    if case != "cached" {
                        service.on_service(ServerMsg::State(DeckState {
                            sessions: vec![replacement],
                            ..Default::default()
                        }));
                    }
                    service.on_service(ServerMsg::PluginResult {
                        request: *request,
                        data: json!({"id": split_id(id).1}),
                        error: None,
                    });
                }
                ClientMsg::Plugin { request, args, .. } => {
                    assert_eq!(args["operation"], "send", "an older APK's /clear is an ordinary message");
                    service.on_service(ServerMsg::PluginResult {
                        request: *request,
                        data: Value::Null,
                        error: Some("this command requires a native provider control; nothing was sent".into()),
                    });
                }
                other => panic!("unexpected clear transport: {other:?}"),
            }
            message
        });
        let (tx, rx) = mpsc::channel();
        let conn = hub.add_conn("phone", tx.clone(), None, None);
        hub.handle(
            conn,
            serde_json::from_value(json!({
                "t":"send","rid":81,"id":7,"text":"/clear","relocate":case != "legacy"
            }))
            .unwrap(),
            &tx,
        );
        let result = reply(&rx, 81);
        let message = worker.join().unwrap();
        match case {
            "legacy" => {
                assert!(matches!(message, ClientMsg::Plugin { .. }));
                assert_eq!(result["t"], "err", "{result}");
                assert!(result.get("id").is_none(), "an older APK keeps its existing native session");
            }
            "connection" | "cached" => {
                assert_eq!(result["t"], "err", "{case} cannot complete a clear: {result}");
                assert!(result.get("id").is_none());
            }
            _ => {
                assert!(matches!(message, ClientMsg::Plugin { .. }));
                assert_eq!(result["id"], id, "the clear receipt names the freshly published replacement");
            }
        }
    }
}

#[test]
fn delayed_phone_model_keeps_its_original_target_after_the_first_command() {
    for replacement in ["process", "conversation", "connection"] {
        let dir = crate::testsupport::TempDir::new("phone-model-custody");
        let hub = Arc::new(Hub::new(crate::remote::tests::hub_options(&dir, vec![]), "custody".into()));
        let id = crate::protocol::host_id(1, 7);
        let original =
            SessionInfo { pid: Some(42), ..search_session(id, Tool::Claude, Some("original"), Some("remote")) };
        hub.on_service(ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: String::new() });
        hub.on_service(ServerMsg::Hosts {
            hosts: ["remote", "other"]
                .map(|name| HostInfo {
                    role: Default::default(),
                    name: name.into(),
                    label: "Shared label".into(),
                    ..Default::default()
                })
                .to_vec(),
        });
        hub.on_service(ServerMsg::State(DeckState { sessions: vec![original.clone()], ..Default::default() }));
        let (socket, peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(crate::testsupport::scaled_timeout(Duration::from_secs(4)))).unwrap();
        *lock(&hub.service) = Some(socket);
        let mut reader = BufReader::new(peer);
        let (tx, rx) = mpsc::channel();
        let conn = hub.add_conn("phone", tx.clone(), None, None);
        hub.handle(
            conn,
            serde_json::from_value(json!({"t":"model","rid":1,"id":id,"model":"opus","effort":"high"})).unwrap(),
            &tx,
        );
        let first = read_msg::<_, ClientMsg>(&mut reader).unwrap().unwrap();
        let request = match first {
            ClientMsg::SendMessage { request, text, .. } => {
                assert_eq!(text, "/model opus");
                request
            }
            ClientMsg::Plugin { request, command, args, .. } => {
                assert_eq!(command, "target");
                assert_eq!(args["host"], "remote");
                assert_eq!(args["id"], 7);
                assert_eq!(args["text"], "/model opus");
                request
            }
            _ => panic!("unexpected first model command"),
        };
        let mut changed = original;
        match replacement {
            "process" => {
                changed.pid = Some(43);
                changed.spawned_ms += 1;
            }
            "conversation" => changed.conversation = Some("replacement".into()),
            "connection" => {
                hub.on_service(ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: String::new() })
            }
            _ => unreachable!(),
        }
        hub.on_service(ServerMsg::State(DeckState { sessions: vec![changed], ..Default::default() }));
        hub.on_service(ServerMsg::MessageSent { id, request, error: None, delivered: None });
        // Let the unsafe baseline finish as well, so this proves a second dispatch.
        let second = match read_msg::<_, ClientMsg>(&mut reader) {
            Ok(message) => message,
            Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => None,
            Err(error) => panic!("reading delayed command: {error:#}"),
        };
        if let Some(ClientMsg::SendMessage { request, .. } | ClientMsg::Plugin { request, .. }) = &second {
            hub.on_service(ServerMsg::MessageSent { id, request: *request, error: None, delivered: None });
        }
        assert!(second.is_none(), "a {replacement} replacement received the delayed effort command: {second:?}");
        assert_eq!(reply(&rx, 1)["t"], "err");
    }
}

#[test]
fn delayed_first_text_accepts_initial_native_identity_but_rejects_a_replaced_spawn() {
    for tool in [Tool::Claude, Tool::Codex] {
        for replace in [true, false] {
            let dir = crate::testsupport::TempDir::new("phone-first-text-custody");
            let opts = crate::remote::tests::hub_options(&dir, vec![]);
            std::fs::create_dir_all(opts.socket.parent().unwrap()).unwrap();
            let listener = std::os::unix::net::UnixListener::bind(&opts.socket).unwrap();
            let hub = Arc::new(Hub::new(opts, "first".into()));
            hub.on_service(ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: String::new() });
            hub.on_service(ServerMsg::State(DeckState::default()));
            let (socket, peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(crate::testsupport::scaled_timeout(Duration::from_secs(3)))).unwrap();
            *lock(&hub.service) = Some(socket);
            let mut reader = BufReader::new(peer);
            let (tx, rx) = mpsc::channel();
            let conn = hub.add_conn("phone", tx.clone(), None, None);
            hub.handle(
                conn,
                serde_json::from_value(json!({"t":"new","rid":2,"tool":tool.wire_key(),"text":"original first text"}))
                    .unwrap(),
                &tx,
            );
            // The start goes on a connection of its own (ROOT-033); the first
            // text follows on the bridge.
            let mut private = private_request(&listener);
            assert!(matches!(read_msg::<_, ClientMsg>(&mut private).unwrap(), Some(ClientMsg::Create { .. })));
            let starting =
                SessionInfo { pid: Some(42), status: Status::Working, ..search_session(7, tool, None, None) };
            write_msg(private.get_mut(), &ServerMsg::Created { id: 7, request: None }).unwrap();
            hub.on_service(ServerMsg::State(DeckState { sessions: vec![starting.clone()], ..Default::default() }));
            assert_eq!(reply(&rx, 2)["t"], "ok");
            if tool == Tool::Codex {
                reader.get_mut().set_read_timeout(Some(Duration::from_millis(100))).unwrap();
                assert!(
                    matches!(read_msg::<_, ClientMsg>(&mut reader), Err(error)
                if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
                    "an idle new Codex must publish its native ID before first text"
                );
                reader
                    .get_mut()
                    .set_read_timeout(Some(crate::testsupport::scaled_timeout(Duration::from_secs(3))))
                    .unwrap();
            }
            // This is the first native conversation publication, not a new spawn.
            let ready = SessionInfo {
                pid: Some(if replace { 43 } else { 42 }),
                spawned_ms: starting.spawned_ms + u64::from(replace),
                status: Status::Working,
                conversation: Some("first-native-id".into()),
                ..starting
            };
            hub.on_service(ServerMsg::State(DeckState { sessions: vec![ready], ..Default::default() }));
            let sent = match read_msg::<_, ClientMsg>(&mut reader) {
                Ok(message) => message,
                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    None
                }
                Err(error) => panic!("reading first text: {error:#}"),
            };
            if replace {
                assert!(sent.is_none(), "the replacement spawn received the original first message: {sent:?}");
            } else {
                let Some(ClientMsg::Plugin { request, command, args, .. }) = sent else {
                    panic!("first text must use target custody: {sent:?}")
                };
                assert_eq!(command, "target");
                assert_eq!(args["expected"]["conversation"], "first-native-id");
                assert_eq!(args["text"], "original first text");
                hub.on_service(ServerMsg::MessageSent { id: 7, request, error: None, delivered: None });
            }
        }
    }
}

#[test]
fn phone_cached_sessions_wait_for_authoritative_service_state() {
    use std::io::Read;
    let dir = crate::testsupport::TempDir::new("phone-service-readiness");
    let hub = Arc::new(Hub::new(crate::remote::tests::hub_options(&dir, vec![]), "e".into()));
    let (socket, mut peer) = UnixStream::pair().unwrap();
    peer.set_nonblocking(true).unwrap();
    *lock(&hub.service) = Some(socket);
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("phone", tx.clone(), None, None);

    // A working phone gateway is not evidence that its execution service is ready.
    hub.on_service(ServerMsg::Welcome { version: 14, pid: 1, start_dir: String::new() });
    for (rid, request) in [(1, Req::Open { rid: 1, id: Some(7), limit: None }), (2, Req::Close { rid: 2, id: 7 })] {
        hub.handle(conn, request, &tx);
        assert!(reply(&rx, rid)["error"].as_str().unwrap().contains("not ready"));
    }
    assert_eq!(peer.read(&mut [0; 1]).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);

    let ended = SessionInfo { alive: false, ..search_session(7, Tool::Claude, None, None) };
    hub.on_service(ServerMsg::State(DeckState { sessions: vec![ended], ..Default::default() }));
    hub.handle(conn, Req::Open { rid: 3, id: Some(7), limit: None }, &tx);
    assert_eq!(reply(&rx, 3)["source"], "none", "ended history remains readable");
    hub.handle(conn, Req::Open { rid: 4, id: Some(8), limit: None }, &tx);
    assert_eq!(reply(&rx, 4)["error"], "no such session");
    assert_eq!(hub.followed(), [7], "a rejected open cannot replace the previous subscription");

    // A new Welcome begins another service connection; the old list is only a cache.
    hub.on_service(ServerMsg::Welcome { version: 14, pid: 2, start_dir: String::new() });
    hub.handle(conn, Req::Close { rid: 5, id: 7 }, &tx);
    assert!(reply(&rx, 5)["error"].as_str().unwrap().contains("not ready"));
    assert_eq!(peer.read(&mut [0; 1]).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(hub.session(7).unwrap().id, 7, "readiness loss retains recoverable history metadata");
    hub.on_service(ServerMsg::State(DeckState::default()));
    assert!(hub.followed().is_empty(), "a removed route stays unsubscribed if its ID is reused later");
}

#[test]
fn legacy_phone_omitted_host_starts_native_and_explicit_hosts_keep_their_ids() {
    use std::os::unix::net::UnixListener;
    for advanced in [true, false] {
        for (host, id) in [
            (None, 7),
            (Some("@here"), 7),
            (Some("one"), crate::protocol::host_id(1, 7)),
            (Some("two"), crate::protocol::host_id(2, 7)),
            (Some("three"), crate::protocol::host_id(3, 7)),
        ] {
            let dir = crate::testsupport::TempDir::new("phone-explicit-create");
            let opts = crate::remote::tests::hub_options(&dir, vec![]);
            std::fs::create_dir_all(opts.socket.parent().unwrap()).unwrap();
            let listener = UnixListener::bind(&opts.socket).unwrap();
            let hub = Arc::new(Hub::new(opts, "e".into()));
            hub.on_service(ServerMsg::Welcome { version: 14, pid: 1, start_dir: String::new() });
            let hosts = ["one", "two", "three"]
                .iter()
                .map(|name| HostInfo {
                    role: Default::default(),
                    name: (*name).into(),
                    label: (*name).into(),
                    online: true,
                    ..Default::default()
                })
                .collect();
            hub.on_service(ServerMsg::Hosts { hosts });
            let sessions = [
                (None, 7),
                (Some("one"), crate::protocol::host_id(1, 7)),
                (Some("two"), crate::protocol::host_id(2, 7)),
                (Some("three"), crate::protocol::host_id(3, 7)),
            ]
            .iter()
            .map(|(host, id)| search_session(*id, Tool::Claude, None, *host))
            .collect();
            let published = DeckState { sessions, ..Default::default() };
            hub.on_service(ServerMsg::State(DeckState {
                sessions: published.sessions.iter().filter(|s| s.id != id).cloned().collect(),
                ..Default::default()
            }));
            let (socket, _bridge_peer) = UnixStream::pair().unwrap();
            *lock(&hub.service) = Some(socket);
            let service_hub = Arc::clone(&hub);
            let (created, created_rx) = mpsc::channel();
            let (publish, publish_rx) = mpsc::channel();
            let service = thread::spawn(move || {
                // A legacy phone's start goes on a connection of its own, as a
                // control create does, so a late reply cannot answer another
                // request (ROOT-033); the connection names its service first.
                let (mut stream, _) = listener.accept().unwrap();
                assert!(matches!(read_msg::<_, ClientMsg>(&mut stream).unwrap(), Some(ClientMsg::Hello { .. })));
                write_msg(&mut stream, &ServerMsg::Welcome { version: 14, pid: 1, start_dir: String::new() }).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut reader = BufReader::new(stream);
                let msg = read_msg::<_, ClientMsg>(&mut reader).unwrap().unwrap();
                write_msg(reader.get_mut(), &ServerMsg::Created { id, request: None }).unwrap();
                let ClientMsg::Create { host, .. } = &msg else { panic!("not a create") };
                created.send(host.clone()).unwrap();
                publish_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                service_hub.on_service(ServerMsg::State(published));
                msg
            });
            let (tx, rx) = mpsc::channel();
            let conn = hub.add_conn("phone", tx.clone(), None, None);
            while rx.try_recv().is_ok() {}
            let req = if advanced {
                Req::Control { rid: 20, action: "create".into(), args: json!({"tool":"claude", "host":host}) }
            } else {
                Req::New {
                    rid: 20,
                    tool: Tool::Claude,
                    model: None,
                    effort: None,
                    text: None,
                    host: host.map(str::to_string),
                }
            };
            hub.handle(conn, req, &tx);
            assert_eq!(created_rx.recv_timeout(Duration::from_secs(3)).unwrap().as_deref(), host);
            assert!(
                rx.recv_timeout(Duration::from_millis(25)).is_err(),
                "Created alone must not expose an unpublished ID"
            );
            publish.send(()).unwrap();
            let result = reply(&rx, 20);
            let ClientMsg::Create { host: routed, .. } = service.join().unwrap() else { panic!("not a create") };
            assert_eq!(routed.as_deref(), host);
            assert_eq!(if advanced { &result["data"]["id"] } else { &result["id"] }, &json!(id));
            hub.handle(conn, Req::Open { rid: 21, id: Some(id), limit: None }, &tx);
            assert_eq!(reply(&rx, 21)["id"], id);
        }
    }
}

#[test]
fn private_phone_create_requires_its_original_services_identity_before_writing() {
    use std::os::unix::net::UnixListener;
    let guard = crate::testsupport::scaled_timeout(Duration::from_secs(3));
    for (private_pid, replace_main) in [(Some(2), false), (None, false), (Some(1), true)] {
        let dir = crate::testsupport::TempDir::new("phone-private-create-identity");
        let opts = crate::remote::tests::hub_options(&dir, vec![]);
        std::fs::create_dir_all(opts.socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&opts.socket).unwrap();
        let hub = Arc::new(Hub::new(opts, "identity".into()));
        hub.on_service(ServerMsg::Welcome { version: 14, pid: 1, start_dir: String::new() });
        hub.on_service(ServerMsg::State(DeckState::default()));
        let (bridge, _bridge_peer) = UnixStream::pair().unwrap();
        *lock(&hub.service) = Some(bridge);
        let (hello, heard_hello) = mpsc::channel();
        let (release, proceed) = mpsc::channel();
        let service_hub = Arc::clone(&hub);
        let serving = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(guard)).unwrap();
            let mut reader = BufReader::new(stream);
            assert!(matches!(read_msg::<_, ClientMsg>(&mut reader).unwrap(), Some(ClientMsg::Hello { .. })));
            hello.send(()).unwrap();
            proceed.recv_timeout(guard).unwrap();
            if let Some(pid) = private_pid {
                write_msg(reader.get_mut(), &ServerMsg::Welcome { version: 14, pid, start_dir: String::new() })
                    .unwrap();
            } else {
                // State without a Welcome is not private-service identity.
                write_msg(reader.get_mut(), &ServerMsg::State(DeckState::default())).unwrap();
                reader.get_mut().shutdown(std::net::Shutdown::Write).unwrap();
            }
            let forwarded = read_msg::<_, ClientMsg>(&mut reader).unwrap();
            if matches!(forwarded, Some(ClientMsg::Create { .. })) && private_pid.is_some() {
                // Let the unsafe baseline finish so failure proves a launch, not a timeout.
                let _ = write_msg(reader.get_mut(), &ServerMsg::Created { id: 7, request: None });
                service_hub.on_service(ServerMsg::State(DeckState {
                    sessions: vec![search_session(7, Tool::Claude, None, None)],
                    ..Default::default()
                }));
            }
            forwarded
        });
        let (tx, rx) = mpsc::channel();
        let conn = hub.add_conn("phone", tx.clone(), None, None);
        hub.handle(conn, Req::Control { rid: 1, action: "create".into(), args: json!({"tool":"claude"}) }, &tx);
        heard_hello.recv_timeout(guard).unwrap();
        if replace_main {
            // Even a replacement using the same PID must not inherit this request's generation.
            hub.on_service(ServerMsg::Welcome { version: 14, pid: 1, start_dir: String::new() });
            hub.on_service(ServerMsg::State(DeckState::default()));
        }
        release.send(()).unwrap();
        let result = reply(&rx, 1);
        assert!(serving.join().unwrap().is_none(), "an unverified private connection must receive no Create");
        assert_eq!(result["t"], "err", "{result}");
    }
}

#[test]
fn phone_actions_preserve_native_and_tagged_sessions_sharing_a_low_id() {
    let dir = crate::testsupport::TempDir::new("phone-exact-route");
    let hub = Arc::new(Hub::new(crate::remote::tests::hub_options(&dir, vec![]), "e".into()));
    let ids: Vec<_> = (0..=3).map(|tag| crate::protocol::host_id(tag, 7)).collect();
    let sessions = ids.iter().map(|id| search_session(*id, Tool::Claude, None, None)).collect();
    hub.on_service(ServerMsg::State(DeckState { sessions, ..Default::default() }));
    let (socket, peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    *lock(&hub.service) = Some(socket);
    let mut reader = BufReader::new(peer);
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("phone", tx.clone(), None, None);
    for (rid, id) in (1..).zip(ids) {
        hub.handle(conn, Req::Close { rid, id }, &tx);
        assert_eq!(reply(&rx, rid)["t"], "ok");
        assert!(
            matches!(read_msg::<_, ClientMsg>(&mut reader).unwrap(), Some(ClientMsg::Close { id: got }) if got == id)
        );
    }
}

#[test]
fn phone_reopen_waits_for_live_publication_and_rejects_a_replacement_connection() {
    use std::os::unix::net::UnixListener;
    let dir = crate::testsupport::TempDir::new("phone-reopen-publication");
    let opts = crate::remote::tests::hub_options(&dir, vec![]);
    std::fs::create_dir_all(opts.socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&opts.socket).unwrap();
    let hub = Arc::new(Hub::new(opts, "e".into()));
    hub.on_service(ServerMsg::Welcome { version: 14, pid: 1, start_dir: String::new() });
    let ended = SessionInfo { alive: false, pid: Some(11), ..search_session(7, Tool::Claude, None, None) };
    let running = SessionInfo { alive: true, pid: Some(22), ..ended.clone() };
    let publish = |session: SessionInfo| {
        hub.on_service(ServerMsg::State(DeckState { sessions: vec![session], ..Default::default() }))
    };
    publish(ended.clone());
    let (socket, _bridge_peer) = UnixStream::pair().unwrap();
    *lock(&hub.service) = Some(socket);
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("phone", tx.clone(), None, None);
    for (rid, replace) in [(1, false), (2, true)] {
        publish(ended.clone());
        let service = Arc::clone(&hub);
        let sender = tx.clone();
        let (done, completed) = mpsc::channel();
        let request = thread::spawn(move || {
            service.handle(conn, Req::Reopen { rid, id: 7 }, &sender);
            done.send(()).unwrap();
        });
        // Each reopen goes on a connection of its own (ROOT-033).
        let mut private = private_request(&listener);
        assert!(matches!(read_msg::<_, ClientMsg>(&mut private).unwrap(), Some(ClientMsg::Reopen { id: 7 })));
        write_msg(private.get_mut(), &ServerMsg::Created { id: 7, request: None }).unwrap();
        assert!(completed.recv_timeout(Duration::from_millis(25)).is_err());
        publish(ended.clone());
        assert!(completed.recv_timeout(Duration::from_millis(25)).is_err(), "an ended record cannot satisfy reopen");
        if replace {
            hub.on_service(ServerMsg::Welcome { version: 14, pid: 2, start_dir: String::new() });
        }
        publish(running.clone());
        completed.recv_timeout(Duration::from_secs(30)).unwrap();
        request.join().unwrap();
        let answer = reply(&rx, rid);
        if replace {
            assert!(answer["error"].as_str().unwrap().contains("connection changed"));
        } else {
            assert_eq!(answer["t"], "ok");
            hub.handle(conn, Req::Open { rid: 3, id: Some(7), limit: None }, &tx);
            assert_eq!(reply(&rx, 3)["id"], 7);
        }
    }
}

/// A request's connection of its own to the fake service at `listener`,
/// once it said hello and heard that the service is process 1, as the
/// gateway's bridge did.
fn private_request(listener: &std::os::unix::net::UnixListener) -> BufReader<UnixStream> {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + crate::testsupport::scaled_timeout(Duration::from_secs(5));
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("no request came on a connection of its own: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(crate::testsupport::scaled_timeout(Duration::from_secs(30)))).unwrap();
    assert!(matches!(read_msg::<_, ClientMsg>(&mut stream).unwrap(), Some(ClientMsg::Hello { .. })));
    write_msg(&mut stream, &ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: String::new() })
        .unwrap();
    BufReader::new(stream)
}

/// A gateway that drives the host "fedora", tag 1.
fn fedora_hub(dir: &crate::testsupport::TempDir) -> Arc<Hub> {
    let mut opts = crate::remote::tests::hub_options(dir, vec![]);
    opts.hosts.push(HostConfig {
        role: Default::default(),
        name: "fedora".into(),
        label: Some("Fedora".into()),
        tag: 1,
        addresses: vec![],
        ssh: None,
        fingerprint: String::new(),
    });
    let hub = Arc::new(Hub::new(opts, "e".into()));
    hub.on_service(ServerMsg::Hosts {
        hosts: vec![HostInfo {
            role: Default::default(),
            name: "fedora".into(),
            label: "Fedora".into(),
            online: true,
            reason: None,
            start_dir: String::new(),
            stats: None,
        }],
    });
    hub
}

/// How a request reached the fake service: on the gateway's shared bridge or
/// on a connection of its own.
enum Via {
    Bridge,
    Alone(UnixStream),
}

/// A service on the gateway's bridge and at its socket, passing on each
/// request with the way back to its sender; the closure stops it. It names
/// itself, as process 1, on the bridge and on each connection of its own, as
/// the service does, and has published an empty state.
fn fake_service(hub: &Arc<Hub>) -> (mpsc::Receiver<(ClientMsg, Via)>, impl FnOnce()) {
    use std::os::unix::net::UnixListener;
    let socket = hub.opts.socket.clone();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let (bridge, peer) = UnixStream::pair().unwrap();
    *lock(&hub.service) = Some(bridge);
    let welcome = || ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: String::new() };
    hub.on_service(welcome());
    hub.on_service(ServerMsg::State(DeckState::default()));
    let (tx, rx) = mpsc::channel();
    let bridged = tx.clone();
    let bridging = thread::spawn(move || {
        let mut peer = BufReader::new(peer);
        while let Ok(Some(msg)) = read_msg::<_, ClientMsg>(&mut peer) {
            let _ = bridged.send((msg, Via::Bridge));
        }
    });
    let accepting = thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            if !matches!(read_msg::<_, ClientMsg>(&mut stream), Ok(Some(ClientMsg::Hello { .. }))) {
                break;
            }
            let _ = write_msg(&mut stream, &welcome());
            let Ok(Some(msg)) = read_msg::<_, ClientMsg>(&mut stream) else { break };
            let _ = tx.send((msg, Via::Alone(stream)));
        }
    });
    let hub = Arc::clone(hub);
    let stop = move || {
        *lock(&hub.service) = None;
        let _ = UnixStream::connect(&socket);
        bridging.join().unwrap();
        accepting.join().unwrap();
    };
    (rx, stop)
}

fn reply_via(hub: &Hub, via: &mut Via, msg: ServerMsg) {
    match via {
        Via::Bridge => hub.on_service(msg),
        // A request that gave up has closed its connection already.
        Via::Alone(stream) => {
            let _ = write_msg(stream, &msg);
        }
    }
}

/// The phone's reply to request `rid`, past what the gateway pushes, such as
/// its hello.
fn phone_reply(rx: &mpsc::Receiver<String>, rid: u64, wait: Duration) -> Value {
    let deadline = Instant::now() + crate::testsupport::scaled_timeout(wait);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let reply: Value = serde_json::from_str(&rx.recv_timeout(left).expect("the phone's reply")).unwrap();
        if reply["rid"] == json!(rid) {
            return reply;
        }
    }
}

#[test]
fn a_phone_resume_keeps_the_conversations_host() {
    let dir = crate::testsupport::TempDir::new("phone-host-resume");
    let hub = fedora_hub(&dir);
    let (requests, stop) = fake_service(&hub);
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("test-phone", tx.clone(), None, None);
    let request = serde_json::from_value(json!({"t":"resume","rid":1,"tool":"grok",
        "conversation":"11111111-1111-4111-8111-111111111111","host":"fedora"}))
    .unwrap();
    hub.handle(conn, request, &tx);
    let (msg, mut via) = requests.recv_timeout(crate::testsupport::scaled_timeout(Duration::from_secs(2))).unwrap();
    assert!(matches!(msg, ClientMsg::Resume { host: Some(ref name), .. } if name == "fedora"), "{msg:?}");
    reply_via(&hub, &mut via, ServerMsg::Created { id: crate::protocol::host_id(1, 3), request: None });
    hub.on_service(ServerMsg::State(DeckState {
        sessions: vec![search_session(crate::protocol::host_id(1, 3), Tool::Grok, None, Some("fedora"))],
        ..Default::default()
    }));
    let reply = phone_reply(&rx, 1, Duration::from_secs(2));
    assert_eq!(reply["id"], crate::protocol::host_id(1, 3), "{reply}");
    stop();
}

#[test]
fn a_late_reply_to_a_phone_request_that_gave_up_does_not_answer_the_next() {
    // ROOT-033: a resume the service answered after the phone's wait ended
    // was taken for the next phone's start, which named the wrong session.
    let dir = crate::testsupport::TempDir::new("phone-late-reply");
    let hub = fedora_hub(&dir);
    let (requests, stop) = fake_service(&hub);
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("test-phone", tx.clone(), None, None);
    let wait = crate::testsupport::scaled_timeout(SERVICE_REPLY * 2);
    let resume = serde_json::from_value(json!({"t":"resume","rid":1,"tool":"claude",
        "conversation":"11111111-1111-4111-8111-111111111111","host":"fedora"}))
    .unwrap();
    hub.handle(conn, resume, &tx);
    let (_, mut resumed) = requests.recv_timeout(wait).unwrap();
    let gave_up = phone_reply(&rx, 1, SERVICE_REPLY * 2);
    assert_eq!(gave_up["t"], "err", "{gave_up}");

    let start = serde_json::from_value(json!({"t":"new","rid":2,"tool":"claude","host":"fedora"})).unwrap();
    hub.handle(conn, start, &tx);
    let (msg, mut started) = requests.recv_timeout(wait).unwrap();
    assert!(matches!(msg, ClientMsg::Create { .. }), "{msg:?}");
    reply_via(&hub, &mut resumed, ServerMsg::Created { id: crate::protocol::host_id(1, 3), request: None });
    reply_via(&hub, &mut started, ServerMsg::Created { id: crate::protocol::host_id(1, 4), request: None });
    // Both sessions are published, so a start that took the late reply would
    // name the resumed session rather than wait out the publication.
    hub.on_service(ServerMsg::State(DeckState {
        sessions: [3, 4]
            .map(|id| search_session(crate::protocol::host_id(1, id), Tool::Claude, None, Some("fedora")))
            .to_vec(),
        ..Default::default()
    }));
    let reply = phone_reply(&rx, 2, Duration::from_secs(2));
    assert_eq!(reply["id"], crate::protocol::host_id(1, 4), "{reply}");
    stop();
}

/// The phone must receive the same active/recent children as the desktop.
#[test]
fn phone_sessions_preserve_native_subagents_and_seconds() {
    let mut session = search_session(1, Tool::Claude, None, None);
    session.subagents = vec![
        crate::protocol::Subagent {
            name: "Inspect parser".into(),
            kind: Some("Explore".into()),
            active: true,
            started_at: 1_700_000_010,
            last_active: 1_700_000_100,
        },
        crate::protocol::Subagent {
            name: "Review parser tests".into(),
            kind: None,
            active: false,
            started_at: 1_700_000_020,
            last_active: 1_700_000_090,
        },
    ];
    let state = DeckState { sessions: vec![session], ..Default::default() };
    let sessions = crate::remote::hub::phone_sessions(&state, &[], &[]);
    assert_eq!(
        sessions[0]["subagents"],
        json!([
            {"name":"Inspect parser","kind":"Explore","active":true,
                "started_at":1_700_000_010,"last_active":1_700_000_100},
            {"name":"Review parser tests","kind":null,"active":false,
                "started_at":1_700_000_020,"last_active":1_700_000_090}
        ])
    );
}

fn search_session(id: u64, tool: Tool, conversation: Option<&str>, host: Option<&str>) -> SessionInfo {
    SessionInfo {
        spawned_ms: 1000 + id,
        account_root: None,
        metadata: Default::default(),
        id,
        tool,
        title: String::new(),
        status: Status::Idle,
        detail: String::new(),
        pid: None,
        conversation: conversation.map(str::to_string),
        worktree: None,
        containers: Vec::new(),
        review: false,
        alive: true,
        has_screen: true,
        created_at: 0,
        status_since: 0,
        subagents: Vec::new(),
        activity: None,
        last_active: 0,
        cwd: None,
        job: None,
        host: host.map(str::to_string),
    }
}

/// The reply to the phone's request `rid` among what the connection was sent.
fn reply(rx: &mpsc::Receiver<String>, rid: u64) -> Value {
    loop {
        let text = rx.recv_timeout(Duration::from_secs(5)).expect("a reply");
        let v: Value = serde_json::from_str(&text).unwrap();
        if v["rid"] == json!(rid) {
            return v;
        }
    }
}

/// Owner, 2026-11-02: "i need to be able to search conversations, just
/// like to type in "agy" and find which one i gave a particular instruction
/// to". A Claude session is searched in its transcript, an Antigravity one
/// on its screen and one on a host out of reach in the chat the hub holds.
#[test]
fn a_phone_search_names_each_session_holding_the_words() {
    let dir = crate::testsupport::TempDir::new("phone-search");
    let mut opts = crate::remote::tests::hub_options(&dir, vec![]);
    opts.hosts.push(HostConfig {
        role: Default::default(),
        name: "fedora".into(),
        label: Some("Fedora".into()),
        tag: 1,
        addresses: vec![],
        ssh: None,
        fingerprint: String::new(),
    });
    let claude_id = "11111111-1111-4111-8111-111111111111";
    let transcript = crate::claude::transcript_path(&opts.claude_dir, "/work/site", claude_id);
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let records = [
        json!({"type": "user", "timestamp": "2026-11-01T10:00:00Z",
            "message": {"role": "user", "content": "the agent deck should start AGY agents too"}}),
        json!({"type": "user", "timestamp": "2026-11-01T10:05:00Z",
            "message": {"role": "user", "content": "now fix the parser"}}),
    ];
    std::fs::write(&transcript, records.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
    let hub = Arc::new(Hub::new(opts, "e".into()));
    lock(&hub.inner).start_dir = Some(PathBuf::from("/work/site"));
    let remote = crate::protocol::host_id(1, 5);
    hub.on_service(ServerMsg::State(DeckState {
        sessions: vec![
            search_session(1, Tool::Claude, Some(claude_id), None),
            search_session(2, Tool::Antigravity, None, None),
            search_session(3, Tool::Claude, Some("33333333-3333-4333-8333-333333333333"), None),
            search_session(remote, Tool::Codex, Some("01a19c8a-50b2-7fd0-a169-0be1ff3e0044"), Some("fedora")),
        ],
        ..DeckState::default()
    }));
    lock(&hub.chats).apply(
        remote,
        chat::Chunk {
            kind: chat::ChunkKind::Log,
            generation: 0,
            dropped: 0,
            start: 0,
            items: vec![Item::User { client_id: None, text: "agy on fedora".into(), images: Vec::new(), at: None }],
            seen: chat::ModelSeen::default(),
            questions: Some(Vec::new()),
            question_replies: Vec::new(),
        },
    );
    let (socket, peer) = UnixStream::pair().unwrap();
    *lock(&hub.service) = Some(socket);
    let service = Arc::clone(&hub);
    let screens = thread::spawn(move || {
        let mut peer = BufReader::new(peer);
        let mut asked = Vec::new();
        while let Ok(Some(msg)) = read_msg::<_, ClientMsg>(&mut peer) {
            if let ClientMsg::Screen { id, .. } = msg {
                asked.push(id);
                let lines = vec!["> start agy here".to_string(), String::new()];
                service.on_service(ServerMsg::ScreenText { id, lines, inputs: 0 });
            }
        }
        asked
    });
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("test-phone", tx.clone(), None, None);
    hub.handle(conn, serde_json::from_value(json!({"t": "search", "rid": 1, "q": "Agy"})).unwrap(), &tx);
    let found = reply(&rx, 1);
    assert_eq!(found["t"], "search", "{found}");
    assert_eq!(found["q"], "Agy");
    let hits = found["hits"].as_array().unwrap();
    let ids: Vec<u64> = hits.iter().map(|h| h["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, [1, remote, 2], "{found}");
    assert_eq!(hits[0]["mine"], 1);
    assert_eq!(hits[0]["snippets"][0]["role"], "user");
    assert_eq!(hits[0]["snippets"][0]["text"], "the agent deck should start AGY agents too");
    assert_eq!(hits[1]["snippets"][0]["text"], "agy on fedora");
    assert_eq!(hits[2]["snippets"][0]["role"], "screen");
    assert_eq!(hits[2]["snippets"][0]["text"], "> start agy here");

    hub.handle(conn, serde_json::from_value(json!({"t": "search", "rid": 2, "q": "  "})).unwrap(), &tx);
    let refused = reply(&rx, 2);
    assert_eq!(refused["t"], "err", "{refused}");

    *lock(&hub.service) = None;
    assert_eq!(screens.join().unwrap(), [2], "only the screen-only session's screen is read");
}

#[test]
fn host_operation_notices_complete_the_phone_request() {
    let source = ReplySource { tag: Some(1), notice_prefix: Some("Fedora: ".into()) };
    for (want, text, error) in [
        (Want::Created, "Could not start: unavailable", true),
        (Want::Reopened(3), "Could not reopen: unavailable", true),
        (Want::Resumed, "Could not resume: unavailable", true),
        (Want::Sent, "Sent to 1 session.", false),
        (Want::Sent, "Type a message first.", true),
    ] {
        assert!(matches(want, &source, &ServerMsg::Notice { text: format!("Fedora: {text}"), error }), "{want:?}");
        assert!(!matches(want, &source, &ServerMsg::Notice { text: format!("Controller: {text}"), error }));
        assert!(!matches(want, &source, &ServerMsg::Notice { text: text.into(), error }));
        assert!(matches(want, &ReplySource::default(), &ServerMsg::Notice { text: text.into(), error }));
    }
    for want in [Want::Created, Want::Resumed] {
        assert!(matches(want, &source, &ServerMsg::Created { id: crate::protocol::host_id(1, 3), request: None }));
        assert!(!matches(want, &source, &ServerMsg::Created { id: 3, request: None }));
        assert!(!matches(want, &source, &ServerMsg::Created { id: crate::protocol::host_id(2, 3), request: None }));
    }
}

#[test]
fn concurrent_phone_sends_keep_out_of_order_receipts_separate() {
    let dir = crate::testsupport::TempDir::new("phone-send-receipts");
    let hub = Arc::new(Hub::new(crate::remote::tests::hub_options(&dir, vec![]), "e".into()));
    hub.on_service(ServerMsg::State(DeckState::default()));
    let (socket, peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    *lock(&hub.service) = Some(socket);
    let mut reader = BufReader::new(peer);
    for version in [12, 13, 18] {
        lock(&hub.inner).service_version = version;
        assert!(hub.type_message(9, "native only").unwrap_err().to_string().contains("Update needed"));
        assert!(hub.type_broadcast(9, "native only").unwrap_err().to_string().contains("Update needed"));
    }
    lock(&hub.inner).service_version = crate::protocol::PROTOCOL_VERSION;
    let senders: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|text| {
            let hub = Arc::clone(&hub);
            thread::spawn(move || (text, hub.type_message(9, text)))
        })
        .collect();
    let mut requests = HashMap::new();
    for _ in 0..2 {
        let Some(ClientMsg::SendMessage { id: 9, request, text, .. }) = read_msg(&mut reader).unwrap() else {
            panic!("expected a correlated send");
        };
        requests.insert(text, request);
    }
    hub.on_service(ServerMsg::MessageSent {
        id: 9,
        request: requests["second"],
        error: Some("second refused".into()),
        delivered: None,
    });
    hub.on_service(ServerMsg::MessageSent {
        id: 9,
        request: u64::MAX,
        error: Some("late old failure".into()),
        delivered: None,
    });
    hub.on_service(ServerMsg::MessageSent { id: 9, request: requests["first"], error: None, delivered: None });
    for sender in senders {
        let (text, result) = sender.join().unwrap();
        if text == "first" {
            assert!(result.is_ok());
        } else {
            assert_eq!(result.unwrap_err().to_string(), "second refused");
        }
    }
}

/// An old card uses the same native answer handshake and persisted receipt as
/// a recent one, even though no ordinary retained row contains its identity.
#[test]
fn an_aged_out_question_is_sent_to_native_controls_and_confirmed_from_its_receipt() {
    use std::io::Write;
    let dir = crate::testsupport::TempDir::new("aged-native-question");
    let opts = crate::remote::tests::hub_options(&dir, vec![]);
    let conversation = "11111111-1111-4111-8111-111111111111";
    let path = opts.codex_dir.join(format!("sessions/2026/11/06/rollout-2026-11-06T00-00-00-{conversation}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let record = |item: Value| json!({"type":"event_msg","payload":{"type":"item_completed","item":item}});
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(
        file,
        "{}",
        record(json!({"type":"AgentMessage","id":"old",
        "content":[{"type":"Text","text":"Choose"}],
        "questions":[{"title":"An old question?","options":["Yes","No"]}]}))
    )
    .unwrap();
    for index in 0..3_001 {
        writeln!(
            file,
            "{}",
            record(json!({"type":"AgentMessage","id":format!("new-{index}"),
            "content":[{"type":"Text","text":"Later history"}]}))
        )
        .unwrap();
    }
    drop(file);
    let hub = Arc::new(Hub::new(opts, "e".into()));
    hub.on_service(ServerMsg::State(DeckState {
        sessions: vec![SessionInfo { pid: Some(42), ..search_session(1, Tool::Codex, Some(conversation), None) }],
        ..DeckState::default()
    }));
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("test-phone", tx.clone(), None, None);
    let page = hub.chat_page(1, None, None, Some(conn)).unwrap();
    assert_eq!(page["questions"][0]["id"], "old:0");
    assert!(
        page["items"].as_array().unwrap().iter().all(|item| item["questions"].is_null()),
        "the question card is independent of indexed ordinary rows"
    );

    let (socket, peer) = UnixStream::pair().unwrap();
    *lock(&hub.service) = Some(socket);
    let service = Arc::clone(&hub);
    let worker = thread::spawn(move || {
        let mut peer = BufReader::new(peer);
        let mut answered = 0;
        while let Ok(Some(msg)) = read_msg::<_, ClientMsg>(&mut peer) {
            match msg {
                ClientMsg::Screen { id, .. } => {
                    service.on_service(ServerMsg::ScreenText {
                        id,
                        inputs: 7,
                        lines: [
                            "An old question?",
                            "",
                            "› 1. Yes",
                            "  2. No",
                            "  3. Other (write an answer)",
                            "",
                            "enter submit   ⌃] skip   ⇧→ main prompt",
                        ]
                        .map(String::from)
                        .to_vec(),
                    });
                }
                ClientMsg::Answer { id, inputs, choice, .. } => {
                    assert_eq!(inputs, 7);
                    assert_eq!(choice, 1);
                    answered += 1;
                    let receipt = format!(
                        "<send_user_message_question_reply>{}</send_user_message_question_reply>",
                        json!([{"question":"An old question?","answer":"No",
                            "questionItemId":json!(["request_user_input_async","old",0]).to_string()}])
                    );
                    let record = json!({"type":"event_msg","payload":{"type":"item_completed","item":{
                        "type":"UserMessage","id":"receipt","content":[{"type":"Text","text":receipt}]}}});
                    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
                    writeln!(file, "{record}").unwrap();
                    service.on_service(ServerMsg::Answered { id, error: None, stale: false });
                }
                other => panic!("an old native card must keep native input guards: {other:?}"),
            }
        }
        answered
    });
    hub.handle(
        conn,
        serde_json::from_value(json!({"t":"question_reply","rid":1,
        "id":1,"question":"old:0","choice":1}))
        .unwrap(),
        &tx,
    );
    let result = reply(&rx, 1);
    assert!(result.get("error").is_none(), "{result}");
    assert!(hub.chat_page(1, None, None, None).unwrap()["questions"].as_array().unwrap().is_empty());
    *lock(&hub.service) = None;
    assert_eq!(worker.join().unwrap(), 1);
}

#[test]
fn phone_answers_duplicate_questions_by_identity_without_reading_or_typing_the_terminal() {
    use std::io::Write;
    for replacement in ["none", "process", "connection"] {
        let dir = crate::testsupport::TempDir::new("structured-phone-question");
        let opts = crate::remote::tests::hub_options(&dir, vec![]);
        let conversation = "11111111-1111-4111-8111-111111111111";
        let path = opts.codex_dir.join(format!("sessions/2026/11/06/rollout-2026-11-06T00-00-00-{conversation}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let record = json!({"type":"event_msg","payload":{"type":"item_completed","item":{
            "type":"AgentMessage","id":"ask","content":[{"type":"Text","text":"Choose"}],
            "questions":[{"title":"Same question?","options":["Yes","No"]},
                         {"title":"Same question?","options":["Yes","No"]}]}}});
        std::fs::write(&path, format!("{record}\n")).unwrap();
        let hub = Arc::new(Hub::new(opts, "e".into()));
        let original = SessionInfo { pid: Some(42), ..search_session(1, Tool::Codex, Some(conversation), None) };
        let expected = crate::attention::ExpectedTarget::from_session(&original).unwrap();
        hub.on_service(ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: String::new() });
        hub.on_service(ServerMsg::State(DeckState { sessions: vec![original.clone()], ..Default::default() }));
        let (bridge, peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(crate::testsupport::scaled_timeout(Duration::from_secs(3)))).unwrap();
        *lock(&hub.service) = Some(bridge);
        let service = Arc::clone(&hub);
        let worker = thread::spawn(move || {
            let mut peer = BufReader::new(peer);
            let mut native_writes = 0;
            while let Ok(Some(message)) = read_msg::<_, ClientMsg>(&mut peer) {
                match message {
                    ClientMsg::PluginList { request } => {
                        if replacement != "none" {
                            let mut changed = original.clone();
                            if replacement == "process" {
                                changed.pid = Some(43);
                                changed.spawned_ms += 1;
                            } else {
                                service.on_service(ServerMsg::Welcome {
                                    version: PROTOCOL_VERSION,
                                    pid: 1,
                                    start_dir: String::new(),
                                });
                            }
                            service.on_service(ServerMsg::State(DeckState {
                                sessions: vec![changed],
                                ..Default::default()
                            }));
                        }
                        service.on_service(ServerMsg::PluginResult { request,
                            data: json!({"plugins":[{"id":"provider-codex","enabled":true,"commands":[{"name":"control"}]}]}), error: None });
                    }
                    ClientMsg::Plugin { request, plugin, command, args } => {
                        assert_eq!(replacement, "none", "replacement received a stale native question answer");
                        assert_eq!((plugin.as_str(), command.as_str()), ("provider-codex", "control"));
                        assert_eq!(
                            args,
                            json!({"id":1,"host":null,"conversation":conversation,"expected":expected,
                            "action":"question_reply","question":"ask:1","answer":"No"})
                        );
                        native_writes += 1;
                        let receipt = super::super::questionpick::follow_up_text(
                            &chat::ChatQuestion {
                                id: "ask:1".into(),
                                title: "Same question?".into(),
                                options: vec!["Yes".into(), "No".into()],
                                answered: false,
                            },
                            "No",
                        )
                        .unwrap();
                        let receipt = json!({"type":"event_msg","payload":{"type":"item_completed","item":{
                            "type":"UserMessage","content":[{"type":"Text","text":receipt}]}}});
                        writeln!(std::fs::OpenOptions::new().append(true).open(&path).unwrap(), "{receipt}").unwrap();
                        service.on_service(ServerMsg::PluginResult {
                            request,
                            data: json!({"supported":true,"accepted":true}),
                            error: None,
                        });
                        break;
                    }
                    other => panic!("native controls must preserve the terminal: {other:?}"),
                }
            }
            native_writes
        });
        let (tx, rx) = mpsc::channel();
        let conn = hub.add_conn("test-phone", tx.clone(), None, None);
        hub.handle(
            conn,
            serde_json::from_value(json!({"t":"question_reply","rid":1,"id":1,"question":"ask:1","choice":1})).unwrap(),
            &tx,
        );
        let result = reply(&rx, 1);
        *lock(&hub.service) = None;
        assert_eq!(worker.join().unwrap(), usize::from(replacement == "none"));
        if replacement == "none" {
            assert!(result.get("error").is_none(), "{result}");
            let page = hub.chat_page(1, None, None, None).unwrap();
            assert_eq!(page["questions"].as_array().unwrap().len(), 1);
            assert_eq!(page["questions"][0]["id"], "ask:0");
        } else {
            assert!(result.get("error").is_some(), "{replacement}: {result}");
        }
    }
}

/// Owner, 2026-11-08: "it says 'this question is already answered or no
/// longer belongs to this conversation' and there is no way out". Each refusal
/// of a question that no longer waits carries a code the phone reads instead
/// of its words; a card the conversation's own transcript refused is no
/// longer offered, and nothing is typed or sent in its place.
#[test]
fn stale_question_refusals_carry_a_code_and_withdraw_a_refused_card() {
    let dir = crate::testsupport::TempDir::new("stale-phone-question");
    let opts = crate::remote::tests::hub_options(&dir, vec![]);
    let conversation = "11111111-1111-4111-8111-111111111111";
    let path = opts.codex_dir.join(format!("sessions/2026/11/06/rollout-2026-11-06T00-00-00-{conversation}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let record = json!({"type":"event_msg","payload":{"type":"item_completed","item":{
        "type":"AgentMessage","id":"old","content":[{"type":"Text","text":"Choose"}],
        "questions":[{"title":"Old question?","options":["Yes","No"]},
                     {"title":"Other question?","options":["A","B"]}]}}});
    std::fs::write(&path, format!("{record}\n")).unwrap();
    let hub = Arc::new(Hub::new(opts, "e".into()));
    let session = SessionInfo { pid: Some(42), ..search_session(1, Tool::Codex, Some(conversation), None) };
    hub.on_service(ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: String::new() });
    hub.on_service(ServerMsg::State(DeckState { sessions: vec![session], ..Default::default() }));
    let (bridge, peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(crate::testsupport::scaled_timeout(Duration::from_secs(3)))).unwrap();
    *lock(&hub.service) = Some(bridge);
    let service = Arc::clone(&hub);
    let worker = thread::spawn(move || {
        let mut peer = BufReader::new(peer);
        let (mut controls, mut answers) = (0, 0);
        while let Ok(Some(message)) = read_msg::<_, ClientMsg>(&mut peer) {
            match message {
                ClientMsg::PluginList { request } => service.on_service(ServerMsg::PluginResult {
                    request,
                    data: json!({"plugins":[{"id":"provider-codex","enabled":true,"commands":[{"name":"control"}]}]}),
                    error: None,
                }),
                ClientMsg::Plugin { request, args, .. } => {
                    assert_eq!(args["question"], "old:0");
                    controls += 1;
                    service.on_service(ServerMsg::PluginResult {
                        request,
                        data: json!({"code": crate::protocol::QUESTION_STALE}),
                        error: Some(
                            "this question is already answered or no longer belongs to this conversation".into(),
                        ),
                    });
                }
                // A waiting prompt the service no longer shows, then one
                // refused for another reason.
                ClientMsg::Answer { id, .. } => {
                    answers += 1;
                    let (error, stale) = if answers == 1 {
                        ("no question is waiting", true)
                    } else {
                        ("the session took other input since the question was read; look again", false)
                    };
                    service.on_service(ServerMsg::Answered { id, error: Some(error.into()), stale });
                }
                other => panic!("a refused answer must not reach the terminal: {other:?}"),
            }
        }
        (controls, answers)
    });
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("test-phone", tx.clone(), None, None);
    assert_eq!(hub.chat_page(1, None, None, Some(conn)).unwrap()["questions"].as_array().unwrap().len(), 2);
    let ask = |rid: u64, body: Value| {
        let mut request = json!({"rid": rid, "id": 1});
        request.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        hub.handle(conn, serde_json::from_value(request).unwrap(), &tx);
        reply(&rx, rid)
    };

    let refused = ask(1, json!({"t":"question_reply","question":"old:0","choice":0}));
    assert_eq!(refused["code"], crate::protocol::QUESTION_STALE, "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("no longer belongs to this conversation"), "{refused}");
    let questions = hub.chat_page(1, None, None, None).unwrap()["questions"].clone();
    assert_eq!(questions.as_array().unwrap().len(), 1, "{questions}");
    assert_eq!(questions[0]["id"], "old:1");
    assert_eq!(questions[0]["submitted"], false, "a refused answer is not reported as sent");

    // The gateway's own refusals, before anything reaches the service.
    let gone = ask(2, json!({"t":"question_reply","question":"gone:0","choice":0}));
    assert_eq!(gone["code"], crate::protocol::QUESTION_STALE, "{gone}");
    lock(&hub.inner).answered_questions.insert((1, "old:1".into()));
    let again = ask(3, json!({"t":"question_reply","question":"old:1","choice":0}));
    assert_eq!(again["code"], crate::protocol::QUESTION_STALE, "{again}");
    lock(&hub.inner).answered_questions.clear();
    let invalid = ask(4, json!({"t":"question_reply","question":"old:1","choice":9}));
    assert!(invalid["error"].is_string() && invalid.get("code").is_none(), "{invalid}");

    // A waiting prompt's refusal is coded only when the service says so.
    let prompt = ask(5, json!({"t":"answer","inputs":3,"fp":"read","choice":0}));
    assert_eq!(prompt["code"], crate::protocol::QUESTION_STALE, "{prompt}");
    assert_eq!(prompt["error"], "no question is waiting");
    let busy = ask(6, json!({"t":"answer","inputs":3,"fp":"read","choice":0}));
    assert!(busy["error"].is_string() && busy.get("code").is_none(), "{busy}");

    *lock(&hub.service) = None;
    assert_eq!(worker.join().unwrap(), (1, 2));
}

#[test]
fn question_snapshots_reach_followers_without_new_rows_or_a_generation_change() {
    let dir = crate::testsupport::TempDir::new("question-snapshot");
    let hub = Arc::new(Hub::new(crate::remote::tests::hub_options(&dir, vec![]), "e".into()));
    let id = crate::protocol::host_id(1, 5);
    hub.on_service(ServerMsg::State(DeckState {
        sessions: vec![search_session(
            id,
            Tool::Codex,
            Some("11111111-1111-4111-8111-111111111111"),
            Some("offline-host"),
        )],
        ..DeckState::default()
    }));
    let question = chat::ChatQuestion {
        id: "old:0".into(),
        title: "Still pending?".into(),
        options: vec!["Yes".into(), "No".into()],
        answered: false,
    };
    let chunk = |questions| chat::Chunk {
        kind: chat::ChunkKind::Log,
        generation: 1,
        dropped: 3_000,
        start: 3_000,
        items: Vec::new(),
        seen: chat::ModelSeen::default(),
        questions: Some(questions),
        question_replies: Vec::new(),
    };
    lock(&hub.chats).apply(id, chunk(vec![question]));
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("test-phone", tx.clone(), None, None);
    while rx.try_recv().is_ok() {}
    hub.handle(conn, Req::Open { rid: 1, id: Some(id), limit: None }, &tx);
    let page = reply(&rx, 1);
    assert_eq!(page["questions"][0]["id"], "old:0");
    let mut replies = super::super::live::ReplyState::default();
    hub.update_chat(id, None, &mut replies);
    let update: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
    assert_eq!(update["questions"][0]["id"], "old:0");
    assert_eq!(update["items"], json!([]));
    lock(&hub.chats).apply(id, chunk(Vec::new()));
    hub.update_chat(id, None, &mut replies);
    let resolved: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
    assert_eq!(resolved["questions"], json!([]));
    assert_eq!(resolved["items"], json!([]));
    assert_eq!(resolved["generation"], update["generation"]);
    assert_eq!(resolved["total"], update["total"]);
}

/// Three physical sources deliberately share a conversation UUID. Only source
/// ownership, never the viewer's local logs or host tags, selects its content.
#[test]
fn automatic_phone_start_reports_selected_host_failure_without_losing_the_request() {
    assert!(matches(
        Want::Created,
        &ReplySource::default(),
        &ServerMsg::Notice { text: "Ubuntu: Could not start: provider unavailable".into(), error: true }
    ));
    assert!(!matches(
        Want::Created,
        &ReplySource::default(),
        &ServerMsg::Notice { text: "Ubuntu: another background notice".into(), error: true }
    ));
}

#[test]
fn shared_phone_reorder_reaches_the_coordinator_and_state_controls_display() {
    use std::os::unix::net::UnixListener;
    let dir = crate::testsupport::TempDir::new("shared-phone-order");
    let opts = crate::remote::tests::hub_options(&dir, vec![]);
    let socket = opts.socket.clone();
    let order_file = opts.order_file.clone();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let local = search_session(1, Tool::Claude, None, None);
    let id = crate::protocol::host_id(2, 1);
    let remote = search_session(id, Tool::Claude, None, Some("ubuntu"));
    let state = DeckState { sessions: vec![local, remote], session_order: vec![id, 1], ..Default::default() };
    let hub = Arc::new(Hub::new(opts, "e".into()));
    hub.on_service(ServerMsg::Welcome { version: 14, pid: 1, start_dir: String::new() });
    hub.on_service(ServerMsg::State(state.clone()));
    assert_eq!(hub.order_for(&state), [id, 1]);
    let (sent, received) = mpsc::channel();
    let serving = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        assert!(matches!(read_msg::<_, ClientMsg>(&mut socket).unwrap(), Some(ClientMsg::Hello { .. })));
        let request = read_msg::<_, ClientMsg>(&mut socket).unwrap().unwrap();
        sent.send(request).unwrap();
        write_msg(&mut socket, &ServerMsg::Notice { text: "Session order saved.".into(), error: false }).unwrap();
    });
    hub.reorder(&[1, id]).unwrap();
    assert!(matches!(received.recv().unwrap(),ClientMsg::SetOrder {ids} if ids==[1,id]));
    serving.join().unwrap();
    assert!(!order_file.exists(), "a viewer must not save its own competing order");
}

#[test]
fn phone_state_uses_service_welcome_and_native_rules_gate_before_transport() {
    use crate::protocol::{encode, write_msg};
    use std::os::unix::fs::PermissionsExt;
    for version in [0, 14, 15, 16, 17] {
        let dir = crate::testsupport::TempDir::new("phone-native-version");
        let opts = crate::remote::tests::hub_options(&dir, vec![]);
        std::fs::create_dir_all(opts.socket.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&opts.socket).unwrap();
        std::fs::set_permissions(&opts.socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let hub = Arc::new(Hub::new(opts, "version-epoch".into()));
        if version != 0 {
            let frame = encode(&ServerMsg::Welcome { version, pid: 42, start_dir: "/work".into() });
            hub.on_service(read_msg::<_, ServerMsg>(&mut frame.as_slice()).unwrap().unwrap());
        }
        let frame = encode(&ServerMsg::State(DeckState { attention_capable: true, ..Default::default() }));
        hub.on_service(read_msg::<_, ServerMsg>(&mut frame.as_slice()).unwrap().unwrap());
        let (tx, rx) = mpsc::channel();
        let client = hub.add_conn("version-phone", tx, None, None);
        let state =
            rx.try_iter().filter_map(|s| serde_json::from_str::<Value>(&s).ok()).find(|v| v["t"] == "state").unwrap();
        assert_eq!(state["protocol"], version, "gateway17 must not invent service capability");
        for (operation, min, args) in [
            (
                "attention-supervisor-rules",
                16,
                json!({"rules":[{"id":"read","tool":"Read","pattern":{"kind":"native_exact","input":{"file_path":"/work/source.rs"}}}]}),
            ),
            (
                "attention-send",
                17,
                json!({"key":"original","session":1,"host":"@here","text":"private","expected_target":{"spawned_ms":1001,"pid":1001,"conversation":null}}),
            ),
            ("attention-status", 15, json!({})),
            ("job-list", 14, json!({})),
            ("queue-list", 14, json!({})),
        ] {
            if version < min {
                let error = hub.control("fleet", json!({"operation":operation,"args":args})).unwrap_err().to_string();
                assert!(error.contains("Update needed"), "{version}/{operation}: {error}");
                assert!(
                    matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock),
                    "rejected before transport"
                );
            } else {
                let listener = listener.try_clone().unwrap();
                let op = operation.to_string();
                let expected = args.clone();
                let serve = thread::spawn(move || {
                    assert!(crate::session::readable(
                        std::os::fd::AsRawFd::as_raw_fd(&listener),
                        Duration::from_secs(2)
                    )
                    .unwrap());
                    let (mut socket, _) = listener.accept().unwrap();
                    socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let Some(ClientMsg::Hello { .. }) = read_msg::<_, ClientMsg>(&mut socket).unwrap() else {
                        panic!("hello expected")
                    };
                    write_msg(&mut socket, &ServerMsg::Welcome { version, pid: 42, start_dir: "/work".into() })
                        .unwrap();
                    let Some(ClientMsg::Fleet { request, action, args }) =
                        read_msg::<_, ClientMsg>(&mut socket).unwrap()
                    else {
                        panic!("fleet expected")
                    };
                    assert_eq!(action, op);
                    assert_eq!(args, expected);
                    write_msg(
                        &mut socket,
                        &ServerMsg::FleetResult { request, data: json!({"accepted":true}), error: None },
                    )
                    .unwrap();
                });
                let result = hub.control("fleet", json!({"operation":operation,"args":args}));
                serve.join().unwrap();
                assert_eq!(result.unwrap()["accepted"], true, "{version}/{operation}");
            }
        }
        hub.remove_conn(client);
    }
}

#[test]
fn a_phone_removed_in_the_deck_stays_removed_when_another_signs_in() {
    // ROOT-035: each sign-in saved the gateway's own copy of the list, which
    // put back a phone the deck's pairing screen had just removed.
    let dir = crate::testsupport::TempDir::new("phone-removed");
    let opts = crate::remote::tests::hub_options(&dir, vec![]);
    let remote = opts.dir.clone();
    let (pixel, pixel_key) = crate::remote::auth::new_device("Pixel", 1).unwrap();
    let (tablet, tablet_key) = crate::remote::auth::new_device("Tablet", 2).unwrap();
    store::save_devices(&remote, &[pixel.clone(), tablet.clone()]).unwrap();
    let hub = Hub::new(opts, "e".into());
    let mut screen = crate::pairing::Pairing::open(&remote, &Config::default(), 10);
    assert_eq!(screen.remove_selected().unwrap().map(|d| d.id), Some(pixel.id.clone()));

    assert!(hub.sign_in(&tablet.id, &tablet_key));
    let ids = || store::load_devices(&remote).into_iter().map(|d| d.id).collect::<Vec<_>>();
    assert_eq!(ids(), std::slice::from_ref(&tablet.id));
    assert!(store::load_devices(&remote)[0].last_seen.is_some(), "the sign-in is noted");
    assert!(!hub.sign_in(&pixel.id, &pixel_key), "the removed phone is refused");
    let (laptop, _) = crate::remote::auth::new_device("Laptop", 3).unwrap();
    hub.add_device(laptop.clone()).unwrap();
    assert_eq!(ids(), [tablet.id, laptop.id]);
}

#[path = "remote_request_content.rs"]
mod content;

#[test]
fn native_phone_new_waits_for_provider_preparation_past_ordinary_controls() {
    let dir = crate::testsupport::TempDir::new("phone-native-new-wait");
    let hub = fedora_hub(&dir);
    let (requests, stop) = fake_service(&hub);
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("test-phone", tx.clone(), None, None);
    hub.handle(
        conn,
        serde_json::from_value(json!({"t":"new","rid":1,"tool":"opencode","host":"fedora"})).unwrap(),
        &tx,
    );
    let (message, mut via) = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(message, ClientMsg::Create { tool: Tool::OpenCode, .. }));
    // A cold provider can finish after ordinary controls have timed out.
    thread::sleep(SERVICE_REPLY + Duration::from_millis(200));
    let id = crate::protocol::host_id(1, 4);
    reply_via(&hub, &mut via, ServerMsg::Created { id, request: None });
    hub.on_service(ServerMsg::State(DeckState {
        sessions: vec![search_session(id, Tool::OpenCode, Some("ses_native_new"), Some("fedora"))],
        ..Default::default()
    }));
    let reply = phone_reply(&rx, 1, Duration::from_secs(3));
    assert_eq!(reply["id"], id, "{reply}");
    assert!(requests.try_recv().is_err(), "New must not be retried");
    stop();
}
