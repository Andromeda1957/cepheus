use super::*;
use crate::testsupport::TempDir;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixStream;

fn snapshot(messages: Value) -> Value {
    json!({"dot_id":"fixture", "room_id":"room", "messages":messages, "working":false, "activity_known":true})
}

fn fixture(state: Arc<State>, stream: UnixStream) -> Bridge {
    Bridge::over(state, stream, None)
}

#[test]
fn image_rpc_backpressure_keeps_deadlines_shutdown_and_reconnect_effective() {
    use std::os::fd::FromRawFd;
    for cancellation in [None, Some(false), Some(true)] {
        let mut descriptors = [0; 2];
        // SAFETY: pipe2 initializes two descriptors owned only by this fixture.
        assert_eq!(unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both fresh pipe descriptors have exactly one File owner.
        let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
        let mut writer = nonblocking_input(unsafe { std::fs::File::from_raw_fd(descriptors[1]) }).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let reconnecting = Arc::clone(&interrupt);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(if cancellation.is_some() { 5_000 } else { 250 });
            let result = write_request(&mut writer, &vec![b'x'; 2 << 20], deadline, &stopping, &reconnecting);
            tx.send(result).unwrap();
        });
        let early = rx.recv_timeout(Duration::from_millis(50));
        let waited = matches!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
        if let Some(reconnect) = cancellation {
            if reconnect { &interrupt } else { &stop }.store(true, Ordering::Relaxed);
        }
        let bounded = match early {
            Ok(result) => Ok(result),
            Err(_) => rx.recv_timeout(Duration::from_secs(1)),
        };
        let in_time = bounded.is_ok();
        // Release the stalled pipe even if a regression ignored cancellation.
        drop(reader);
        let result = bounded.unwrap_or_else(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap());
        worker.join().unwrap();
        assert!(waited, "backpressure must wait rather than reject a partial image frame");
        assert!(in_time, "stalled image write ignored deadline/cancellation");
        assert_eq!(
            result.unwrap_err().kind(),
            if cancellation.is_some() { io::ErrorKind::Interrupted } else { io::ErrorKind::TimedOut }
        );
    }

    // A temporary full pipe must also deliver the entire image frame once
    // the host resumes reading, rather than truncate it at a partial write.
    let mut descriptors = [0; 2];
    // SAFETY: this fixture exclusively owns both new pipe descriptors.
    assert_eq!(unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
    let mut writer = nonblocking_input(unsafe { std::fs::File::from_raw_fd(descriptors[1]) }).unwrap();
    let payload: Vec<_> = (0..(2 << 20)).map(|n| b'a' + (n % 26) as u8).chain(std::iter::once(b'\n')).collect();
    let expected = payload.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = write_request(
            &mut writer,
            &payload,
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        tx.send(result).unwrap();
    });
    assert!(matches!(rx.recv_timeout(Duration::from_millis(50)), Err(std::sync::mpsc::RecvTimeoutError::Timeout)));
    let mut received = Vec::new();
    std::io::Read::take(reader, expected.len() as u64 + 1).read_to_end(&mut received).unwrap();
    rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
    worker.join().unwrap();
    assert_eq!(received, expected);
}

