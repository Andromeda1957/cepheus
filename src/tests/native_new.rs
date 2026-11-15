use super::*;
use crate::testsupport::TempDir;
use serde_json::{json, Value};

fn create(dir: &TempDir, request: u64) -> ClientMsg {
    ClientMsg::Create {
        tool: Tool::Codex,
        host: None,
        model: Some("gpt-6.1-sol".into()),
        effort: Some("xhigh".into()),
        cwd: Some(dir.path().to_string_lossy().into_owned()),
        command: vec![],
        name: None,
        request: Some(request),
    }
}

fn drain(replies: &Receiver<Out>) -> Vec<ServerMsg> {
    replies
        .try_iter()
        .filter_map(|out| match out {
            Out::Msg(bytes) => read_msg(&mut bytes.as_slice()).unwrap(),
            _ => None,
        })
        .collect()
}

#[test]
fn native_new_precreates_distinct_threads_before_first_clients_and_owner_text() {
    let (dir, mut service, events, replies, old, _server) = native_message_tests::fixture();
    let probe = dir.path().join("busy-probe");
    fs::create_dir_all(probe.join("pressure")).unwrap();
    fs::write(probe.join("meminfo"), "MemTotal: 16777216 kB\nMemAvailable: 1024 kB\n").unwrap();
    fs::write(probe.join("pressure/cpu"), "some avg60=100.0\n").unwrap();
    service.scheduler.settings.probe.root = Some(probe);
    service.handle_client(7, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
    service.close_and_remove(old);
    fs::remove_file(dir.path().join("launches")).unwrap();
    let endpoint = dir.path().join("codex-control.sock");
    std::os::unix::fs::symlink("codex.sock", &endpoint).unwrap();
    *service.opts.config.codex_command.iter_mut().find(|arg| arg.starts_with("unix://")).unwrap() =
        format!("unix://{}", endpoint.display());
    dir.write("pause-new", "");
    let mut named = create(&dir, 71);
    if let ClientMsg::Create { name, .. } = &mut named {
        *name = Some("Owner named New".into());
    }
    service.handle_client(7, named);
    for request in 72..81 {
        service.handle_client(7, create(&dir, request));
    }
    native_message_tests::until(&mut service, &events, |service| {
        assert!(service.native_new_pending.len() + service.placed_pending.len() <= crate::protocol::MAX_PENDING_STARTS);
        for message in drain(&replies) {
            assert!(!matches!(&message, ServerMsg::Notice { error: true, .. }), "before native start: {message:?}");
        }
        dir.path().join("new-started").exists()
    });
    assert!(!dir.path().join("launches").exists(), "native creation must precede any TUI process");
    assert!(drain(&replies).iter().all(|message| !matches!(message, ServerMsg::Created { .. })));
    fs::remove_file(dir.path().join("pause-new")).unwrap();
    let mut created = Vec::new();
    native_message_tests::until(&mut service, &events, |service| {
        assert!(service.native_new_pending.len() + service.placed_pending.len() <= crate::protocol::MAX_PENDING_STARTS);
        for message in drain(&replies) {
            assert!(!matches!(&message, ServerMsg::Notice { error: true, .. }), "before Created: {message:?}");
            created.push(message);
        }
        created.iter().filter(|message| matches!(message, ServerMsg::Created { request: Some(71..=80), .. })).count()
            == 10
    });
    let ids: Vec<_> = created
        .iter()
        .filter_map(|message| match message {
            ServerMsg::Created { id, request: Some(71..=80) } => Some(*id),
            _ => None,
        })
        .collect();
    let threads: Vec<_> = ids.iter().map(|id| service.sessions[id].conversation.clone().unwrap()).collect();
    assert_eq!(
        threads.iter().collect::<std::collections::HashSet<_>>().len(),
        10,
        "same-cwd concurrent New must never infer a nearest thread"
    );
    let named: Vec<Value> = fs::read_to_string(dir.path().join("new-names"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(named.len(), 10, "every empty native thread must be saved before first attachment");
    for id in &ids {
        let session = &service.sessions[id];
        let conversation = session.conversation.as_deref().unwrap();
        let expected_name = session.name.as_deref().unwrap_or(Tool::Codex.label());
        let matching: Vec<_> = named.iter().filter(|value| value["threadId"] == conversation).collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0]["name"], expected_name);
        let prepared: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(format!("new-{conversation}"))).unwrap()).unwrap();
        assert_eq!(prepared["thread"]["name"], expected_name);
        let receipt = fs::read_to_string(prepared["thread"]["path"].as_str().unwrap()).unwrap();
        let saved: Vec<Value> = receipt.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(saved.len(), 2, "native settings must be durably saved without a synthetic turn");
        assert_eq!(saved[1]["payload"]["thread_settings"]["reasoning_effort"], "xhigh");
        assert_eq!(saved[1]["payload"]["thread_id"], conversation);
    }
    let calls: Vec<Value> = fs::read_to_string(dir.path().join("new-starts"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(calls.len(), 10);
    for call in calls {
        assert_eq!(call["cwd"], json!(dir.path().canonicalize().unwrap()));
        assert_eq!(call["model"], "gpt-6.1-sol");
        assert_eq!(call["config"]["model_reasoning_effort"], "xhigh");
        assert!(call.get("input").is_none() && call.get("prompt").is_none());
    }
    native_message_tests::until(&mut service, &events, |_| {
        fs::read_to_string(dir.path().join("launches")).unwrap_or_default().lines().count() == 10
    });
    let launches: Vec<Vec<String>> = fs::read_to_string(dir.path().join("launches"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for thread in &threads {
        assert_eq!(
            launches
                .iter()
                .filter(|args| args.windows(2).any(|pair| pair[0] == "resume" && &pair[1] == thread))
                .count(),
            1
        );
    }
    assert!(ids.iter().all(|id| !service.sessions[id].owner_draft));
    service.send_message(7, ids[0], 73, "first exact owner message", false);
    let mut sent = false;
    native_message_tests::until(&mut service, &events, |_| {
        for message in drain(&replies) {
            if let ServerMsg::MessageSent { request: 73, error, .. } = message {
                assert!(error.is_none(), "{error:?}");
                sent = true;
            }
        }
        sent
    });
    assert_eq!(fs::read_to_string(dir.path().join("inbox")).unwrap(), "\"first exact owner message\"\n");
    assert!(!dir.path().join("typed").exists());
    service.begin_shutdown(false);
}

#[test]
fn native_new_waits_for_the_pinned_client_to_finish_starting() {
    let (dir, mut service, _events, _replies, _old, _server) = native_message_tests::fixture();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_secs(15);
    let prepared = crate::providers::messages::new_thread::prepare(
        &service.opts.config,
        dir.path(),
        &service.opts.codex_dir,
        &StartModel { model: Some("gpt-6.1-sol".into()), effort: Some("xhigh".into()) },
        "Delayed attachment",
        deadline,
        &stop,
    )
    .unwrap();
    dir.write("pause-runtime", "");
    let mut argv = vec!["/usr/bin/python3".into(), "-c".into(),
        "import os,sys,time\nfrom pathlib import Path\nPath('runtime-waiting').touch()\nwhile Path('pause-runtime').exists(): time.sleep(0.01)\nos.execv(sys.argv[1], sys.argv[1:])".into()];
    argv.extend(service.opts.config.codex_command.clone());
    argv.extend([
        "resume".into(),
        prepared.conversation.clone(),
        "--cd".into(),
        dir.path().to_string_lossy().into_owned(),
    ]);
    let env = vec![("CODEX_HOME".into(), service.opts.codex_dir.to_string_lossy().into_owned())];
    let (events, _) = mpsc::channel();
    let hosted = Hosted::spawn(
        SpawnSpec {
            id: 999,
            argv: &argv,
            cwd: dir.path(),
            env_set: &env,
            env_remove: &[],
            cols: 80,
            rows: 24,
            scrollback: 100,
        },
        events,
    )
    .unwrap();
    let pid = hosted.pid.unwrap();
    let (done, result) = mpsc::channel();
    let worker = thread::spawn(move || {
        done.send(prepared.verify(pid, deadline, &stop)).unwrap();
    });
    while !dir.path().join("runtime-waiting").exists() {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let early = result.recv_timeout(Duration::from_millis(40));
    fs::remove_file(dir.path().join("pause-runtime")).unwrap();
    let verified = early.or_else(|_| result.recv_timeout(Duration::from_secs(15))).unwrap();
    hosted.signal(libc::SIGHUP);
    worker.join().unwrap();
    service.begin_shutdown(false);
    assert!(verified.is_ok(), "delayed attachment: {verified:?}");
}

#[test]
fn native_new_queue_drains_failed_preparations_and_abandoned_connections() {
    for disconnected in [false, true] {
        let (dir, mut service, events, replies, old, _server) = native_message_tests::fixture();
        service.handle_client(7, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
        service.close_and_remove(old);
        dir.write("pause-new", "");
        dir.write("wrong-new-cwd", "");
        for request in 1..=12 {
            service.handle_client(7, create(&dir, request));
        }
        assert_eq!(service.native_new_queue.len(), 4);
        if disconnected {
            service.handle(Ev::ClientGone(7));
        }
        fs::remove_file(dir.path().join("pause-new")).unwrap();
        native_message_tests::until(&mut service, &events, |s| {
            s.native_new_queue.is_empty() && s.native_new_pending.is_empty()
        });
        let messages = drain(&replies);
        assert!(!messages.iter().any(|m| matches!(m, ServerMsg::Created { .. })));
        if !disconnected {
            assert_eq!(messages.iter().filter(|m| matches!(m, ServerMsg::Notice { error: true, .. })).count(), 12);
        }
        service.begin_shutdown(false);
    }
}

#[test]
fn native_new_refuses_bad_metadata_or_changed_launch_context_without_spawning() {
    for scenario in [
        "wrong-new-cwd",
        "changed-launch",
        "unsupported-profile",
        "endpoint-file",
        "foreign-endpoint",
        "rejected-new-name",
        "bad-settings-receipt",
        "stale-settings-prefix",
    ] {
        let (dir, mut service, events, replies, old, _server) = native_message_tests::fixture();
        service.handle_client(7, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
        service.close_and_remove(old);
        fs::remove_file(dir.path().join("launches")).unwrap();
        if matches!(scenario, "wrong-new-cwd" | "rejected-new-name" | "bad-settings-receipt" | "stale-settings-prefix")
        {
            dir.write(scenario, "");
        }
        if scenario == "endpoint-file" {
            dir.write("not-socket", "");
            let endpoint = dir.path().join("codex-control.sock");
            std::os::unix::fs::symlink("not-socket", &endpoint).unwrap();
            *service.opts.config.codex_command.iter_mut().find(|arg| arg.starts_with("unix://")).unwrap() =
                format!("unix://{}", endpoint.display());
        }
        if scenario == "foreign-endpoint" {
            use std::os::unix::fs::MetadataExt;
            let endpoint = Path::new("/dev/stdout");
            assert_ne!(fs::symlink_metadata(endpoint).unwrap().uid(), unsafe { libc::geteuid() });
            *service.opts.config.codex_command.iter_mut().find(|arg| arg.starts_with("unix://")).unwrap() =
                format!("unix://{}", endpoint.display());
        }
        if scenario == "changed-launch" {
            dir.write("pause-new", "");
        }
        if scenario == "unsupported-profile" {
            service.opts.config.codex_command.extend(["--profile".into(), "private".into()]);
        }
        service.handle_client(7, create(&dir, 81));
        if scenario == "changed-launch" {
            native_message_tests::until(&mut service, &events, |_| dir.path().join("new-started").exists());
            service.opts.config.codex_command.push("--no-alt-screen".into());
            fs::remove_file(dir.path().join("pause-new")).unwrap();
        }
        let mut refused = false;
        native_message_tests::until(&mut service, &events, |_| {
            for message in drain(&replies) {
                assert!(!matches!(message, ServerMsg::Created { request: Some(81), .. }));
                refused |= matches!(message, ServerMsg::Notice { error: true, .. });
            }
            refused
        });
        assert!(!dir.path().join("launches").exists());
        assert!(!dir.path().join("typed").exists());
        service.begin_shutdown(false);
    }
}
