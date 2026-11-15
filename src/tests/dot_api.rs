use super::*;
use crate::testsupport::TempDir;

fn provider(dir: &Path) -> crate::providers::Provider {
    serde_json::from_value(json!({"id":"dot","label":"Dot","command":["cepheus","dot","chat"],"cloud":true,"chat_snapshot":dir.join("transcript.json")})).unwrap()
}

#[test]
fn direct_socket_preserves_image_and_text_and_refuses_a_replaced_controller() {
    let dir = TempDir::new("dot-api");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let (seen, received) = mpsc::channel();
    let server = Server::start(dir.path(), Arc::new(AtomicBool::new(false)), move |call| {
        seen.send(call.message).unwrap();
        call.reply.send(Ok(json!({"id":"accepted"}))).unwrap();
        true
    })
    .unwrap();
    let provider = provider(dir.path());
    let mut wrong = OwnerRequest::prepare(&provider, std::process::id(), "do not send").unwrap();
    wrong.start_ticks += 1;
    assert!(wrong.deliver(Instant::now() + Duration::from_secs(2), &AtomicBool::new(false)).is_err());
    assert!(received.try_recv().is_err());
    let parent = crate::procs::stat(std::process::id()).unwrap().ppid;
    let wrong = OwnerRequest::prepare(&provider, parent, "do not send").unwrap();
    assert!(wrong.deliver(Instant::now() + Duration::from_secs(2), &AtomicBool::new(false)).is_err());
    assert!(received.try_recv().is_err());
    let bytes = b"\x89PNG\r\n\x1a\nfixture-image";
    let image = Image::new("pasted.png", bytes).unwrap();
    let request = OwnerRequest::with_images(
        &provider,
        std::process::id(),
        Message { text: "exact draft\nsecond line".into(), images: vec![image] },
    )
    .unwrap();
    assert_eq!(
        request.deliver(Instant::now() + Duration::from_secs(2), &AtomicBool::new(false)).unwrap()["id"],
        "accepted"
    );
    let message = received.recv().unwrap();
    assert_eq!(message.text, "exact draft\nsecond line");
    assert_eq!(message.images[0].bytes().unwrap().0, bytes);
    drop(server);
    assert!(!dir.path().join(SOCKET).exists());
}

#[test]
fn missing_ack_is_unknown_and_never_retries_the_message() {
    let dir = TempDir::new("dot-api-ack");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let (seen, received) = mpsc::channel();
    let _server = Server::start(dir.path(), Arc::new(AtomicBool::new(false)), move |_call| {
        seen.send(()).unwrap();
        true
    })
    .unwrap();
    let request = OwnerRequest::prepare(&provider(dir.path()), std::process::id(), "one message").unwrap();
    let error = request.deliver(Instant::now() + Duration::from_secs(2), &AtomicBool::new(false)).unwrap_err();
    assert!(error.contains("unknown"), "{error}");
    received.recv().unwrap();
    assert!(received.try_recv().is_err());
}

#[test]
fn an_expired_queued_dot_request_never_reaches_the_controller_worker() {
    let dir = TempDir::new("dot-api-expired");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let (seen, received) = mpsc::channel();
    let _server = Server::start(dir.path(), Arc::new(AtomicBool::new(false)), move |_call| {
        seen.send(()).unwrap();
        true
    })
    .unwrap();
    let mut stream = UnixStream::connect(dir.path().join(SOCKET)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    write_frame(
        &mut stream,
        &Request {
            controller: std::process::id(),
            expires: 0,
            message: Message { text: "do not send later".into(), images: Vec::new() },
        },
    )
    .unwrap();
    let mut header = [0; 4];
    stream.read_exact(&mut header).unwrap();
    let mut reply = vec![0; u32::from_be_bytes(header) as usize];
    stream.read_exact(&mut reply).unwrap();
    let reply: Value = serde_json::from_slice(&reply).unwrap();
    assert!(reply["error"].as_str().unwrap().contains("expired"));
    assert!(received.try_recv().is_err());
}

#[test]
fn dot_images_reject_unsafe_names_non_images_symlinks_and_oversized_sources() {
    let bytes = b"\x89PNG\r\n\x1a\nfixture";
    for name in ["../outside.png", "header\r\n.png", "quoted\".png"] {
        assert!(Image::new(name, bytes).is_err());
    }
    assert!(Image::new("text.png", b"this is not an image").is_err());
    let dir = TempDir::new("dot-image-source");
    let image = dir.write("image.png", bytes);
    let linked = dir.path().join("linked.png");
    std::os::unix::fs::symlink(&image, &linked).unwrap();
    assert!(Image::from_file(&linked).is_err());
    let large = dir.path().join("large.png");
    fs::File::create(&large).unwrap().set_len(IMAGE_MAX as u64 + 1).unwrap();
    assert!(Image::from_file(&large).is_err());
}

#[test]
fn reconnect_can_interrupt_a_waiting_api_call_and_quit_is_acknowledged_locally() {
    let dir = TempDir::new("dot-api-controls");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let controller_stop = Arc::new(AtomicBool::new(false));
    let waiting = Arc::new(std::sync::Mutex::new(None::<mpsc::Sender<Result<Value, String>>>));
    let pending = Arc::clone(&waiting);
    let (seen, started) = mpsc::channel();
    let server = Server::start(dir.path(), Arc::clone(&controller_stop), move |call| {
        match call.message.control() {
            Some(Control::Reconnect) => {
                pending
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(Err("Delivery interrupted; check the conversation".into()))
                    .unwrap();
                call.reply.send(Ok(json!({"control":Control::Reconnect}))).unwrap();
            }
            Some(Control::Quit) => {
                call.reply.send(Ok(json!({"control":Control::Quit}))).unwrap();
            }
            _ => {
                *pending.lock().unwrap() = Some(call.reply);
                seen.send(()).unwrap();
            }
        }
        true
    })
    .unwrap();
    let provider = provider(dir.path());
    let request = OwnerRequest::prepare(&provider, std::process::id(), "waiting for a receipt").unwrap();
    let first =
        std::thread::spawn(move || request.deliver(Instant::now() + Duration::from_secs(5), &AtomicBool::new(false)));
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    for command in ["/reconnect", "/quit"] {
        let request = OwnerRequest::prepare(&provider, std::process::id(), command).unwrap();
        assert!(request
            .deliver(Instant::now() + Duration::from_secs(2), &AtomicBool::new(false))
            .unwrap()
            .get("id")
            .is_none());
    }
    assert!(first.join().unwrap().unwrap_err().contains("interrupted"));
    drop(server);
    assert!(controller_stop.load(Ordering::SeqCst));
}
