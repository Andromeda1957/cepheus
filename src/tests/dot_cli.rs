use super::*;
use crate::dot::api::{Call, Control, Message};
use crate::testsupport::TempDir;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;

fn snapshot(messages: Value) -> Value {
    json!({"dot_id":"fixture", "room_id":"room", "messages":messages, "working":false, "activity_known":true})
}

/// Answer each request from the chat with `answers`, in order.
fn host(server: UnixStream, answers: Vec<Value>) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let mut writer = server;
        let mut actions = Vec::new();
        for answer in answers {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            actions.push(request["action"].as_str().unwrap().to_string());
            let mut answer = answer;
            answer["id"] = request["id"].clone();
            writeln!(writer, "{answer}").unwrap();
        }
        actions
    })
}

#[test]
fn direct_api_controls_stay_local_and_preserve_the_selected_room() {
    let dir = TempDir::new("dot-local-controls");
    let state = Arc::new(State::new(dir.path()).unwrap());
    let selection = json!({"dot_id":"fixture","room_id":"room"});
    state.write("selection", &selection).unwrap();
    state.write("pending", &json!({"text":"uncertain"})).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let mut bridge = Bridge::new(Arc::clone(&state), "local".into(), Arc::clone(&stop)).unwrap();
    bridge.ready = true;
    let (commands, input) = mpsc::channel();
    let (reply, replies) = mpsc::channel();
    for command in ["/resolve", "/reconnect", "/quit"] {
        commands
            .send(Work::Api(Call {
                message: Message { text: command.into(), images: Vec::new() },
                deadline: Instant::now() + Duration::from_secs(120),
                reply: reply.clone(),
            }))
            .unwrap();
    }
    // A closed, preloaded queue makes any attempt to open a cloud transport
    // unnecessary, including when this fixture runs under CPU contention.
    drop(commands);
    drop(reply);
    let quit = std::cell::Cell::new(false);
    // A newer build takes over only once every queued request is answered.
    let queued = Arc::new(AtomicUsize::new(3));
    worker(bridge, input, &|update| quit.set(quit.get() || update.quit), stop, Arc::clone(&queued));
    assert_eq!(queued.load(Ordering::SeqCst), 0, "every request was answered");
    let receipts: Vec<_> = replies.try_iter().map(|receipt| receipt.unwrap()).collect();
    assert_eq!(
        receipts,
        vec![
            json!({"control":Control::Resolve}),
            json!({"control":Control::Reconnect}),
            json!({"control":Control::Quit})
        ]
    );
    assert!(quit.get(), "/quit ends the chat");
    assert_eq!(state.read("pending").unwrap(), Some(Value::Null));
    assert_eq!(state.read("selection").unwrap(), Some(selection));
}

// Any host failure during a send used to leave the chat disconnected for
// good, refusing every later text and image.
#[test]
fn a_failed_send_says_it_was_not_sent_and_reconnects() {
    let dir = TempDir::new("dot-send-failure");
    let state = Arc::new(State::new(dir.path()).unwrap());
    let (client, server) = UnixStream::pair().unwrap();
    let host = host(
        server,
        vec![
            json!({"ok":true, "result":snapshot(json!([]))}),
            json!({"ok":false, "error":"Native request timed out.", "retry":true, "retry_after":5}),
        ],
    );
    let bridge = Bridge::over(Arc::clone(&state), client, None);
    let (commands, input) = mpsc::channel();
    let (reply, replies) = mpsc::channel();
    let failures = std::sync::Mutex::new(Vec::new());
    let output = |update: Update| failures.lock().unwrap().push(update.failure);
    let mut worker = Worker::new(bridge, &output);
    // The first refresh is answered, then the send's own refresh fails.
    worker.due();
    commands
        .send(Work::Api(Call {
            message: Message { text: "hello".into(), images: Vec::new() },
            deadline: Instant::now() + Duration::from_secs(120),
            reply,
        }))
        .unwrap();
    commands.send(Work::Stop).unwrap();
    worker.run(input, Arc::new(AtomicBool::new(false)));
    let error = replies.recv().unwrap().unwrap_err();
    assert_eq!(error, "Not sent: Native request timed out.");
    assert_eq!(host.join().unwrap(), ["snapshot", "snapshot"]);
    assert_eq!(state.read("pending").unwrap(), None, "nothing was journaled");
    let failures = failures.into_inner().unwrap();
    assert_eq!(failures.last().unwrap().as_deref(), Some("Cannot connect to Dot; retrying. Nothing is resent."));
    // A timeout is temporary: the chat says it retries, not the host's words.
    assert!(!failures.iter().flatten().any(|failure| failure.contains("timed out")), "{failures:?}");
}