#[test]
fn journal_uuid_precedes_post_and_only_matching_new_echo_clears_uncertainty() {
    let temp = TempDir::new("dot-journal");
    let state = Arc::new(State::new(&temp.path().join("state")).unwrap());
    let (client, server) = UnixStream::pair().unwrap();
    let observed = Arc::clone(&state);
    let host = std::thread::spawn(move || {
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let mut writer = server;
        for step in 0..3 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let result = if step == 1 {
                assert_eq!(request["action"], "send");
                let pending = observed.read("pending").unwrap().unwrap();
                assert_eq!(request["request_id"], pending["request_id"]);
                assert_eq!(pending["text"], "new\nmessage");
                json!({"id":"accepted"})
            } else {
                snapshot(json!([{"id":"old","role":"user","text":"new message"}]))
            };
            writeln!(writer, "{}", json!({"id":request["id"], "ok":true, "result":result})).unwrap();
        }
    });
    let mut bridge = fixture(Arc::clone(&state), client);
    bridge.send("new\nmessage").unwrap();
    host.join().unwrap();
    let pending = state.read("pending").unwrap().unwrap();
    assert_eq!(pending["ack"], "accepted");
    assert!(matches!(bridge.send("another message"), Err(Error::Attention(_))));
    bridge.publish(snapshot(json!([{"id":"wrong-ack","role":"user","text":"new message"}]))).unwrap();
    assert_eq!(state.read("pending").unwrap().unwrap(), pending);
    bridge
        .publish(snapshot(json!([{"id":"accepted","role":"user","text":"new message","at":"2026-11-07T06:00:00Z"}])))
        .unwrap();
    assert_eq!(state.read("pending").unwrap(), Some(Value::Null));
    let feed = state.read("transcript").unwrap().unwrap();
    assert_eq!(feed.as_array().unwrap().iter().find(|item| item["id"] == "accepted").unwrap()["at"], 1794031200000u64);
}

#[test]
fn image_echo_requires_the_same_uploaded_file_identity() {
    let temp = TempDir::new("dot-image-journal");
    let state = Arc::new(State::new(&temp.path().join("state")).unwrap());
    let (client, _server) = UnixStream::pair().unwrap();
    let mut bridge = fixture(Arc::clone(&state), client);
    let pending = json!({"room_id":"room","text":"look","before":[],"files":["original-file"]});
    state.write("pending", &pending).unwrap();
    bridge
        .publish(snapshot(json!([{"id":"new","role":"user","text":"look","attachments":[{"id":"different-file"}]}])))
        .unwrap();
    assert_eq!(state.read("pending").unwrap().unwrap(), pending);
    bridge
        .publish(snapshot(json!([{"id":"new","role":"user","text":"look","attachments":[{"id":"original-file"}]}])))
        .unwrap();
    assert_eq!(state.read("pending").unwrap(), Some(Value::Null));
}

#[test]
fn lost_send_ack_keeps_the_same_journal_without_resending() {
    let temp = TempDir::new("dot-lost-ack");
    let state = Arc::new(State::new(&temp.path().join("state")).unwrap());
    let (client, server) = UnixStream::pair().unwrap();
    let host = std::thread::spawn(move || {
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let mut writer = server;
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        writeln!(writer, "{}", json!({"id":request["id"],"ok":true,"result":snapshot(json!([]))})).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["action"], "send");
        // Accepted by the synthetic host, then the connection disappears.
    });
    let mut bridge = fixture(Arc::clone(&state), client);
    assert!(bridge.send("hello").is_err());
    host.join().unwrap();
    let before = std::fs::read(state.path("pending")).unwrap();
    assert!(bridge.send("hello").is_err());
    assert_eq!(std::fs::read(state.path("pending")).unwrap(), before);

    // Reconnect must interrupt a host that accepted the request but never
    // replies, without waiting for the normal network deadline or resending.
    let (client, server) = UnixStream::pair().unwrap();
    let mut waiting = fixture(Arc::clone(&state), client);
    let interrupt = waiting.interrupt.clone();
    let (finished, completion) = std::sync::mpsc::channel();
    let request = std::thread::spawn(move || {
        finished.send(waiting.rpc("snapshot", json!({})).is_err()).unwrap();
    });
    let mut reader = BufReader::new(server);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    interrupt.raise();
    assert!(completion.recv_timeout(Duration::from_secs(2)).unwrap());
    request.join().unwrap();
    assert_eq!(std::fs::read(state.path("pending")).unwrap(), before);
}

