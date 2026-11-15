use super::*;

/// Send `text` with `images` as the phone gateway does, and return the
/// recipient's error and what the provider confirmed.
fn send_images(
    svc: &mut Service,
    events: &Receiver<Ev>,
    replies: &Receiver<Out>,
    id: u64,
    text: &str,
    images: Value,
) -> (Option<String>, Option<crate::protocol::Delivered>) {
    let client_id = crate::util::uuid_v4().unwrap();
    let session = &svc.sessions[&id];
    let expected = json!({"spawned_ms":session.spawned_ms,"pid":session.hosted.as_ref().unwrap().pid,
        "conversation":session.conversation});
    svc.handle_client(
        7,
        ClientMsg::Plugin {
            request: 1001,
            plugin: "provider-codex".into(),
            command: "target".into(),
            args: json!({"id":id,"expected":expected,"operation":"send","text":text,"images":images,"client_id":client_id}),
        },
    );
    let mut result = None;
    until(svc, events, |_| {
        while let Ok(Out::Msg(bytes)) = replies.try_recv() {
            match crate::protocol::read_msg(&mut bytes.as_slice()).unwrap() {
                Some(ServerMsg::PluginResult { request: 1001, error: Some(error), .. }) => {
                    result = Some((Some(error), None))
                }
                Some(ServerMsg::MessageSent { request: 1001, error, delivered, .. }) => {
                    result = Some((error, delivered))
                }
                _ => (),
            }
        }
        result.is_some()
    });
    let result = result.unwrap();
    if let Some(calls) =
        result.0.is_none().then(|| fs::read_to_string(svc.opts.start_dir.join("controls")).ok()).flatten()
    {
        let last: Value = serde_json::from_str(calls.lines().last().unwrap()).unwrap();
        assert_eq!(last["params"]["clientUserMessageId"], client_id);
    }
    result
}

#[test]
fn owner_phone_image_steers_active_turn_and_preserves_desktop_draft() {
    let (dir, mut svc, events, replies, id, _server) = fixture();
    let folder = svc.opts.state_dir.join("uploads").join(id.to_string());
    fs::create_dir_all(&folder).unwrap();
    let image = folder.join("photo.png");
    fs::write(&image, b"\x89PNG\r\n\x1a\nfixture").unwrap();
    svc.handle_client(7, ClientMsg::Input { id, data: "preserve desktop draft".into() });
    until(&mut svc, &events, |_| dir.path().join("typed").exists());
    let digest = crate::remote::auth::sha256_hex(b"\x89PNG\r\n\x1a\nfixture");
    let images = json!([[image, digest]]);
    let (error, delivered) = send_images(&mut svc, &events, &replies, id, "look at this", images);
    assert_eq!(error, None);
    assert_eq!(delivered, Some(crate::protocol::Delivered::Steered), "the thread is still working");
    let calls: Vec<Value> = fs::read_to_string(dir.path().join("controls"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["method"], "turn/start");
    assert_eq!(calls[0]["params"]["threadId"], THREAD);
    assert_eq!(
        calls[0]["params"]["input"],
        json!([
            {"type":"text","text":"look at this","text_elements":[]},
            {"type":"localImage","path":image}
        ])
    );
    assert!(!dir.path().join("woken").exists(), "active admission does not wake a separate turn");
    assert_eq!(fs::read_to_string(dir.path().join("typed")).unwrap(), "preserve desktop draft");
    svc.begin_shutdown(false);
}

#[test]
fn owner_image_alone_starts_an_idle_thread_and_changed_or_foreign_files_are_refused() {
    let (dir, mut svc, events, replies, id, _server) = fixture();
    dir.write("interrupted", "");
    let folder = svc.opts.state_dir.join("uploads").join(id.to_string());
    fs::create_dir_all(&folder).unwrap();
    let image = folder.join("photo.png");
    fs::write(&image, b"\x89PNG\r\n\x1a\nidle").unwrap();
    let digest = crate::remote::auth::sha256_hex(b"\x89PNG\r\n\x1a\nidle");
    let (error, delivered) = send_images(&mut svc, &events, &replies, id, "", json!([[image, digest]]));
    assert_eq!(error, None);
    assert_eq!(delivered, Some(crate::protocol::Delivered::Submitted));
    assert!(dir.path().join("woken").exists());
    let calls = fs::read_to_string(dir.path().join("controls")).unwrap();
    let call: Value = serde_json::from_str(calls.lines().next().unwrap()).unwrap();
    assert_eq!(call["params"]["input"], json!([{"type":"localImage","path":image}]), "no empty text part");

    // Bytes changed after the gateway hashed them, a file outside the upload
    // folder, and a file that is not an image all send nothing.
    let changed = crate::remote::auth::sha256_hex(b"\x89PNG\r\n\x1a\nbefore");
    let outside = dir.path().join("outside.png");
    fs::write(&outside, b"\x89PNG\r\n\x1a\nidle").unwrap();
    let text = folder.join("notes.txt");
    fs::write(&text, b"plain text").unwrap();
    let text_digest = crate::remote::auth::sha256_hex(b"plain text");
    for images in [json!([[image, changed]]), json!([[outside, digest]]), json!([[text, text_digest]])] {
        let (error, delivered) = send_images(&mut svc, &events, &replies, id, "look", images);
        assert!(error.is_some_and(|error| !error.is_empty()) && delivered.is_none());
    }
    assert_eq!(fs::read_to_string(dir.path().join("controls")).unwrap().lines().count(), 1);
    assert!(!dir.path().join("typed").exists(), "nothing is ever typed");
    svc.begin_shutdown(false);
}

#[test]
fn owner_image_verifies_remote_peer_bytes_and_refuses_mismatch() {
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};
    let dir = TempDir::new("owner-image-peer");
    let state = dir.path();
    let bytes = b"\x89PNG\r\n\x1a\nactual peer image";
    let image = crate::hosts::peer::keep_upload(&state.join("peer/uploads"), "phone.png", bytes).unwrap();
    let expected = crate::remote::auth::sha256_hex(bytes);
    let cancel = AtomicBool::new(false);
    let verify = |digest| {
        super::extensions::native::provider_target::owner_images(
            state,
            &[(image.clone(), digest)],
            Instant::now() + Duration::from_secs(5),
            &cancel,
        )
    };
    assert_eq!(verify(expected).unwrap(), vec![image.canonicalize().unwrap()]);
    assert!(verify(crate::remote::auth::sha256_hex(b"different image")).is_err());
    assert_eq!(fs::read(image).unwrap(), bytes);
}

