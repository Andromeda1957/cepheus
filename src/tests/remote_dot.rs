use super::*;

fn reply(rx: &mpsc::Receiver<String>, rid: u64) -> Value {
    loop {
        let text = rx.recv_timeout(Duration::from_secs(5)).expect("a phone reply");
        let value: Value = serde_json::from_str(&text).unwrap();
        if value["rid"] == json!(rid) {
            return value;
        }
    }
}

#[test]
fn dot_phone_images_use_the_exact_provider_api_and_keep_uploads_on_refusal() {
    for (host, refused) in [(None, false), (None, true), (Some("remote"), false)] {
        let dir = crate::testsupport::TempDir::new("dot-phone-image");
        let hub = Arc::new(Hub::new(crate::remote::tests::hub_options(&dir, vec![]), "dot".into()));
        let id = if host.is_some() { crate::protocol::host_id(1, 7) } else { 7 };
        let mut original = crate::tui::tests::session(id, Tool::Dynamic("dot"), Status::Idle, true);
        original.pid = Some(42);
        original.conversation = None;
        original.host = host.map(str::to_string);
        hub.on_service(ServerMsg::Welcome { version: 19, pid: 1, start_dir: String::new() });
        if let Some(host) = host {
            hub.on_service(ServerMsg::Hosts {
                hosts: vec![HostInfo {
                    role: Default::default(),
                    name: host.into(),
                    label: "Remote".into(),
                    online: true,
                    ..Default::default()
                }],
            });
        }
        hub.on_service(ServerMsg::State(DeckState { sessions: vec![original.clone()], ..Default::default() }));
        let (socket, peer) = UnixStream::pair().unwrap();
        *lock(&hub.service) = Some(socket);
        let (tx, rx) = mpsc::channel();
        let conn = hub.add_conn("phone", tx.clone(), None, None);
        let bytes = b"\x89PNG\r\n\x1a\nfixture-image";
        hub.handle(
            conn,
            Req::Upload {
                rid: 1,
                id,
                name: "paste.png".into(),
                size: bytes.len() as u64,
                sha256: crate::remote::auth::sha256_hex(bytes),
            },
            &tx,
        );
        let token = reply(&rx, 1)["upload"].as_str().expect("Dot accepts the image upload").to_string();
        hub.piece(conn, Piece { rid: 2, upload: token.clone(), offset: 0 }, bytes, &tx);
        assert_eq!(reply(&rx, 2)["t"], "ok");
        let service = Arc::clone(&hub);
        let worker =
            thread::spawn(move || match read_msg::<_, ClientMsg>(&mut BufReader::new(peer)).unwrap().unwrap() {
                ClientMsg::Plugin { request, plugin, command, args } => {
                    assert_eq!((plugin.as_str(), command.as_str()), ("provider-dot", "send"));
                    assert_eq!(args["id"], 7);
                    assert_eq!(args["host"], json!(host));
                    assert_eq!(
                        args["expected"],
                        json!(crate::attention::ExpectedTarget::from_session(&original).unwrap())
                    );
                    assert_eq!(args["text"], "exact text");
                    let image: crate::dot::api::Image = serde_json::from_value(args["images"][0].clone()).unwrap();
                    assert_eq!(image.bytes().unwrap().0, bytes);
                    service.on_service(ServerMsg::PluginResult {
                        request,
                        data: json!({"id":"ack"}),
                        error: refused.then(|| "Dot needs an updated controller".into()),
                    });
                }
                other => panic!("Dot was routed through a terminal: {other:?}"),
            });
        hub.handle(
            conn,
            Req::Send {
                client_id: None,
                rid: 3,
                id,
                text: "exact text".into(),
                files: vec![token.clone()],
                relocate: false,
            },
            &tx,
        );
        let result = reply(&rx, 3);
        worker.join().unwrap();
        assert_eq!(result["t"], if refused { "err" } else { "ok" }, "{result}");
        assert_eq!(
            lock(&hub.uploads).paths("phone", id, &[token]).is_ok(),
            refused,
            "only a confirmed message consumes upload ownership"
        );
    }
}
