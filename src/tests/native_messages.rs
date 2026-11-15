use super::*;
use crate::testsupport::TempDir;
use serde_json::json;

const THREAD: &str = "00000000-0000-4000-8000-000000000001";

#[path = "native_owner_input.rs"]
mod owner_input;

// One executable serves an interactive composer and the provider's native CLI.
// Its inbox and PTY input are independent, just as the real shared daemon is.
pub(super) fn fixture() -> (TempDir, Service, Receiver<Ev>, Receiver<Out>, u64, NativeServer) {
    fixture_with(None, None)
}

fn fixture_with(
    resume: Option<&str>,
    prompt: Option<&str>,
) -> (TempDir, Service, Receiver<Ev>, Receiver<Out>, u64, NativeServer) {
    let dir = TempDir::new("native-message");
    let server = NativeServer::start(dir.path());
    let program = dir.executable(
        "codex",
        r#"#!/usr/bin/python3
import json, os, signal, sys, time, tty
from pathlib import Path
codex_home = str(Path.cwd() / "codex-home")
if os.environ.get("CODEX_HOME") != codex_home:
    os.execve(sys.executable, [sys.executable, *sys.argv], dict(os.environ, CODEX_HOME=codex_home))
if len(sys.argv) == 1:
    sys.exit(0)
if sys.argv[1] == "queue":
    if "--help" in sys.argv:
        print("Unavailable" if Path("unsupported").exists() else "Queue a message --thread THREAD --message TEXT")
        sys.exit(0)
    Path("native-started").touch()
    with open("native-attempts", "a") as attempts:
        attempts.write("attempt\n")
    while Path("pause").exists():
        time.sleep(0.02)
    text = sys.argv[sys.argv.index("--message") + 1]
    if Path("reject").exists():
        sys.exit(1)
    with open("inbox", "a") as inbox:
        inbox.write(json.dumps(text) + "\n")
    print("Queued message test for thread " + sys.argv[sys.argv.index("--thread") + 1] + ".")
    sys.exit(0)
signal.signal(signal.SIGHUP, lambda *_: None if Path("ignore-hup").exists() else sys.exit(0))
with open("launches", "a") as launches:
    launches.write(json.dumps(sys.argv[1:]) + "\n")
tty.setraw(0)
os.write(1, b"\x1b[?2004hready\r\n\r\n\xe2\x80\xba \x1b[2mAsk Codex to do anything\x1b[0m\r\n\r\n\x1b[2m  GPT-6.1-Sol xhigh \xc2\xb7 Test conversation\x1b[0m\r\n\x1b[3A\x1b[3G")
while True:
    data = os.read(0, 4096)
    if not data:
        break
    while Path("guarded-pause").exists():
        time.sleep(0.01)
    with open("typed", "ab") as typed:
        typed.write(data)
    if Path("guarded").exists() and b"\x1b[200~" in data and b"\x1b[201~" in data:
        body = data.split(b"\x1b[200~", 1)[1].split(b"\x1b[201~", 1)[0].decode()
        lines = body.split("\n")
        shown = "\r\n".join(["› " + lines[0], *lines[1:]])
        row = 3 + len(lines) - 1
        column = len(lines[-1]) + (3 if len(lines) == 1 else 1)
        os.write(1, ("\x1b[2J\x1b[Hready\r\n\r\n" + shown + "\r\n\r\n\x1b[2m  GPT-6.1-Sol xhigh · Test conversation\x1b[0m" + f"\x1b[{row};{column}H").encode())
    elif Path("guarded").exists() and data == b"\r":
        os.write(1, b"\x1b[2J\x1b[Hready\r\n\r\n\xe2\x80\xba \x1b[2mAsk Codex to do anything\x1b[0m\r\n\r\n\x1b[2m  GPT-6.1-Sol xhigh \xc2\xb7 Test conversation\x1b[0m\x1b[3;3H")
    else:
        os.write(1, data)
"#,
    ).canonicalize().unwrap();
    let mut opts = super::tests::options(&dir, "true");
    opts.codex_dir = dir.path().join("codex-home");
    fs::create_dir_all(&opts.codex_dir).unwrap();
    opts.config.codex_command = vec![
        program.to_string_lossy().into(),
        "--search".into(),
        "--remote".into(),
        format!("unix://{}", dir.path().join("codex.sock").display()),
    ];
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let (out, replies) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 7, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    let id = svc
        .create_with(
            Tool::Codex,
            resume.map(str::to_owned),
            &StartModel::default(),
            &Placement { cwd: Some(dir.path().into()), prompt: prompt.map(str::to_owned), ..Placement::default() },
        )
        .unwrap();
    until(&mut svc, &events, |s| {
        s.sessions[&id]
            .hosted
            .as_ref()
            .is_some_and(|h| screen::screen_text(&h.lock().term).join("\n").contains("ready"))
    });
    svc.sessions.get_mut(&id).unwrap().conversation = Some(THREAD.into());
    let request = crate::providers::messages::request(
        Tool::Codex,
        &svc.opts.config,
        THREAD,
        "identity check",
        dir.path(),
        &svc.opts.codex_dir,
    )
    .unwrap();
    assert!(
        request.matches_runtime(svc.sessions[&id].hosted.as_ref().unwrap().pid.unwrap()),
        "a directory-selected native client must remain eligible for its own daemon"
    );
    while replies.try_recv().is_ok() {}
    (dir, svc, events, replies, id, server)
}

pub(super) fn until(svc: &mut Service, events: &Receiver<Ev>, mut done: impl FnMut(&Service) -> bool) {
    let scale = std::env::var("CEPHEUS_TEST_TIMEOUT_SCALE").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(10);
    let deadline = Instant::now() + Duration::from_secs(5 * scale.max(1));
    while !done(svc) {
        assert!(Instant::now() < deadline, "message did not reach the expected state");
        svc.messages_due();
        svc.submit_due(Instant::now());
        if let Ok(ev) = events.recv_timeout(Duration::from_millis(10)) {
            svc.handle(ev);
        }
    }
}

fn notice(replies: &Receiver<Out>) -> Option<(String, bool)> {
    while let Ok(Out::Msg(bytes)) = replies.try_recv() {
        if let Some(ServerMsg::Notice { text, error }) = crate::protocol::read_msg(&mut bytes.as_slice()).unwrap() {
            return Some((text, error));
        }
    }
    None
}