#[test]
fn consecutive_owner_messages_reach_one_active_turn_in_order_without_pty_input() {
    let (dir, mut svc, events, replies, id, _server) = fixture();
    for text in ["first correction", "second correction", "third correction"] {
        let (error, delivered) = send_images(&mut svc, &events, &replies, id, text, json!([]));
        assert_eq!(error, None);
        assert_eq!(delivered, Some(crate::protocol::Delivered::Steered));
    }
    let calls: Vec<Value> = fs::read_to_string(dir.path().join("controls"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls
            .iter()
            .map(|v| v["params"]["clientUserMessageId"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    assert_eq!(
        calls.iter().map(|v| v["params"]["input"][0]["text"].as_str().unwrap()).collect::<Vec<_>>(),
        ["first correction", "second correction", "third correction"]
    );
    assert!(calls.iter().all(|v| v["method"] == "turn/start"));
    assert!(!dir.path().join("typed").exists());
    assert!(!dir.path().join("woken").exists());
    svc.begin_shutdown(false);
}

#[test]
fn native_controls_cannot_activate_queued_input_after_stop_close_or_conversation_change() {
    for boundary in ["stop", "close", "new", "replace", "shutdown", "unsupported-stop"] {
        let (dir, mut svc, events, replies, id, _server) = fixture();
        dir.write("interrupted", "");
        dir.write("pause-control-reply", "");
        dir.write("ignore-hup", "");
        if boundary == "unsupported-stop" {
            dir.write("unsupported-control", "");
        }
        let path = svc.opts.codex_dir.join(format!("sessions/2026/11/06/rollout-2026-11-06T00-00-00-{THREAD}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let record = json!({"type":"event_msg","payload":{"type":"item_completed","item":{
            "type":"AgentMessage","id":"ask","content":[{"type":"Text","text":"Choose"}],"questions":[{"title":"Continue?","options":["Yes","No"]}]}}});
        fs::write(&path, format!("{record}\n")).unwrap();
        svc.handle_client(7, ClientMsg::Plugin { request:900, plugin:"provider-codex".into(),command:"control".into(),
            args:json!({"id":id,"conversation":THREAD,"action":"question_reply","question":"ask:0","answer":"Yes"}) });
        // The daemon admitted the queue write, but its receipt is held. The
        // owner boundary must prevent the subsequent queue/start RPC.
        until(&mut svc, &events, |_| dir.path().join("controls").exists());
        match boundary {
            "stop" | "unsupported-stop" => svc.handle_client(7, ClientMsg::Input { id, data: "\x1b".into() }),
            "close" => svc.close(id),
            "new" => svc.handle_client(7, ClientMsg::Input { id, data: "/new\r".into() }),
            "replace" => {
                svc.sessions.get_mut(&id).unwrap().enter_conversation(Some("another-conversation".into()));
            }
            "shutdown" => svc.begin_shutdown(false),
            _ => unreachable!(),
        }
        fs::remove_file(dir.path().join("pause-control-reply")).unwrap();
        let mut result = None;
        until(&mut svc, &events, |_| {
            while let Ok(Out::Msg(bytes)) = replies.try_recv() {
                if let Some(ServerMsg::PluginResult { request: 900, data, error }) =
                    crate::protocol::read_msg(&mut bytes.as_slice()).unwrap()
                {
                    result = Some((data, error));
                }
            }
            result.is_some()
        });
        assert!(result.unwrap().1.is_some(), "{boundary} must cancel the pending control");
        assert!(!dir.path().join("woken").exists(), "{boundary} must not be followed by native activation");
        assert_eq!(fs::read_to_string(dir.path().join("controls")).unwrap().lines().count(), 1);
        fs::remove_file(dir.path().join("ignore-hup")).unwrap();
        svc.begin_shutdown(false);
    }
}