/// A host answering each request with the next of `answers`.
fn answering(server: UnixStream, answers: Vec<Value>) -> std::thread::JoinHandle<Vec<Value>> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let mut writer = server;
        let mut requests = Vec::new();
        for mut answer in answers {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            answer["id"] = request["id"].clone();
            requests.push(request);
            writeln!(writer, "{answer}").unwrap();
        }
        requests
    })
}

fn message(text: &str) -> super::super::api::Message {
    super::super::api::Message { text: text.into(), images: Vec::new() }
}

// Before the journal nothing reached the host, so the draft is simply not
// sent; after it, the host may have posted the message, which is never
// shown as sent or failed until its echo or the owner's /resolve.
#[test]
fn a_send_failure_says_whether_the_message_may_have_been_posted() {
    let temp = TempDir::new("dot-send-outcome");
    let state = Arc::new(State::new(&temp.path().join("state")).unwrap());
    let timed_out = json!({"ok":false, "error":"Native request timed out.", "retry":true, "retry_after":5});
    let (client, server) = UnixStream::pair().unwrap();
    let host = answering(server, vec![timed_out.clone()]);
    let mut bridge = fixture(Arc::clone(&state), client);
    let deadline = Instant::now() + Duration::from_secs(120);
    let error = bridge.send_message(&message("hello"), deadline).unwrap_err();
    assert_eq!(error.detail(), "Not sent: Native request timed out.");
    assert!(!bridge.ready, "the chat reconnects");
    assert_eq!(state.read("pending").unwrap(), None);
    host.join().unwrap();

    let (client, server) = UnixStream::pair().unwrap();
    let host = answering(server, vec![json!({"ok":true, "result":snapshot(json!([]))}), timed_out]);
    let mut bridge = fixture(Arc::clone(&state), client);
    let error = bridge.send_message(&message("hello"), deadline).unwrap_err();
    assert_eq!(error.detail(), "Delivery unconfirmed: Native request timed out. Check ChatGPT; nothing is resent.");
    let requests = host.join().unwrap();
    let pending = state.read("pending").unwrap().unwrap();
    assert_eq!(pending["request_id"], requests[1]["request_id"]);
    assert_eq!(pending["text"], "hello");
}

// An acknowledged message is submitted even if the refresh after it fails:
// reporting it as failed would invite the owner to send it twice.
#[test]
fn an_acknowledged_send_succeeds_even_when_the_next_refresh_fails() {
    let temp = TempDir::new("dot-ack-refresh");
    let state = Arc::new(State::new(&temp.path().join("state")).unwrap());
    let (client, server) = UnixStream::pair().unwrap();
    let host = answering(
        server,
        vec![
            json!({"ok":true, "result":snapshot(json!([]))}),
            json!({"ok":true, "result":{"id":"accepted"}}),
            json!({"ok":false, "error":"Native request timed out.", "retry":true, "retry_after":5}),
        ],
    );
    let mut bridge = fixture(Arc::clone(&state), client);
    let sent = bridge.send_message(&message("hello"), Instant::now() + Duration::from_secs(120)).unwrap();
    assert_eq!(sent["id"], "accepted");
    assert!(!bridge.ready, "the failed refresh still reconnects");
    assert_eq!(state.read("pending").unwrap().unwrap()["ack"], "accepted");
    assert_eq!(host.join().unwrap().len(), 3);
}

