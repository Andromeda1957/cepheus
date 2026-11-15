use super::*;
use crate::testsupport::TempDir;
fn socket() -> (Endpoint, UnixStream) {
    let (writer, reader) = UnixStream::pair().unwrap();
    let endpoint = Endpoint::from_stream(&writer).unwrap();
    reader.set_nonblocking(true).unwrap();
    (endpoint, reader)
}
#[test]
fn native_writer_accepts_only_current_generation_input_and_deadline_once() {
    let input = Mutex::new(7);
    let authority = Mutex::new(19);
    let (endpoint, mut reader) = socket();
    let deadline = Instant::now() + Duration::from_secs(1);
    assert!(authorized_write(&endpoint, "one-request", deadline, &input, 7, &authority, 19));
    assert!(matches!(read_msg::<_, ServerMsg>(&mut reader).unwrap(),
        Some(ServerMsg::PermissionDecision { request, allow: true }) if request == "one-request"));
    *authority.lock().unwrap() = 20;
    assert!(!authorized_write(&endpoint, "revoked-after-audit", deadline, &input, 7, &authority, 19));
    *authority.lock().unwrap() = 19;
    *input.lock().unwrap() = 8;
    assert!(!authorized_write(&endpoint, "owner-input", deadline, &input, 7, &authority, 19));
    assert!(!authorized_write(
        &endpoint,
        "expired",
        Instant::now() - Duration::from_millis(1),
        &input,
        8,
        &authority,
        19
    ));
    endpoint.canceled.store(true, Ordering::SeqCst);
    assert!(!authorized_write(&endpoint, "disconnected", deadline, &input, 8, &authority, 19));
    assert!(read_msg::<_, ServerMsg>(&mut reader).is_err(), "no late or revoked response bytes");
}
#[test]
fn unread_native_socket_never_holds_authority_waiting_for_transport() {
    let (endpoint, mut reader) = socket();
    let bytes = vec![b'x'; 8192];
    loop {
        let n = unsafe {
            libc::send(
                endpoint.socket.as_raw_fd(),
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if n < 0 {
            assert_eq!(std::io::Error::last_os_error().kind(), std::io::ErrorKind::WouldBlock);
            break;
        }
    }
    let input = Mutex::new(0);
    let authority = Mutex::new(1);
    let before = Instant::now();
    assert!(!authorized_write(&endpoint, "no-reader", before + Duration::from_secs(1), &input, 0, &authority, 1));
    assert!(before.elapsed() < Duration::from_secs(1), "one nonblocking attempt");
    *authority.try_lock().unwrap() = 2;
    *input.try_lock().unwrap() = 1;
    let mut all = Vec::new();
    let mut buffer = [0u8; 8192];
    while let Ok(n) = reader.read(&mut buffer) {
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buffer[..n]);
    }
    assert!(all.iter().all(|b| *b == b'x'), "neither partial approval nor retry tail was written");
    assert!(!authorized_write(
        &endpoint,
        "late-reader",
        Instant::now() + Duration::from_secs(1),
        &input,
        0,
        &authority,
        1
    ));
}
#[test]
fn embedded_payload_is_versioned_private_and_changed_or_symlinked_bytes_refuse() {
    let d = TempDir::new("attention-hook-payload");
    let state = d.path().join("state");
    crate::job_sessions::store::directory(&state).unwrap();
    let path = materialize(&state).unwrap();
    assert_eq!(fs::read(&path).unwrap(), fs::read(interpreter().unwrap()).unwrap());
    assert_eq!(materialize(&state).unwrap(), path);
    fs::write(&path, b"changed").unwrap();
    assert!(materialize(&state).is_err(), "never replace a running helper's changed ABI");
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(d.path().join("outside"), &path).unwrap();
    assert!(materialize(&state).is_err());
    assert!(!d.path().join("outside").exists());
}
#[test]
fn native_rules_share_ids_and_limits_with_terminal_rules_and_old_hooks_get_only_welcome() {
    let d = TempDir::new("attention-native-rules");
    let (tx, _) = mpsc::channel();
    let mut svc = Service::new(super::super::tests::options(&d, "true"), tx);
    let read = serde_json::json!({"id":"native","tool":"Read","pattern":{"kind":"native_exact","input":{"file_path":d.path().join("file")}}});
    let shell = serde_json::json!({"id":"shell","tool":"Bash","pattern":{"kind":"exact","command":"cat safe.txt"}});
    assert!(svc.attention_validate_rules(&[read.clone(), shell]).is_ok());
    assert!(svc.attention_validate_rules(&[read.clone(), read]).is_err());
    let (out, rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 1, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(1, ClientMsg::Hello { version: 15, role: ClientRole::Hook });
    let outputs = rx.try_iter().collect::<Vec<_>>();
    assert_eq!(outputs.len(), 1, "Hook has no potentially oversized State or notice before a decision");
    let Out::Msg(bytes) = &outputs[0] else { panic!("welcome") };
    assert!(matches!(
        read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap(),
        Some(ServerMsg::Welcome { version, .. }) if version == PROTOCOL_VERSION
    ));
    svc.handle_client(
        1,
        ClientMsg::PermissionRequest {
            session: 1,
            spawned_ms: 1,
            token: "absent".into(),
            payload: serde_json::json!({"prompt_id":"private"}),
        },
    );
    assert!(rx.try_recv().is_err(), "never send new reply variant to a protocol15 hook");
}
#[test]
fn native_worktree_proof_declines_unknown_and_custom_authentication_store_overlap() {
    let d = TempDir::new("attention-native-root");
    let root = d.path().join("work");
    fs::create_dir(&root).unwrap();
    assert!(!approved_worktree(&root, &root, &[]));
    assert!(std::process::Command::new("git").args(["init", "--quiet"]).arg(&root).status().unwrap().success());
    let root = root.canonicalize().unwrap();
    assert!(approved_worktree(&root, &root, &[]));
    assert!(!approved_worktree(&root, &root, &[root.clone()]));
    let authentication = root.join("custom-provider-home");
    fs::create_dir(&authentication).unwrap();
    assert!(!approved_worktree(&root, &root, &[authentication]));
    assert!(!approved_worktree(&root, &root, &[d.path().to_path_buf()]));
}

#[test]
fn configured_opencode_data_root_is_private_even_when_redirected_into_a_worktree() {
    let d = TempDir::new("attention-native-xdg");
    let root = d.path().join("work");
    fs::create_dir(&root).unwrap();
    assert!(std::process::Command::new("git").args(["init", "--quiet"]).arg(&root).status().unwrap().success());
    let root = root.canonicalize().unwrap();
    let auth = root.join("redirected-xdg-data/opencode");
    fs::create_dir_all(&auth).unwrap();
    fs::write(auth.join("auth.json"), "{}\n").unwrap();
    let mut opts = super::super::tests::options(&d, "true");
    opts.opencode_db = auth.join("opencode.db");
    let (tx, _) = mpsc::channel();
    let svc = Service::new(opts, tx);
    let session = super::super::tests::fake_session(1, Tool::Claude);
    let roots = private_roots(&svc, &session);
    assert!(roots.iter().any(|p| p == &auth));
    assert!(!approved_worktree(&root, &root, &roots));
}

#[test]
fn exited_fire_and_forget_hook_still_reaches_the_existing_turn_tracker() {
    let d = TempDir::new("attention-legacy-hook-exit");
    let path = d.path().join("hook.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let mut frames = encode(&ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Hook });
    frames.extend(encode(&ClientMsg::Hook {
        session: 1,
        tool: Tool::Claude,
        payload: serde_json::json!({"hook_event_name":"Stop","session_id":"conversation"}),
    }));
    let hex: String = frames.iter().map(|b| format!("{b:02x}")).collect();
    let status = std::process::Command::new("/usr/bin/python3")
        .args(["-I","-S","-c", "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(bytes.fromhex(sys.argv[2])); s.close()"])
        .arg(&path).arg(hex).status().unwrap();
    assert!(status.success());
    let (stream, _) = listener.accept().unwrap();
    assert!(super::super::peer_is_owner(&stream));
    assert!(Endpoint::from_stream(&stream).is_none(), "the actual sending process was reaped before accept");
    let (tx, rx) = mpsc::channel();
    let mut svc = Service::new(super::super::tests::options(&d, "true"), tx.clone());
    let mut session = super::super::tests::fake_session(1, Tool::Claude);
    session.conversation = Some("conversation".into());
    svc.sessions.insert(1, session);
    super::super::accept_client(stream, 1, tx);
    loop {
        let event = rx.recv_timeout(Duration::from_secs(3)).expect("actual hook frames delivered after sender exit");
        let gone = matches!(&event, Ev::ClientGone(1));
        svc.handle(event);
        if gone {
            break;
        }
    }
    assert!(svc.sessions[&1].tracker.pending_finish, "legacy Stop was applied without native authorization");
}