#[test]
fn failures_reconnect_with_growing_pauses_and_none_waits_for_the_owner() {
    let dir = TempDir::new("dot-reconnect-policy");
    let state = Arc::new(State::new(dir.path()).unwrap());
    let bridge = Bridge::new(Arc::clone(&state), "local".into(), Arc::new(AtomicBool::new(false))).unwrap();
    let shown = std::sync::Mutex::new(Vec::new());
    let output = |update: Update| shown.lock().unwrap().push(update.failure);
    let mut worker = Worker::new(bridge, &output);
    let pause = |worker: &Worker| match worker.link {
        Link::Down(at) => Some(at.saturating_duration_since(Instant::now())),
        Link::Open(_) => None,
    };
    // A temporary failure waits 5, 10, 20 ... seconds, at most five minutes.
    let mut expected = RETRY_FIRST;
    for _ in 0..8 {
        worker.failed(Error::Connection("Native request timed out.".into()), true);
        let waited = pause(&worker).unwrap();
        assert!(waited <= expected && waited + Duration::from_secs(1) > expected, "{waited:?} {expected:?}");
        expected = (expected * 2).min(RETRY_MAX);
    }
    // A problem after connecting is checked once by connecting again at once,
    // then waits like a temporary failure until a refresh succeeds.
    worker.retry = RETRY_FIRST;
    worker.failed(Error::Attention("Native Dot request returned HTTP 404.".into()), false);
    assert!(pause(&worker).unwrap() < Duration::from_secs(1));
    worker.failed(Error::Attention("Native Dot request returned HTTP 404.".into()), false);
    assert!(pause(&worker).unwrap() > Duration::from_secs(4));
    // A problem found while connecting, such as a login another host is
    // renewing, connects again by itself with the same growing pause
    // (2026-11-12: the chat kept "Native ChatGPT login is unavailable." after
    // the renewed login reached every host).
    worker.retry = RETRY_FIRST;
    for expected in [RETRY_FIRST, RETRY_FIRST * 2] {
        worker.failed(Error::Attention("Native ChatGPT login is unavailable.".into()), true);
        let waited = pause(&worker).expect("the chat connects again by itself");
        assert!(waited <= expected && waited + Duration::from_secs(1) > expected, "{waited:?} {expected:?}");
    }
    let last = shown.lock().unwrap().last().cloned().flatten();
    assert_eq!(last.as_deref(), Some("Native ChatGPT login is unavailable. Retrying automatically."));
    // A recovered room clears the failure even when no messages changed;
    // idle Dot need not produce another message to remove its old error.
    worker.messages = json!([]);
    worker.opened(snapshot(json!([])));
    assert!(matches!(worker.link, Link::Open(_)));
    assert_eq!(shown.lock().unwrap().last(), Some(&None));
}

// The chat refreshes when the host announces a live activity change, not
// on a fixed timer: with nothing changing, the next timed refresh is a minute away.
#[test]
fn a_live_announcement_refreshes_at_once() {
    let dir = TempDir::new("dot-live-refresh");
    let state = Arc::new(State::new(dir.path()).unwrap());
    let (client, server) = UnixStream::pair().unwrap();
    let (commands, input) = mpsc::channel();
    let announced = commands.clone();
    let events: bridge::Events = Arc::new(move || {
        let _ = announced.send(Work::Live);
    });
    let bridge = Bridge::over(Arc::clone(&state), client, Some(events));
    let (refreshed, refreshes) = mpsc::channel();
    let host = std::thread::spawn(move || {
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let mut writer = server;
        for step in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["action"], "snapshot");
            writeln!(writer, "{}", json!({"id":request["id"], "ok":true, "result":snapshot(json!([]))})).unwrap();
            if step == 0 {
                writeln!(writer, "{{\"event\":\"live\"}}").unwrap();
            }
            refreshed.send(()).unwrap();
        }
    });
    let output = |_: Update| {};
    let mut worker = Worker::new(bridge, &output);
    worker.messages = json!([]);
    worker.failure = None;
    worker.idle = IDLE_ANNOUNCED;
    let driver = std::thread::spawn(move || {
        let first = refreshes.recv_timeout(Duration::from_secs(10)).is_ok();
        let second = first && refreshes.recv_timeout(Duration::from_secs(10)).is_ok();
        commands.send(Work::Stop).unwrap();
        second
    });
    worker.run(input, Arc::new(AtomicBool::new(false)));
    assert!(driver.join().unwrap(), "the announcement did not refresh the conversation");
    host.join().unwrap();
}

#[test]
fn a_newer_build_takes_over_the_chat_only_once_it_shows_it_runs_dot() {
    let dir = TempDir::new("dot-newer-build");
    let asked = dir.path().join("asked");
    let script = |answer: &str| {
        format!("#!/bin/sh\n[ \"$1 $2\" = 'dot --version' ] || exit 0\necho x >> '{}'\n{answer}\n", asked.display())
    };
    let exe = dir.executable("cepheus", script("exit 1"));
    let link = PathBuf::from(format!("{} (deleted)", exe.display()));
    let mut declined = None;
    // A build that cannot run Dot is declined, and not asked again.
    assert_eq!(newer_build(&link, &mut declined), None);
    assert_eq!(newer_build(&link, &mut declined), None);
    assert_eq!(std::fs::read_to_string(&asked).unwrap().lines().count(), 1);
    // The build installed over it runs Dot and takes over.
    std::fs::rename(dir.executable("next", script("echo 'cepheus dot 9.9.9'")), &exe).unwrap();
    assert_eq!(newer_build(&link, &mut declined), Some(exe.clone()));
    // A program whose file was not replaced has no newer build.
    assert_eq!(newer_build(&exe, &mut declined), None);
}