#[test]
fn owner_messages_use_only_native_admission_without_terminal_readiness() {
    for scenario in ["unsupported", "question", "starting", "recovery", "deadline"] {
        let (dir, mut svc, events, replies, id, _server) = fixture();
        match scenario {
            "unsupported" => {
                dir.write("unsupported", "");
                dir.write("unsupported-control", "");
            }
            "question" => svc.sessions.get_mut(&id).unwrap().tracker.hook_asks("A native question".into()),
            "starting" => {
                let mut emu = svc.sessions[&id].hosted.as_ref().unwrap().lock();
                let crate::session::Emu { term, parser, .. } = &mut *emu;
                parser.advance(term, b"\x1b[?2004l");
            }
            "recovery" => svc.sessions.get_mut(&id).unwrap().recovery.phase = Some(recovery::Phase::Starting),
            "deadline" => {}
            _ => unreachable!(),
        }
        if scenario == "deadline" {
            dir.write("pause", "");
            let request = crate::providers::messages::request(
                Tool::Codex,
                &svc.opts.config,
                THREAD,
                "bounded native owner message",
                dir.path(),
                &svc.opts.codex_dir,
            )
            .unwrap()
            .for_owner(svc.sessions[&id].hosted.as_ref().unwrap().pid.unwrap())
            .with_steering(true);
            let result = request.deliver(Instant::now() + Duration::from_secs(2), &AtomicBool::new(false));
            let error = match result {
                crate::providers::messages::Delivery::Failed(error) => error,
                _ => panic!("a blocked native send must expire without another transport"),
            };
            assert!(error.contains("expired") || error.contains("unknown"), "{error}");
            assert_eq!(fs::read_to_string(dir.path().join("native-attempts")).unwrap(), "attempt\n");
            assert!(!dir.path().join("inbox").exists());
            assert!(!dir.path().join("typed").exists());
            svc.begin_shutdown(false);
            continue;
        }
        svc.send_message(7, id, 401, "owner message through the native interface", false);
        let mut result = None;
        until(&mut svc, &events, |_| {
            result = receipt(&replies).or(result.take());
            result.is_some()
        });
        let (_, request, error, delivered) = result.unwrap();
        assert_eq!(request, 401);
        if scenario == "unsupported" {
            assert!(error.is_some(), "an unsupported native API must refuse rather than paste");
            assert_eq!(delivered, None);
            assert!(!dir.path().join("inbox").exists());
        } else {
            assert!(error.is_none(), "native admission must not depend on terminal readiness: {scenario}: {error:?}");
            // Owner input reaches the active turn without waiting for it to finish.
            assert_eq!(delivered, Some(crate::protocol::Delivered::Steered), "{scenario}");
            let inbox: Vec<String> = fs::read_to_string(dir.path().join("inbox"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(inbox, ["owner message through the native interface"], "exactly one native admission");
        }
        assert!(!dir.path().join("typed").exists(), "owner Send must never write terminal input: {scenario}");
        svc.begin_shutdown(false);
    }
}

#[test]
fn owner_messages_bind_the_loaded_backend_thread_instead_of_the_startup_resume() {
    const CURRENT: &str = "00000000-0000-4000-8000-000000000002";
    for scenario in ["changed thread", "wrong-thread", "wrong-cwd"] {
        let (dir, mut svc, events, replies, id, _server) = fixture_with(Some(THREAD), None);
        if scenario == "changed thread" {
            svc.sessions.get_mut(&id).unwrap().conversation = Some(CURRENT.into());
            dir.write("current-thread", CURRENT);
        } else {
            dir.write(scenario, "");
        }
        svc.send_message(7, id, 402, "captured native backend", false);
        let mut result = None;
        until(&mut svc, &events, |_| {
            result = receipt(&replies).or(result.take());
            result.is_some()
        });
        let (_, _, error, _) = result.unwrap();
        if scenario == "changed thread" {
            assert!(
                error.is_none(),
                "a positively identified native thread is independent of the initial TUI resume: {error:?}"
            );
            let inbox: Vec<String> = fs::read_to_string(dir.path().join("inbox"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(inbox, ["captured native backend"]);
        } else {
            assert!(error.is_some(), "wrong native thread identity must fail closed");
            assert!(!dir.path().join("inbox").exists(), "identity must be checked before enqueue");
        }
        assert!(!dir.path().join("typed").exists());
        svc.begin_shutdown(false);
    }
}

#[test]
fn a_turn_resumed_with_its_continuation_keeps_its_native_inbox() {
    // Cold-start recovery resumes an interrupted turn as
    // `codex resume ID --cd DIR -- MESSAGE`; that client is still the same
    // conversation's runtime, so later messages keep the native inbox.
    let (dir, mut svc, events, replies, id, _server) = fixture_with(Some(THREAD), Some("-carry on"));
    let launches = fs::read_to_string(dir.path().join("launches")).unwrap();
    let argv: Vec<String> = serde_json::from_str(launches.lines().next().unwrap()).unwrap();
    assert_eq!(argv[argv.len() - 2..], ["--", "-carry on"], "{argv:?}");
    svc.send_message(7, id, 402, "after the resumed turn", false);
    let mut result = None;
    until(&mut svc, &events, |_| {
        result = receipt(&replies).or(result.take());
        result.is_some()
    });
    assert_eq!(result.unwrap().2, None);
    let inbox: Vec<String> = fs::read_to_string(dir.path().join("inbox"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(inbox, ["after the resumed turn"]);
    assert!(!dir.path().join("typed").exists(), "nothing is typed into the resumed client");
    svc.begin_shutdown(false);
}

#[test]
fn native_messages_preserve_the_desktop_draft_and_acknowledge_admission() {
    let (dir, mut svc, events, replies, id, _server) = fixture();
    svc.handle_client(7, ClientMsg::Input { id, data: "unfinished desktop draft".into() });
    until(&mut svc, &events, |_| dir.path().join("typed").exists());
    svc.broadcast_message(7, &[id], "phone message with $() and '\nsecond line");
    assert!(notice(&replies).is_none(), "success must wait for native admission");
    until(&mut svc, &events, |_| dir.path().join("inbox").exists());
    let mut result = None;
    until(&mut svc, &events, |_| {
        result = notice(&replies).or(result.take());
        result.is_some()
    });
    let (_, error) = result.expect("delivery result");
    assert!(!error);
    assert_eq!(fs::read_to_string(dir.path().join("typed")).unwrap(), "unfinished desktop draft");
    let messages: Vec<String> = fs::read_to_string(dir.path().join("inbox"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(messages, ["phone message with $() and '\nsecond line"]);
    svc.begin_shutdown(false);
}

#[test]
fn rapid_native_messages_are_distinct_and_keep_their_order() {
    let (dir, mut svc, events, replies, id, _server) = fixture();
    for message in ["first", "second", "third"] {
        svc.broadcast_message(7, &[id], message);
    }
    let mut results = 0;
    until(&mut svc, &events, |_| {
        while let Some((_, error)) = notice(&replies) {
            assert!(!error);
            results += 1;
        }
        results == 3
    });
    let messages: Vec<String> = fs::read_to_string(dir.path().join("inbox"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(messages, ["first", "second", "third"]);
    assert!(!dir.path().join("typed").exists(), "native messages never enter the composer");
    svc.begin_shutdown(false);
}

#[test]
fn rejected_native_delivery_never_retries_through_the_terminal() {
    let (dir, mut svc, events, replies, id, _server) = fixture();
    dir.write("reject", "");
    svc.broadcast_message(7, &[id], "send once");
    let mut result = None;
    until(&mut svc, &events, |_| {
        result = notice(&replies).or(result.take());
        result.is_some()
    });
    assert!(result.unwrap().1, "native rejection must reach the caller");
    assert!(!dir.path().join("inbox").exists());
    assert!(!dir.path().join("typed").exists(), "a failed native send cannot become a duplicate terminal send");
    svc.begin_shutdown(false);
}

/// Idle chores go through the provider's own inbox, exactly once, and
/// reconcile its asynchronous receipt, including a completion report that
/// arrives before the receipt event is drained. The terminal is never read
/// or typed into on their behalf.
#[test]
fn idle_messages_use_the_native_inbox_exactly_once() {
    for scenario in [
        "rejected",
        "fast completion",
        "later owner turn",
        "recovery Stop",
        "late identity",
        "handover",
        "handover Stop",
        "legacy handover",
    ] {
        let (dir, mut svc, mut events, _replies, id, _server) = fixture();
        eprintln!("idle native admission scenario: {scenario}");
        dir.write("pause", "");
        let now = util::now_secs();
        let s = svc.sessions.get_mut(&id).unwrap();
        s.tracker.status = Status::Idle;
        // An unrelated composer on screen neither blocks nor receives it.
        {
            let mut emu = s.hosted.as_ref().unwrap().lock();
            let crate::session::Emu { term, parser, .. } = &mut *emu;
            parser.advance(term, "\r\n› text the service never reads".as_bytes());
        }
        let mut spec: crate::work_queue::Spec = serde_json::from_value(json!({
            "key":"idle-admission", "title":"idle admission", "kind":"session", "cwd":dir.path(),
            "provider":"codex", "placement":"local", "prompt":"private idle task"
        }))
        .unwrap();
        let q = svc.work_queue.as_mut().unwrap();
        q.chore_register(
            serde_json::from_value(json!({
                "id":"idle-admission", "owner":"owner", "repository":"fixture", "title":"idle admission",
                "target":"idle", "schedule":{"cron":"0 9 * * *","timezone":"UTC"}, "spec":spec
            }))
            .unwrap(),
            now,
        )
        .unwrap();
        spec.chore = Some(crate::work_queue::chores::Context {
            id: "idle-admission".into(),
            day: "2026-11-06".into(),
            key: "claim".into(),
            generation: 1,
            token: "token".into(),
            session: "fixture".into(),
        });
        let item = q.add_batch(vec![spec.clone()], now).unwrap()[0].id;
        spec.key = q.records[&item].dispatch_key.clone();
        let pending = svc.work_idle_start(&spec, Some(item)).unwrap_err();
        assert!(pending.to_string().contains("awaits its admission receipt"), "{pending:#}");
        until(&mut svc, &events, |_| dir.path().join("native-started").exists());
        assert!(svc.pending_submits.iter().all(|p| p.id != id), "no terminal Enter is ever scheduled");
        assert!(svc.work_idle_session(&spec.key).unwrap().is_none());
        if scenario == "rejected" {
            dir.write("reject", "");
        }
        if scenario == "late identity" {
            svc.sessions.get_mut(&id).unwrap().conversation = Some("00000000-0000-4000-8000-000000000002".into());
            // Keep provider admission gated until the identity change has
            // canceled and reaped its worker; otherwise the fixture races it.
            until(&mut svc, &events, |s| !s.messages.busy());
        }
        fs::remove_file(dir.path().join("pause")).unwrap();
        if scenario == "fast completion" {
            // The provider accepted it, but deliberately leave its receipt
            // event queued until the completed-turn counter has advanced.
            let deadline = Instant::now() + Duration::from_secs(20);
            while !dir.path().join("inbox").exists() {
                assert!(Instant::now() < deadline, "native request was not admitted");
                thread::sleep(Duration::from_millis(20));
            }
            svc.sessions.get_mut(&id).unwrap().tracker.completed_turns += 1;
        }
        until(&mut svc, &events, |s| !s.messages.busy());
        let attempts = fs::read_to_string(dir.path().join("native-attempts")).unwrap();
        assert_eq!(attempts, "attempt\n", "exactly one native admission attempt");
        let inbox = fs::read_to_string(dir.path().join("inbox")).unwrap_or_default();
        assert!(!dir.path().join("typed").exists(), "idle delivery never types into the terminal");
        assert!(!dir.path().join("controls").exists(), "automatic idle input must retain queued admission");
        if matches!(scenario, "rejected" | "late identity") {
            assert!(svc.work_idle_start(&spec, Some(item)).is_err());
            assert!(!svc.messages.busy(), "uncertain admission must not resend the reserved request");
            assert!(svc.work_idle_session(&spec.key).unwrap().is_none());
            svc.sessions.get_mut(&id).unwrap().tracker.completed_turns += 1;
            svc.work_save_turn_receipts(id).unwrap();
            assert!(
                svc.work_retired_receipt(&spec.key).unwrap().is_none(),
                "unaccepted requests must not acquire turn tombstones"
            );
            assert!(inbox.is_empty(), "a refused or canceled admission leaves the inbox empty");
            let attempts = fs::read_to_string(dir.path().join("native-attempts")).unwrap();
            assert_eq!(attempts, "attempt\n", "an uncertain admission is never sent again");
        } else {
            let inbox: Vec<String> = inbox.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            assert_eq!(inbox.len(), 1, "exactly one native admission");
            assert!(inbox[0].contains("private idle task"));
            let accepted = svc.work_idle_start(&spec, Some(item)).unwrap();
            let q = svc.work_queue.as_mut().unwrap();
            q.records.get_mut(&item).unwrap().turn_anchor = accepted["turn_anchor"].as_u64();
            q.transition(
                item,
                "dispatched",
                "admitted",
                now,
                Some(crate::work_queue::Dispatch { kind: "session".into(), id }),
            )
            .unwrap();
            if scenario.contains("handover") {
                let inputs = svc.sessions[&id].inputs;
                let spawned_ms = svc.sessions[&id].spawned_ms;
                assert!(inputs > 0, "admission advanced input custody");
                let config = svc.opts.config.clone();
                let codex_dir = svc.opts.codex_dir.clone();
                let mut state = svc.prepare_handover(Fds { listener: 100, lock: 101 }).unwrap();
                assert_eq!(state.sessions[0].inputs, Some(inputs));
                if scenario == "legacy handover" {
                    state.sessions[0].inputs = None;
                }
                let path = handover::handover_path(&svc.opts.socket);
                state.write(&path).unwrap();
                for s in svc.sessions.values_mut() {
                    if let Some(h) = s.hosted.take() {
                        h.release();
                    }
                }
                svc.registry_dirty = true;
                svc.save_registry();
                let mut opts = super::tests::options(&dir, "true");
                opts.config = config;
                opts.codex_dir = codex_dir;
                let (tx, restored_events) = mpsc::channel();
                svc = Service::new(opts, tx);
                svc.adopt(Handover::take(&path).unwrap());
                events = restored_events;
                until(&mut svc, &events, |s| {
                    screen::screen_text(&s.sessions[&id].hosted.as_ref().unwrap().lock().term)
                        .join("\n")
                        .contains("ready")
                });
                assert_eq!(svc.sessions[&id].spawned_ms, spawned_ms);
                assert_eq!(svc.sessions[&id].retry.input_revision, Some(0), "fresh output is still required");
                if scenario == "legacy handover" {
                    assert_eq!(svc.sessions[&id].inputs, 0);
                    assert!(svc.work_idle_session(&spec.key).is_err(), "missing custody must fail closed");
                } else {
                    assert_eq!(svc.sessions[&id].inputs, inputs, "handover must preserve accepted input custody");
                    assert_eq!(svc.work_idle_session(&spec.key).unwrap(), Some(id));
                    assert_eq!(svc.work_turn_receipt(&spec.key).unwrap()["session"].as_u64(), Some(id));
                }
                assert_eq!(
                    fs::read_to_string(dir.path().join("native-attempts")).unwrap(),
                    "attempt\n",
                    "handover cannot send it again"
                );
                assert!(!dir.path().join("typed").exists());
                if scenario != "handover Stop" {
                    svc.sessions.get_mut(&id).unwrap().tracker.completed_turns += 1;
                }
            }
            if scenario == "later owner turn" {
                svc.client_input(id, "\x1b").unwrap();
                svc.client_input(id, "unrelated owner task\r").unwrap();
                svc.sessions.get_mut(&id).unwrap().tracker.completed_turns += 1;
            } else if matches!(scenario, "recovery Stop" | "handover Stop") {
                let s = svc.sessions.get_mut(&id).unwrap();
                s.recovery.phase = Some(recovery::Phase::Replaying);
                s.recovery.draft = Some("preserved owner draft".into());
                svc.client_input(id, "\x1b").unwrap();
                assert!(svc.sessions[&id].recovery.paused);
                svc.resume_recovery_input(id);
                let s = svc.sessions.get_mut(&id).unwrap();
                s.recovery.phase = None;
                s.tracker.completed_turns += 1;
                assert_eq!(s.recovery.draft.as_deref(), Some("preserved owner draft"));
            }
            svc.work_observe_dispatches(now + 1);
            let state = &svc.work_queue.as_ref().unwrap().records[&item].info.state;
            if matches!(scenario, "fast completion" | "handover") {
                assert_eq!(
                    state, "done",
                    "a completion before the receipt event still belongs to the admitted request"
                );
            } else {
                assert_eq!(state, "held", "a later owner's completion must not finish the idle chore");
                svc.work_save_turn_receipts(id).unwrap();
                assert!(svc.work_retired_receipt(&spec.key).unwrap().is_none());
                assert!(svc.work_turn_receipt(&spec.key).is_err());
            }
        }
        svc.begin_shutdown(false);
    }
}

type Receipt = (u64, u64, Option<String>, Option<crate::protocol::Delivered>);

fn receipt(replies: &Receiver<Out>) -> Option<Receipt> {
    while let Ok(Out::Msg(bytes)) = replies.try_recv() {
        if let Some(ServerMsg::MessageSent { id, request, error, delivered }) =
            crate::protocol::read_msg(&mut bytes.as_slice()).unwrap()
        {
            return Some((id, request, error, delivered));
        }
    }
    None
}

#[test]
fn attachments_never_fall_back_to_terminal_input() {
    let (dir, mut svc, _events, replies, id, _server) = fixture();
    svc.send_message(7, id, 41, "/scratch/image.png", true);
    assert!(receipt(&replies).unwrap().2.unwrap().contains("native attachment"));
    assert!(!dir.path().join("typed").exists());
    assert!(!dir.path().join("inbox").exists(), "text-only queue cannot preserve image conversion");
    svc.begin_shutdown(false);
}

/// Without a native interface an idle message is refused, never typed; a
/// question or the owner's unsent draft holds it before anything is sent.
#[test]
fn idle_messages_without_a_native_interface_or_while_held_are_never_typed() {
    for hold in ["unsupported", "question", "owner draft"] {
        let (dir, mut svc, events, _replies, id, _server) = fixture();
        let s = svc.sessions.get_mut(&id).unwrap();
        s.tracker.status = Status::Idle;
        match hold {
            "unsupported" => {
                dir.write("unsupported", "");
            }
            "question" => s.tracker.hook_asks("Approve?".into()),
            "owner draft" => {
                svc.handle_client(7, ClientMsg::Input { id, data: "half-written owner draft".into() });
                until(&mut svc, &events, |_| dir.path().join("typed").exists());
                svc.sessions.get_mut(&id).unwrap().tracker.status = Status::Idle;
            }
            _ => unreachable!(),
        }
        let spec: crate::work_queue::Spec = serde_json::from_value(json!({
            "key":"idle-held", "title":"idle held", "kind":"session", "cwd":dir.path(),
            "provider":"codex", "placement":"local", "prompt":"private held task"
        }))
        .unwrap();
        let error = svc.work_idle_start(&spec, None).unwrap_err().to_string();
        if hold == "unsupported" {
            assert!(error.contains("awaits its admission receipt"), "{error}");
            until(&mut svc, &events, |s| !s.messages.busy());
            assert!(svc.work_idle_session(&spec.key).unwrap().is_none());
            assert!(svc.work_idle_bindings(id).unwrap().is_empty());
            assert!(svc.work_idle_start(&spec, None).is_err(), "an unconfirmed admission is never retried");
        } else {
            assert!(error.contains("no safely idle session"), "{error}");
        }
        assert!(svc.pending_submits.iter().all(|p| p.id != id));
        assert!(!dir.path().join("native-attempts").exists());
        assert!(!dir.path().join("inbox").exists());
        let typed = fs::read_to_string(dir.path().join("typed")).unwrap_or_default();
        let expected = if hold == "owner draft" { "half-written owner draft" } else { "" };
        assert_eq!(typed, expected, "the composer keeps exactly what the owner typed");
        svc.begin_shutdown(false);
    }
}

#[test]
fn a_question_revokes_prepared_guarded_enter_before_the_native_writer_accepts_it() {
    let (dir, mut svc, _events, _replies, id, _server) = fixture();
    let (prepared, waiting) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let (done, outcome) = mpsc::channel();
    let epoch = svc.sessions[&id].hosted.as_ref().unwrap().input_epoch();
    svc.sessions[&id]
        .hosted
        .as_ref()
        .unwrap()
        .write_guarded_prepared(
            b"\r".to_vec(),
            Instant::now() + Duration::from_secs(10),
            epoch,
            Arc::new(std::sync::Mutex::new(1)),
            1,
            move || {
                prepared.send(()).unwrap();
                resume.recv().unwrap();
                true
            },
            |_| true,
            move |accepted| {
                let _ = done.send(accepted);
            },
        )
        .unwrap();
    waiting.recv_timeout(Duration::from_secs(15)).unwrap();
    svc.withhold_submit(id);
    assert_ne!(
        svc.sessions[&id].hosted.as_ref().unwrap().input_epoch(),
        epoch,
        "the question revokes an already queued action even without a pending-submit entry"
    );
    release.send(()).unwrap();
    assert!(!outcome.recv_timeout(Duration::from_secs(15)).unwrap());
    assert!(!fs::read(dir.path().join("typed")).unwrap_or_default().contains(&b'\r'));
    svc.begin_shutdown(false);
}

#[test]
fn fallback_refuses_a_multiline_desktop_draft_without_changing_it() {
    let (dir, mut svc, events, replies, id, _server) = fixture();
    let draft = "first line\r\nsecond line";
    svc.handle_client(7, ClientMsg::Input { id, data: draft.into() });
    until(&mut svc, &events, |s| {
        s.sessions[&id]
            .hosted
            .as_ref()
            .is_some_and(|h| screen::screen_text(&h.lock().term).iter().any(|line| line.contains("second line")))
    });
    svc.send_message(7, id, 42, "phone attachment", true);
    let (_, request, error, _) = receipt(&replies).expect("refused draft");
    assert_eq!(request, 42);
    assert!(error.unwrap().contains("native attachment"));
    assert_eq!(fs::read_to_string(dir.path().join("typed")).unwrap(), draft);
    assert!(!dir.path().join("inbox").exists());
    svc.begin_shutdown(false);
}

#[test]
fn stopped_terminal_submission_settles_its_receipt_and_preserves_recovery_input() {
    let (_dir, mut svc, _events, replies, id, _server) = fixture();
    svc.sessions.get_mut(&id).unwrap().recovery.draft = Some("held attachment caption".into());
    svc.sessions.get_mut(&id).unwrap().recovery.phase = Some(recovery::Phase::Replaying);
    svc.send_message(7, id, 43, "held attachment caption", true);
    assert!(svc.pending_submits.iter().all(|p| p.id != id));
    svc.handle_client(7, ClientMsg::Input { id, data: "\x1b".into() });
    assert!(svc.pending_submits.iter().all(|p| p.id != id));
    svc.messages_due();
    let (_, request, error, _) = receipt(&replies).expect("stopped delivery receipt");
    assert_eq!(request, 43);
    assert!(error.is_some());
    assert!(!svc.messages.busy(), "a removed pending Enter must not strand the delivery queue");
    let s = &svc.sessions[&id];
    assert!(s.recovery.paused);
    assert!(
        s.recovery.draft.as_ref().is_some_and(|text| text.contains("held attachment caption"))
            || s.recovery.pending.iter().any(|text| text.contains("held attachment caption"))
    );
    svc.begin_shutdown(false);
}

#[test]
fn queued_messages_respect_a_cleared_chat_and_a_draft_restored_after_recovery() {
    for clearing in [false, true] {
        let (dir, mut svc, events, replies, id, _server) = fixture();
        dir.write("pause", "");
        svc.broadcast_message(7, &[id], "already submitting");
        until(&mut svc, &events, |_| dir.path().join("native-started").exists());
        if !clearing {
            svc.sessions.get_mut(&id).unwrap().recovery.phase = Some(recovery::Phase::Replaying);
        }
        svc.send_message(7, id, 44, "queued follow-up", false);
        svc.sessions.get_mut(&id).unwrap().recovery.phase = None;
        if clearing {
            svc.sessions.get_mut(&id).unwrap().chat.asked = Some((Instant::now(), util::now_ms()));
        } else {
            svc.client_input(id, "restored desktop draft").unwrap();
            until(&mut svc, &events, |s| {
                screen::screen_text(&s.sessions[&id].hosted.as_ref().unwrap().lock().term)
                    .iter()
                    .any(|line| line.contains("restored desktop draft"))
            });
        }
        fs::remove_file(dir.path().join("pause")).unwrap();
        let mut result = None;
        until(&mut svc, &events, |_| {
            result = receipt(&replies).or(result.take());
            result.is_some()
        });
        assert_eq!(
            result.unwrap().2.is_some(),
            clearing,
            "a service clear fences the backend; a desktop draft does not"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("inbox")).unwrap_or_default().contains("queued follow-up"),
            !clearing
        );
        if !clearing {
            assert_eq!(fs::read_to_string(dir.path().join("typed")).unwrap(), "restored desktop draft");
        }
        assert!(!svc.messages.busy());
        svc.begin_shutdown(false);
    }
}

#[test]
fn recovery_closure_holds_new_messages_without_reopening_explicit_close() {
    let (dir, mut svc, _events, replies, id, _server) = fixture();
    let s = svc.sessions.get_mut(&id).unwrap();
    s.recovery.phase = Some(recovery::Phase::Closing);
    s.closing = Some((Instant::now(), 1));
    svc.send_message(7, id, 45, "message while reconnecting", false);
    assert!(receipt(&replies).unwrap().2.is_some());
    assert!(svc.sessions[&id].recovery.pending.is_empty());
    assert!(!dir.path().join("typed").exists(), "held input must not touch the failed terminal");
    svc.sessions.get_mut(&id).unwrap().remove_when_ended = true;
    svc.send_message(7, id, 46, "message after Close", false);
    assert!(receipt(&replies).unwrap().2.is_some());
    assert!(svc.sessions[&id].recovery.pending.is_empty());
    assert!(!svc.messages.busy());
    svc.begin_shutdown(false);
}

#[test]
fn closing_or_stopping_a_session_cancels_its_unsent_native_backlog() {
    for stop in [false, true] {
        let (dir, mut svc, events, replies, id, _server) = fixture();
        dir.write("pause", "");
        svc.broadcast_message(7, &[id], "already submitting");
        svc.broadcast_message(7, &[id], "still queued");
        until(&mut svc, &events, |_| dir.path().join("native-started").exists());
        dir.write("ignore-hup", "");
        if stop {
            svc.handle_client(7, ClientMsg::Input { id, data: "\x1b".into() });
        } else {
            svc.close(id);
        }
        fs::remove_file(dir.path().join("pause")).unwrap();
        let mut results = Vec::new();
        until(&mut svc, &events, |_| {
            while let Some(result) = notice(&replies) {
                results.push(result);
            }
            results.len() == 2
        });
        let inbox = fs::read_to_string(dir.path().join("inbox")).unwrap_or_default();
        if stop {
            // Stop cancels the unsent backlog and interrupts the agent; it
            // never closes the session.
            until(&mut svc, &events, |_| dir.path().join("typed").exists());
            let s = &svc.sessions[&id];
            assert!(s.alive && s.closing.is_none() && s.hosted.is_some(), "Stop must not close the session");
            assert_eq!(fs::read_to_string(dir.path().join("typed")).unwrap(), "\x1b");
        }
        fs::remove_file(dir.path().join("ignore-hup")).unwrap();
        svc.begin_shutdown(false);
        assert!(results[0].1, "Close/Stop must cancel the in-flight delivery before activation");
        assert!(results[1].1, "queued message must be refused once Close starts");
        assert!(!inbox.contains("still queued"));
    }
}

pub(super) struct NativeServer {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl NativeServer {
    fn start(dir: &Path) -> Self {
        let listener = std::os::unix::net::UnixListener::bind(dir.join("codex.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&stop);
        let dir = dir.to_owned();
        let thread = thread::spawn(move || {
            let calls = Arc::new(Mutex::new(()));
            let mut connections = Vec::new();
            while !cancel.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let cancel = cancel.clone();
                let dir = dir.clone();
                let calls = calls.clone();
                connections.push(thread::spawn(move || {
                    stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
                    let Ok(mut ws) = tungstenite::accept(stream) else { return };
                    while !cancel.load(Ordering::SeqCst) {
                    let message = match ws.read() {
                        Ok(message) => message,
                        Err(tungstenite::Error::Io(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                            ) =>
                        {
                            continue;
                        }
                        Err(_) => break,
                    };
                    let Ok(v) = serde_json::from_str::<Value>(message.to_text().unwrap_or("")) else {
                        continue;
                    };
                    let Some(id) = v.get("id") else { continue };
                    let result = match v["method"].as_str().unwrap() {
                        "initialize" => json!({}),
                        "thread/start" => {
                            use std::io::Write;
                            {
                                let _append = calls.lock().unwrap();
                                let mut log =
                                    fs::OpenOptions::new().append(true).create(true).open(dir.join("new-starts")).unwrap();
                                writeln!(log, "{}", v["params"]).unwrap();
                            }
                            fs::write(dir.join("new-started"), "").unwrap();
                            while dir.join("pause-new").exists() && !cancel.load(Ordering::SeqCst) {
                                thread::sleep(Duration::from_millis(5));
                            }
                            let uuid = crate::util::uuid_v4().unwrap();
                            let model = v["params"]["model"].as_str().unwrap_or("gpt-6.1-sol");
                            let effort = v["params"]["config"]["model_reasoning_effort"].as_str().unwrap_or("high");
                            let result = json!({"model":model,"reasoningEffort":effort,"thread":{
                                "id":uuid,"model":model,"reasoningEffort":effort,"cwd":if dir.join("wrong-new-cwd").exists(){dir.join("other")}else{dir.clone()},"status":{"type":"idle"}}});
                            fs::write(dir.join(format!("new-{uuid}")), result.to_string()).unwrap();
                            result
                        }
                        "thread/name/set" => {
                            use std::io::Write;
                            if dir.join("rejected-new-name").exists() {
                                let _ = ws.send(tungstenite::Message::text(
                                    json!({"id":id,"error":{"code":-32602,"message":"Name refused"}}).to_string(),
                                ));
                                continue;
                            }
                            let thread = v["params"]["threadId"].as_str().unwrap();
                            let path = dir.join(format!("new-{thread}"));
                            let mut prepared: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
                            prepared["thread"]["name"] = v["params"]["name"].clone();
                            let receipt = dir.join("codex-home/sessions").join(format!("{thread}.jsonl"));
                            fs::create_dir_all(receipt.parent().unwrap()).unwrap();
                            fs::write(&receipt, format!("{}\n", json!({"type":"session_meta","payload":{"id":thread,"cwd":dir}}))).unwrap();
                            prepared["thread"]["path"] = json!(receipt);
                            fs::write(path, prepared.to_string()).unwrap();
                            let _append = calls.lock().unwrap();
                            let mut log = fs::OpenOptions::new().append(true).create(true).open(dir.join("new-names")).unwrap();
                            writeln!(log, "{}", v["params"]).unwrap();
                            json!({})
                        }
                        "thread/read" => {
                            if dir.join("pause-capability").exists() {
                                fs::write(dir.join("capability-started"), "").unwrap();
                                while dir.join("pause-capability").exists() && !cancel.load(Ordering::SeqCst) {
                                    thread::sleep(Duration::from_millis(5));
                                }
                            }
                            if let Some(prepared) = v["params"]["threadId"]
                                .as_str()
                                .and_then(|id| fs::read_to_string(dir.join(format!("new-{id}"))).ok())
                            {
                                serde_json::from_str(&prepared).unwrap()
                            } else {
                                json!({"thread":{"id":if dir.join("wrong-thread").exists(){"wrong".into()}else{fs::read_to_string(dir.join("current-thread")).unwrap_or_else(|_|THREAD.into())},"cwd":if dir.join("wrong-cwd").exists(){dir.join("other")}else{dir.clone()},"status":{"type":if dir.join("interrupted").exists() {"idle"} else {"active"}}}})
                            }
                        }
                        "thread/turns/list" => json!({"data":[{"id":"turn-1","status":"inProgress"}]}),
                        "thread/settings/update" | "turn/steer" | "thread/queue/add" | "turn/start" => {
                            use std::io::Write;
                            let mut calls =
                                fs::OpenOptions::new().append(true).create(true).open(dir.join("controls")).unwrap();
                            writeln!(calls, "{v}").unwrap();
                            if v["method"] == "turn/start" && !dir.join("unsupported-control").exists() && !dir.join("rejected-control").exists() {
                                fs::write(dir.join("native-started"), "").unwrap();
                                let mut attempts = fs::OpenOptions::new().append(true).create(true).open(dir.join("native-attempts")).unwrap();
                                writeln!(attempts, "attempt").unwrap();
                                while dir.join("pause").exists() && !cancel.load(Ordering::SeqCst) { thread::sleep(Duration::from_millis(5)); }
                                if cancel.load(Ordering::SeqCst) { break; }
                                if dir.join("reject").exists() {
                                    let _ = ws.send(tungstenite::Message::text(json!({"id":id,"error":{"code":-32602,"message":"Input refused"}}).to_string()));
                                    continue;
                                }
                                let text = v["params"]["input"].as_array().unwrap().iter()
                                    .filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n");
                                let mut inbox = fs::OpenOptions::new().append(true).create(true).open(dir.join("inbox")).unwrap();
                                writeln!(inbox, "{}", serde_json::to_string(&text).unwrap()).unwrap();
                                if dir.join("interrupted").exists() { fs::write(dir.join("woken"), "").unwrap(); }
                            }
                            while dir.join("pause-control-reply").exists() && !cancel.load(Ordering::SeqCst) {
                                thread::sleep(Duration::from_millis(5));
                            }
                            if dir.join("lose-control-reply").exists() {
                                break;
                            }
                            if dir.join("unsupported-control").exists() {
                                let _ = ws.send(tungstenite::Message::text(
                                    json!({"id":id,"error":{"code":-32601,"message":"Method not found"}}).to_string(),
                                ));
                                continue;
                            }
                            if dir.join("rejected-control").exists() {
                                let _ = ws.send(tungstenite::Message::text(
                                    json!({"id":id,"error":{"code":-32602,"message":"Invalid params"}}).to_string(),
                                ));
                                continue;
                            }
                            if v["method"] == "thread/settings/update" {
                                if let Some(thread) = v["params"]["threadId"].as_str() {
                                    if let Ok(prepared) = fs::read_to_string(dir.join(format!("new-{thread}"))) {
                                        let prepared: Value = serde_json::from_str(&prepared).unwrap();
                                        let receipt = prepared["thread"]["path"].as_str().unwrap();
                                        let snapshot = json!({
                                            "model":v["params"]["model"],
                                            "reasoning_effort":v["params"]["effort"],
                                            "cwd":if dir.join("bad-settings-receipt").exists(){dir.join("other")}else{dir.clone()}
                                        });
                                        let mut log = fs::OpenOptions::new().append(true).open(receipt).unwrap();
                                        writeln!(log, "{}", json!({"type":"event_msg","payload":{
                                            "type":"thread_settings_applied","thread_id":thread,"thread_settings":snapshot
                                        }})).unwrap();
                                        if dir.join("stale-settings-prefix").exists() {
                                            writeln!(log, "{}", json!({"type":"event_msg","payload":{
                                                "type":"thread_settings_applied","thread_id":thread,
                                                "thread_settings":{"cwd":dir,"model":v["params"]["model"],"reasoning_effort":"low"}
                                            }})).unwrap();
                                        }
                                    }
                                }
                            }
                            match v["method"].as_str().unwrap() {
                                "turn/start" => json!({"turn":{"id":"turn-1","status":"inProgress"}}),
                                "turn/steer" => json!({"turnId":v["params"]["expectedTurnId"]}),
                                "thread/queue/add" => {
                                    json!({"queuedSubmission":{"id":"queued-1","clientUserMessageId":v["params"]["clientUserMessageId"]}})
                                }
                                _ => json!({}),
                            }
                        }
                        "thread/queue/start" => {
                            fs::write(dir.join("woken"), "").unwrap();
                            json!({"turn":{"status":"inProgress"}})
                        }
                        other => panic!("unexpected native RPC {other}"),
                    };
                    if ws.send(tungstenite::Message::text(json!({"id":id,"result":result}).to_string())).is_err() {
                        break;
                    }
                    }
                }));
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self { stop, thread: Some(thread) }
    }
}

#[test]
fn owner_messages_cancel_before_enqueue_when_the_owner_explicitly_starts_a_new_chat() {
    for command in ["/new\r", "/clear\r"] {
        let (dir, mut svc, events, replies, id, _server) = fixture();
        dir.write("pause-capability", "");
        svc.send_message(7, id, 701, "captured old conversation", false);
        svc.send_message(7, id, 702, "queued old conversation", false);
        until(&mut svc, &events, |_| dir.path().join("capability-started").exists());
        svc.handle_client(7, ClientMsg::Input { id, data: command.into() });
        assert!(svc.sessions[&id].chat.asked.is_some());
        fs::remove_file(dir.path().join("pause-capability")).unwrap();
        let mut results = Vec::new();
        until(&mut svc, &events, |_| {
            while let Some(result) = receipt(&replies) {
                results.push(result);
            }
            results.len() == 2
        });
        assert!(results.iter().all(|(_, _, error, _)| error.is_some()));
        assert!(!dir.path().join("native-attempts").exists());
        assert!(!dir.path().join("inbox").exists());
        svc.begin_shutdown(false);
    }
}
impl Drop for NativeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn a_new_native_message_wakes_a_previously_interrupted_thread() {
    let (dir, mut svc, events, replies, id, _server) = fixture();
    dir.write("interrupted", "");
    svc.broadcast_message(7, &[id], "continue with this message");
    let mut result = None;
    until(&mut svc, &events, |_| {
        result = notice(&replies).or(result.take());
        result.is_some()
    });
    svc.begin_shutdown(false);
    let (text, error) = result.unwrap();
    assert!(!error);
    assert!(text.ends_with("(submitted)"), "native admission is confirmed: {text}");
    assert!(dir.path().join("woken").exists(), "durable queue admission alone leaves interrupted threads asleep");
    assert!(!dir.path().join("typed").exists());
}

#[test]
fn native_provider_controls_keep_drafts_and_question_ids_out_of_terminal_navigation() {
    let (dir, mut svc, events, replies, id, _server) = fixture();
    svc.handle_client(7, ClientMsg::Input { id, data: "keep this desktop draft".into() });
    until(&mut svc, &events, |_| dir.path().join("typed").exists());
    let path = svc.opts.codex_dir.join(format!("sessions/2026/11/06/rollout-2026-11-06T00-00-00-{THREAD}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let question = json!({"type":"event_msg","payload":{"type":"item_completed","item":{
        "type":"AgentMessage","id":"ask","content":[{"type":"Text","text":"Choose"}],
        "questions":[{"title":"Identical wording?","options":["Yes","No"]},
                     {"title":"Identical wording?","options":["Yes","No"]}]}}});
    fs::write(&path, format!("{question}\n")).unwrap();
    // A question this conversation's transcript does not hold open is refused
    // with the code phones read, and nothing reaches the agent.
    svc.handle_client(
        7,
        ClientMsg::Plugin {
            request: 800,
            plugin: "provider-codex".into(),
            command: "control".into(),
            args: json!({"id":id,"conversation":THREAD,"action":"question_reply","question":"elsewhere:0","answer":"Yes"}),
        },
    );
    let mut refused = None;
    until(&mut svc, &events, |_| {
        while let Ok(Out::Msg(bytes)) = replies.try_recv() {
            if let Some(ServerMsg::PluginResult { request: 800, data, error }) =
                crate::protocol::read_msg(&mut bytes.as_slice()).unwrap()
            {
                refused = Some((data, error));
            }
        }
        refused.is_some()
    });
    let (data, error) = refused.unwrap();
    assert_eq!(data, json!({"code": crate::protocol::QUESTION_STALE}));
    assert!(error.as_deref().is_some_and(|e| e.contains("no longer belongs to this conversation")), "{error:?}");
    assert!(!dir.path().join("controls").exists(), "a refused answer reaches no native control");
    for (request, mut args) in [
        (801, json!({"action":"settings","model":"gpt-6.1-sol","effort":"high"})),
        (802, json!({"action":"question_reply","question":"ask:1","answer":"No"})),
        (803, json!({"action":"question_reply","question":"ask:0","answer":"Yes"})),
    ] {
        if request == 803 {
            dir.write("interrupted", "");
        }
        args["id"] = json!(id);
        args["conversation"] = json!(THREAD);
        svc.handle_client(
            7,
            ClientMsg::Plugin { request, plugin: "provider-codex".into(), command: "control".into(), args },
        );
        let mut result = None;
        until(&mut svc, &events, |_| {
            while let Ok(Out::Msg(bytes)) = replies.try_recv() {
                if let Some(ServerMsg::PluginResult { request: token, data, error }) =
                    crate::protocol::read_msg(&mut bytes.as_slice()).unwrap()
                {
                    if token == request {
                        result = Some((data, error));
                    }
                }
            }
            result.is_some()
        });
        let (data, error) = result.unwrap();
        assert!(error.is_none(), "{error:?}");
        assert_eq!(data["accepted"], true);
    }
    let calls: Vec<Value> = fs::read_to_string(dir.path().join("controls"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(calls[0]["method"], "thread/settings/update");
    assert_eq!(calls[0]["params"], json!({"threadId":THREAD,"model":"gpt-6.1-sol","effort":"high"}));
    assert_eq!(calls[1]["method"], "turn/steer");
    assert_eq!(calls[1]["params"]["expectedTurnId"], "turn-1");
    let answer = calls[1]["params"]["input"][0]["text"].as_str().unwrap();
    let body = answer
        .strip_prefix("<send_user_message_question_reply>\n")
        .unwrap()
        .strip_suffix("\n</send_user_message_question_reply>")
        .unwrap();
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body[0]["questionItemId"], json!(["request_user_input_async", "ask", 1]).to_string());
    assert_eq!(body[0]["answer"], "No");
    assert_eq!(calls[2]["method"], "thread/queue/add");
    assert!(dir.path().join("woken").exists());
    assert_eq!(fs::read_to_string(dir.path().join("typed")).unwrap(), "keep this desktop draft");
    svc.begin_shutdown(false);
}

#[test]
fn native_control_fallback_requires_explicit_unsupported_and_never_retries_uncertain_delivery() {
    use crate::providers::messages::controls::Control;
    for scenario in [
        "unsupported-control",
        "lose-control-reply",
        "rejected-control",
        "wrong-thread",
        "authenticated-control",
        "relative-endpoint",
        "expired-control",
    ] {
        let (dir, mut svc, _events, _replies, id, _server) = fixture();
        dir.write(scenario, "");
        if scenario == "authenticated-control" {
            svc.opts.config.codex_command.extend(["--remote-auth-token-env".into(), "CODEX_CONTROL_TEST_TOKEN".into()]);
        }
        if scenario == "relative-endpoint" {
            let endpoint = svc.opts.config.codex_command.iter_mut().find(|arg| arg.starts_with("unix://")).unwrap();
            *endpoint = "unix://codex.sock".into();
        }
        let request = crate::providers::messages::request(
            Tool::Codex,
            &svc.opts.config,
            THREAD,
            "",
            dir.path(),
            &svc.opts.codex_dir,
        )
        .unwrap();
        let pid = svc.sessions[&id].hosted.as_ref().unwrap().pid.unwrap();
        let result = request.control_until(
            pid,
            Control::Settings { model: Some("gpt-6.1-sol".into()), effort: None },
            if scenario == "expired-control" {
                Instant::now() - Duration::from_secs(1)
            } else {
                Instant::now() + Duration::from_secs(8)
            },
            &AtomicBool::new(false),
        );
        if matches!(scenario, "unsupported-control" | "authenticated-control" | "relative-endpoint") {
            assert_eq!(result.unwrap()["supported"], false);
        } else {
            assert!(result.is_err());
        }
        assert!(!dir.path().join("typed").exists(), "a native error must never send terminal keys");
        if matches!(scenario, "wrong-thread" | "authenticated-control" | "relative-endpoint" | "expired-control") {
            assert!(!dir.path().join("controls").exists());
        }
        svc.begin_shutdown(false);
    }
}
