use super::*;
use crate::testsupport::TempDir;

fn settle(svc: &mut Service, events: &Receiver<Ev>, id: u64, cwd: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = &svc.sessions[&id];
        let text = s.hosted.as_ref().map(|h| screen::screen_text(&h.lock().term).join("\n")).unwrap_or_default();
        if s.alive && text.lines().any(|line| line.trim_end() == format!("READY {}", cwd.display())) {
            return;
        }
        assert!(Instant::now() < deadline, "agent never arrived at {}: {text}", cwd.display());
        if let Ok(event) = events.recv_timeout(Duration::from_millis(20)) {
            svc.handle(event);
        }
    }
}

fn request(svc: &mut Service, id: u64, path: &str) {
    let msg = serde_json::from_value(serde_json::json!({"t":"change_directory", "id":id, "path":path})).unwrap();
    svc.handle_client(0, msg);
}

#[test]
fn directory_change_validates_before_stopping_and_starts_fresh_in_the_same_slot() {
    let dir = TempDir::new("directory-change");
    let target = dir.path().join("a folder; literal $(text)");
    fs::create_dir(&target).unwrap();
    dir.write("a-file", "file");
    let agent = dir.write(
        "agent.py",
        "import os,sys,json\nprint('\x1b[?2004hREADY '+os.getcwd(), flush=True)\nprint(json.dumps(sys.argv[1:]), flush=True)\nfor line in sys.stdin: print(line.strip(), flush=True)\n",
    );
    let mut opts = super::tests::options(&dir, "true");
    opts.config.codex_command = vec!["python3".into(), agent.to_string_lossy().into_owned()];
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let id = svc.create(Tool::Codex, None).unwrap();
    settle(&mut svc, &events, id, dir.path());
    svc.sessions.get_mut(&id).unwrap().conversation = Some("saved-conversation".into());
    let pid = svc.sessions[&id].hosted.as_ref().unwrap().pid;
    for invalid in ["missing", "a-file", "", "\0"] {
        request(&mut svc, id, invalid);
        assert_eq!(svc.sessions[&id].hosted.as_ref().unwrap().pid, pid);
        assert!(svc.sessions[&id].closing.is_none(), "invalid destination must leave the agent running");
    }
    request(&mut svc, id, "a folder; literal $(text)");
    svc.broadcast_message(0, &[id], "input for the previous agent");
    assert!(svc.pending_submits.is_empty(), "a stopping agent must not queue input for its replacement");
    settle(&mut svc, &events, id, &target);
    let s = &svc.sessions[&id];
    assert_ne!(s.hosted.as_ref().unwrap().pid, pid);
    assert!(s.conversation.is_none(), "a fresh Codex chat gets its id after its first message");
    let saved = Registry::load(&svc.registry_path);
    assert_eq!(saved.sessions.iter().find(|e| e.id == id).unwrap().cwd.as_deref(), target.to_str());
    svc.sessions.get_mut(&id).unwrap().conversation = Some("second-conversation".into());
    request(&mut svc, id, "..");
    settle(&mut svc, &events, id, dir.path());
    svc.apply_report(Report {
        facts: [(
            id,
            SessionFacts {
                known_pid: pid,
                known_conversation: None,
                conversation: Some("stale-conversation".into()),
                ..SessionFacts::default()
            },
        )]
        .into_iter()
        .collect(),
        ..Report::default()
    });
    assert!(svc.sessions[&id].conversation.is_none(), "a previous process's snapshot must not take over the new chat");
    assert_eq!(svc.sessions.len(), 1, "directory changes must keep the same deck entry");
    svc.list_conversations(0, String::new(), None);
    let history = loop {
        match events.recv_timeout(Duration::from_secs(10)).unwrap() {
            Ev::Conversations(_, _, _, items) => break items,
            event => svc.handle(event),
        }
    };
    assert!(history.iter().any(|c| c.id == "second-conversation"), "a departed folder's chat must stay in Resume");
    let resumed = svc.resume(Tool::Codex, "second-conversation".into()).unwrap();
    settle(&mut svc, &events, resumed, &target);
    assert_eq!(svc.sessions[&resumed].cwd.as_deref(), Some(target.as_path()));
    for closing in [id, resumed] {
        svc.close(closing);
    }
    while svc.sessions.values().any(|s| s.alive) {
        svc.handle(events.recv_timeout(Duration::from_secs(10)).unwrap());
    }
}

#[test]
fn a_failed_directory_change_recovers_and_an_explicit_close_cancels_the_restart() {
    for close in [false, true] {
        let dir = TempDir::new("directory-cancel");
        let target = dir.path().join("destination");
        fs::create_dir(&target).unwrap();
        let opts = super::tests::options(&dir, "printf 'READY '; pwd; read answer");
        let (tx, events) = mpsc::channel();
        let mut svc = Service::new(opts, tx);
        let id = svc.create(Tool::Claude, None).unwrap();
        settle(&mut svc, &events, id, dir.path());
        request(&mut svc, id, "destination");
        if close {
            svc.close_and_remove(id);
            let deadline = Instant::now() + Duration::from_secs(10);
            while svc.sessions.contains_key(&id) {
                assert!(Instant::now() < deadline, "closed agent was not removed");
                if let Ok(event) = events.recv_timeout(Duration::from_millis(20)) {
                    svc.handle(event);
                }
            }
        } else {
            // Only this fixture's empty directory disappears, after validation
            // but before the service handles its old process's exit.
            fs::remove_dir(&target).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !svc.pending_directories.is_empty() {
                assert!(Instant::now() < deadline, "directory recovery never finished");
                if let Ok(event) = events.recv_timeout(Duration::from_millis(20)) {
                    svc.handle(event);
                }
            }
            settle(&mut svc, &events, id, dir.path());
            svc.close(id);
            while svc.sessions[&id].alive {
                svc.handle(events.recv_timeout(Duration::from_secs(10)).unwrap());
            }
        }
        assert!(svc.pending_directories.is_empty());
    }
}