#[test]
fn live_announcements_reach_the_chat_and_never_answer_a_request() {
    let temp = TempDir::new("dot-announcements");
    let state = Arc::new(State::new(&temp.path().join("state")).unwrap());
    let (client, server) = UnixStream::pair().unwrap();
    let (heard, announcements) = std::sync::mpsc::channel();
    let events: Events = Arc::new(move || {
        let _ = heard.send(());
    });
    let mut bridge = Bridge::over(Arc::clone(&state), client, Some(events));
    let host = std::thread::spawn(move || {
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let mut writer = server;
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        writeln!(writer, "{{\"event\":\"live\"}}").unwrap();
        writeln!(writer, "{}", json!({"id":request["id"],"ok":true,"result":snapshot(json!([]))})).unwrap();
        writeln!(writer, "{{\"event\":\"live\"}}").unwrap();
    });
    assert_eq!(bridge.snapshot().unwrap()["room_id"], "room");
    assert!(bridge.ready);
    for _ in 0..2 {
        announcements.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    host.join().unwrap();
}

#[test]
fn sliding_windows_keep_history_and_update_streaming_messages_by_identity() {
    let temp = TempDir::new("dot-history");
    let state = Arc::new(State::new(&temp.path().join("state")).unwrap());
    let (client, _server) = UnixStream::pair().unwrap();
    let mut bridge = fixture(Arc::clone(&state), client);
    state.write("transcript", &json!([{"k":"user","text":"earlier"}])).unwrap();
    bridge.publish(snapshot(json!([]))).unwrap();
    assert_eq!(
        state.read("transcript").unwrap().unwrap()[0]["text"],
        "earlier",
        "an empty initial window must preserve the legacy cache"
    );
    for messages in [
        json!([{"id":"first","role":"user","text":"earlier"},{"id":"reply","role":"assistant","text":"partial"}]),
        json!([{"id":"reply","role":"assistant","text":"complete"},{"id":"next","role":"user","text":"later"}]),
    ] {
        bridge.publish(snapshot(messages)).unwrap();
    }
    let feed = state.read("transcript").unwrap().unwrap();
    let items = feed.as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["text"], "earlier");
    assert_eq!(items[1]["text"], "complete");
    let mut deleted = snapshot(json!([]));
    deleted["deleted"] = json!(["first"]);
    bridge.publish(deleted).unwrap();
    assert_eq!(state.read("transcript").unwrap().unwrap().as_array().unwrap().len(), 2);
}

#[test]
fn wrong_room_and_duplicate_message_ids_cannot_acknowledge_delivery() {
    let temp = TempDir::new("dot-room");
    let state = Arc::new(State::new(&temp.path().join("state")).unwrap());
    let (client, _server) = UnixStream::pair().unwrap();
    let mut bridge = fixture(Arc::clone(&state), client);
    state.write("pending", &json!({"room_id":"room","text":"hello","before":[]})).unwrap();
    let mut wrong = snapshot(json!([{"id":"new","role":"user","text":"hello"}]));
    wrong["room_id"] = "different".into();
    assert!(bridge.publish(wrong).is_err());
    assert!(state.read("pending").unwrap().unwrap().is_object());
    assert!(bridge
        .publish(snapshot(
            json!([{"id":"same","role":"user","text":"hello"},{"id":"same","role":"assistant","text":"hi"}])
        ))
        .is_err());
    assert!(state.read("pending").unwrap().unwrap().is_object());

    // A legacy native helper can answer hello but must never receive room
    // access, or change the controller's private state, without this protocol.
    let before = std::fs::read(state.path("pending")).unwrap();
    let (client, server) = UnixStream::pair().unwrap();
    server.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let host = std::thread::spawn(move || {
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let mut writer = server;
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["action"], "hello");
        writeln!(writer, "{}", json!({"id":request["id"],"ok":true,"result":{"version":"0.2.0"}})).unwrap();
        line.clear();
        let next = reader.read_line(&mut line);
        assert!(
            matches!(next, Ok(0))
                || matches!(next, Err(ref error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
            "an old host must not receive open: {next:?}"
        );
    });
    let mut old = fixture(Arc::clone(&state), client);
    assert!(matches!(old.handshake("https://chatgpt.com/dots/fixture"), Err(Error::Attention(_))));
    host.join().unwrap();
    assert_eq!(std::fs::read(state.path("pending")).unwrap(), before);
    assert_eq!(state.read("selection").unwrap(), None);
    assert!(!old.ready);
}
