use super::*;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::{net::UnixListener, process::CommandExt};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use crate::remote::chat::{Cache, Dirs};

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn send(ws: &mut Socket, value: Value) {
    ws.send(Message::Text(value.to_string().into())).unwrap();
}

fn request(ws: &mut Socket, method: &str, result: Value) {
    let value: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(value["method"], method);
    send(ws, json!({"id": value["id"], "result": result}));
}

fn event(method: &str, params: Value) -> Value {
    let mut params = params;
    params["threadId"] = json!("11111111-1111-4111-8111-111111111111");
    json!({"method": method, "params": params})
}

fn assistant(log: &crate::remote::chat::Log) -> Vec<String> {
    log.items
        .iter()
        .filter_map(|i| match i {
            Item::Assistant { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn phone_chat_streams_before_completion_and_reconciles_reconnect_and_rollout() {
    // This is a known buffering regression: exercise the real Unix WebSocket
    // observer and canonical phone chunks, with completion withheld.
    let dir = crate::testsupport::TempDir::new("streaming");
    let home = dir.path().join("codex");
    fs::create_dir_all(home.join("app-server-control")).unwrap();
    let path = home.join("app-server-control/app-server-control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let child = Child(
        Command::new("cat")
            .arg0("codex")
            .env("CODEX_HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let thread_id = "11111111-1111-4111-8111-111111111111";
    let rollout = home.join(format!("sessions/2026/11/05/rollout-2026-11-05T22-00-00-{thread_id}.jsonl"));
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let mut records = vec![json!({"type":"session_meta","payload":{"id":thread_id,"cli_version":"0.167.0"}})];
    for (kind, id, text) in [
        ("UserMessage", "old-user", "Earlier question"),
        ("AgentMessage", "old-reply", "Earlier saved reply"),
        ("UserMessage", "new-user", "Current question"),
    ] {
        records.push(json!({"type":"event_msg","payload":{"type":"item_completed","item":{
            "type":kind,"id":id,"content":[{"type":"Text","text":text}]}}}));
    }
    fs::write(&rollout, records.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
    let mut info = crate::tui::tests::session(7, Tool::Codex, crate::protocol::Status::Working, true);
    info.pid = Some(child.0.id());
    info.conversation = Some(thread_id.into());
    let dirs = Dirs {
        codex: home,
        claude: dir.path().join("claude"),
        opencode_db: dir.path().join("opencode"),
        antigravity: dir.path().join("agy"),
        start_dir: dir.path().into(),
        sqlite3: Vec::new(),
    };
    let (action_tx, action_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        for connection in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            request(&mut ws, "initialize", json!({}));
            let initialized: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(initialized["method"], "initialized");
            request(&mut ws, "thread/read", json!({"thread":{"id":thread_id,"status":{"type":"active"}}}));
            request(&mut ws, "thread/resume", json!({"thread":{"id":thread_id}}));
            if connection == 0 {
                ready_tx.send(connection).unwrap();
                action_rx.recv_timeout(Duration::from_secs(20)).unwrap();
                send(&mut ws, event("item/started", json!({"item":{"type":"agentMessage","id":"reply","text":""}})));
                send(&mut ws, event("item/agentMessage/delta", json!({"itemId":"reply","delta":"First "})));
                ws.send(Message::Ping(Vec::new().into())).unwrap();
                assert!(matches!(ws.read().unwrap(), Message::Pong(_)));
                ready_tx.send(2).unwrap();
                action_rx.recv_timeout(Duration::from_secs(20)).unwrap();
                send(&mut ws, event("item/agentMessage/delta", json!({"itemId":"reply","delta":"half"})));
                action_rx.recv_timeout(Duration::from_secs(20)).unwrap();
                let _ = ws.close(None);
            } else {
                // The reconnect missed words. A suffix must not be appended
                // to the old prefix; completion supplies the authoritative text.
                send(&mut ws, event("item/agentMessage/delta", json!({"itemId":"reply","delta":" lost suffix"})));
                ws.send(Message::Ping(Vec::new().into())).unwrap();
                assert!(matches!(ws.read().unwrap(), Message::Pong(_)));
                ready_tx.send(connection).unwrap();
                action_rx.recv_timeout(Duration::from_secs(20)).unwrap();
                send(
                    &mut ws,
                    event(
                        "item/completed",
                        json!({"item":{"type":"agentMessage","id":"reply","text":"First half, recovered whole."}}),
                    ),
                );
                action_rx.recv_timeout(Duration::from_secs(20)).unwrap();
            }
        }
    });
    let (mut cache, mut phone) = (Cache::default(), Cache::default());
    // The generation the first read took; live deltas never replace it.
    let canonical_generation = std::cell::OnceCell::new();
    let refresh = |cache: &mut Cache, phone: &mut Cache| {
        let (from, generation) = phone.peek(info.id).map_or((0, 0), |l| (l.total(), l.generation));
        let chunk = cache.chunk(&info, &dirs, from, generation);
        let chunk = serde_json::from_value(serde_json::to_value(chunk).unwrap()).unwrap();
        phone.apply(info.id, chunk);
        cache.peek(info.id).and_then(|log| log.stream())
    };
    let wait_text = |cache: &mut Cache, phone: &mut Cache, expected: &str| {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if refresh(cache, phone).as_deref() == Some(expected) {
                let canonical = phone.peek(info.id).unwrap();
                assert_eq!(canonical.generation, *canonical_generation.get_or_init(|| canonical.generation));
                assert_eq!(canonical.total(), 3);
                assert_eq!(assistant(canonical), ["Earlier saved reply"]);
                return;
            }
            assert!(
                Instant::now() < end,
                "phone never displayed {expected:?}; got {:?}",
                phone.peek(info.id).map(assistant)
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while Runtime::of(&info).is_none() {
        assert!(Instant::now() < deadline, "fixture process did not finish exec");
        thread::sleep(Duration::from_millis(20));
    }
    cache.watch_sessions(std::slice::from_ref(&info));
    ready_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    action_tx.send(()).unwrap();
    ready_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    wait_text(&mut cache, &mut phone, "First ");
    action_tx.send(()).unwrap();
    wait_text(&mut cache, &mut phone, "First half");
    assert_eq!(
        Some(&cache.peek(info.id).unwrap().generation),
        canonical_generation.get(),
        "live deltas must preserve the canonical generation and loaded history"
    );
    // Reopening the phone does not recreate the daemon observer or lose text.
    phone = Cache::default();
    assert_eq!(refresh(&mut cache, &mut phone).as_deref(), Some("First half"));
    assert_eq!(assistant(phone.peek(info.id).unwrap()), ["Earlier saved reply"]);
    action_tx.send(()).unwrap();
    ready_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    let mut file = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
    writeln!(file, "{}", json!({"type":"turn_context","payload":{"model":"reconnected"}})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while cache.peek(info.id).unwrap().seen.model.as_deref() != Some("reconnected") {
        assert!(Instant::now() < deadline, "reconnect did not refresh");
        refresh(&mut cache, &mut phone);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(assistant(phone.peek(info.id).unwrap()), ["Earlier saved reply"]);
    assert_eq!(cache.peek(info.id).unwrap().stream().as_deref(), Some("First half"));
    action_tx.send(()).unwrap();
    wait_text(&mut cache, &mut phone, "First half, recovered whole.");
    let mut file = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
    writeln!(file,"{}",json!({"timestamp":"2026-11-05T22:00:00Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"reply","content":[{"type":"Text","text":"First half, recovered whole."}]}}})).unwrap();
    // Force the next normal log refresh without a fixed wall-clock delay.
    let deadline = Instant::now() + Duration::from_secs(20);
    while cache.peek(info.id).unwrap().total() != 4 {
        assert!(Instant::now() < deadline, "completed rollout did not replace live overlay");
        refresh(&mut cache, &mut phone);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(Some(&cache.peek(info.id).unwrap().generation), canonical_generation.get());
    assert_eq!(cache.peek(info.id).unwrap().stream(), None);
    assert_eq!(assistant(phone.peek(info.id).unwrap()), ["Earlier saved reply", "First half, recovered whole."]);
    action_tx.send(()).unwrap();
    drop(cache);
    server.join().unwrap();
}

#[test]
fn observer_refuses_embedded_and_remote_runtimes() {
    let env = b"HOME=/fixture\0";
    assert!(Runtime::parse(1, "thread", b"codex\0--no-daemon\0", env).is_none());
    assert!(Runtime::parse(1, "thread", b"codex\0-c\0model=x\0", env).is_none());
    assert!(Runtime::parse(1, "thread", b"codex\0--remote\0wss://elsewhere\0", env).is_none());
    assert!(Runtime::parse(1, "thread", b"wrapper\0codex\0", env).is_none());
    assert_eq!(
        Runtime::parse(1, "thread", b"codex\0--remote\0unix:///fixture/socket\0", env).unwrap().socket,
        Path::new("/fixture/socket")
    );
    let dir = crate::testsupport::TempDir::new("observer-directory");
    let args = format!("codex\0--cd\0{}\0", dir.path().display());
    assert_eq!(
        Runtime::parse(1, "thread", args.as_bytes(), env).unwrap().cwd,
        Some(dir.path().canonicalize().unwrap())
    );
    for suffix in ["--cd\0relative\0", "--cd\0", "--cd\0/this-path-does-not-exist\0"] {
        assert!(Runtime::parse(1, "thread", format!("codex\0{suffix}").as_bytes(), env).is_none());
    }
}

#[test]
fn observer_follows_a_turn_resumed_with_its_continuation() {
    // Cold-start recovery resumes an interrupted turn as
    // `codex resume ID -- MESSAGE`: the message is the last argument.
    let env = b"HOME=/fixture\0";
    let resumed = Runtime::parse(1, "thread", b"codex\0resume\0thread\0--\0--carry on\0", env).unwrap();
    assert_eq!(resumed.socket, Path::new("/fixture/.codex/app-server-control/app-server-control.sock"));
    assert!(Runtime::parse(1, "thread", b"codex\0resume\0thread\0--\0", env).is_none());
    assert!(Runtime::parse(1, "thread", b"codex\0resume\0thread\0--\0one\0two\0", env).is_none());
    assert!(Runtime::parse(1, "thread", b"codex\0resume\0other\0--\0carry on\0", env).is_none());
}

#[test]
fn observer_does_not_load_a_thread_in_the_wrong_daemon() {
    let dir = crate::testsupport::TempDir::new("stream-unloaded");
    let path = dir.path().join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        request(&mut ws, "initialize", json!({}));
        let initialized: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(initialized["method"], "initialized");
        request(&mut ws, "thread/read", json!({"thread":{"id":"thread","status":{"type":"notLoaded"}}}));
        assert!(ws.read().is_err(), "an unloaded thread must not be resumed by the observer");
    });
    let runtime = Runtime { pid: std::process::id(), socket: path, thread: "thread".into(), cwd: None };
    assert!(observe(&runtime, &Mutex::new(State::default()), &AtomicBool::new(false)).is_err());
    server.join().unwrap();
}

#[test]
fn full_unix_backlog_connect_respects_deadline_and_stop() {
    let dir = crate::testsupport::TempDir::new("stream-backlog");
    for cancelled in [false, true] {
        let path = dir.path().join(if cancelled { "cancel" } else { "deadline" });
        let listener = UnixListener::bind(&path).unwrap();
        // A paused daemon with a full accept queue used to block connect before
        // timeouts were installed, and observer Drop then joined indefinitely.
        // SAFETY: listen changes the backlog of our valid listening descriptor.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let queued = UnixStream::connect(&path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(if cancelled { 5_000 } else { 250 });
            tx.send(connect_local(&path, deadline, &done)).unwrap();
        });
        // Ensure this attempt encountered the saturated queue before cancelling.
        assert!(matches!(rx.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout)));
        if cancelled {
            stop.store(true, Ordering::Relaxed);
        }
        let bounded = rx.recv_timeout(Duration::from_secs(1));
        let in_time = bounded.is_ok();
        // Release the queue even with the broken blocking implementation, so a
        // regression fails with evidence instead of leaving a blocked child.
        let result = match bounded {
            Ok(result) => result,
            Err(_) => {
                let accepted = listener.accept().unwrap();
                let result = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                drop(accepted);
                result
            }
        };
        worker.join().unwrap();
        drop(queued);
        assert!(in_time, "full backlog ignored deadline/cancellation");
        assert_eq!(
            result.unwrap_err().kind(),
            if cancelled { io::ErrorKind::Interrupted } else { io::ErrorKind::TimedOut }
        );
    }
}
