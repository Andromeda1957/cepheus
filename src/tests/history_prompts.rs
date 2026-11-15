use super::*;
use crate::testsupport::TempDir;
use serde_json::json;

#[test]
fn submitted_instructions_match_across_native_histories_and_pastes() {
    let dir = TempDir::new("prompt-histories");
    let mut items = Vec::new();
    let mut roots = Vec::new();
    for (n, tool) in [Tool::Codex, Tool::Claude, Tool::Antigravity, Tool::Grok].into_iter().enumerate() {
        let root = dir.path().join(n.to_string());
        fs::create_dir(&root).unwrap();
        roots.push((tool, root.clone()));
        items.push(Conversation {
            search_words: vec![],
            tool,
            id: "saved-chat".into(),
            title: "Unrelated title".into(),
            created_at: 0,
            updated_at: 0,
            open_in: None,
            host: None,
        });
        let record = match tool {
            Tool::Codex => json!({"session_id":"saved-chat","text":format!("{} roadmap", "x".repeat(30_000))}),
            Tool::Claude => {
                fs::create_dir(root.join("paste-cache")).unwrap();
                fs::write(root.join("paste-cache/0123456789abcdef.txt"), "make a roadmap").unwrap();
                json!({"sessionId":"saved-chat","display":"[Pasted text]", "pastedContents":{"1":{"type":"text","contentHash":"0123456789abcdef"}}})
            }
            Tool::Antigravity => json!({"conversationId":"saved-chat","display":"make a ROADMAP","workspace":"/work"}),
            Tool::Grok => json!({"session_id":"saved-chat","prompt":"make a roadmap","is_bash":false}),
            _ => unreachable!(),
        };
        let filename = if tool == Tool::Grok { "prompt_history.jsonl" } else { "history.jsonl" };
        let ignored = json!({"session_id":"other-chat","text":"private unmatched"});
        fs::write(root.join(filename), format!("{record}\n{ignored}\n")).unwrap();
    }
    prompt_matches(&mut items, &roots, &HashMap::new(), &["roadmap".into(), "private".into()], &AtomicBool::new(false));
    for item in items {
        assert_eq!(item.search_words, ["roadmap"], "{:?}", item.tool);
    }
}

#[test]
fn prompt_cache_paths_and_account_roots_do_not_cross() {
    let dir = TempDir::new("prompt-boundaries");
    let root = dir.path().join("claude");
    fs::create_dir(&root).unwrap();
    let foreign = dir.path().join("other");
    fs::create_dir(&foreign).unwrap();
    fs::write(dir.path().join("outside.txt"), "roadmap").unwrap();
    fs::create_dir(root.join("paste-cache")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("outside.txt"), root.join("paste-cache/0123456789abcdef.txt")).unwrap();
    let record = json!({"sessionId":"saved-chat","display":"[Pasted text]","pastedContents":{
        "1":{"type":"text","contentHash":"../outside"},
        "2":{"type":"text","contentHash":"0123456789abcdef"}
    }});
    fs::write(root.join("history.jsonl"), format!("{record}\n")).unwrap();
    fs::write(foreign.join("history.jsonl"), format!("{}\n", json!({"sessionId":"saved-chat","display":"roadmap"})))
        .unwrap();
    let mut items = vec![Conversation {
        search_words: vec![],
        tool: Tool::Claude,
        id: "saved-chat".into(),
        title: "Unrelated".into(),
        created_at: 0,
        updated_at: 0,
        open_in: None,
        host: None,
    }];
    let profiles = HashMap::from([((Tool::Claude, "saved-chat".into()), root.clone())]);
    prompt_matches(
        &mut items,
        &[(Tool::Claude, root), (Tool::Claude, foreign)],
        &profiles,
        &["roadmap".into()],
        &AtomicBool::new(false),
    );
    assert!(items[0].search_words.is_empty());
}

fn next_list(svc: &mut Service, events: &mpsc::Receiver<Ev>) -> (String, Vec<Conversation>) {
    loop {
        match events.recv_timeout(Duration::from_secs(30)).unwrap() {
            Ev::Conversations(cid, query, request, items) => {
                let found = (query.clone(), items.clone());
                svc.handle(Ev::Conversations(cid, query, request, items));
                break found;
            }
            event => svc.handle(event),
        }
    }
}

#[test]
fn a_request_during_a_scan_waits_and_searches_reuse_one_transcript_listing() {
    let dir = TempDir::new("resume-scan-guard");
    let opts = super::super::tests::options(&dir, "true");
    let project = opts.claude_dir.join("projects").join(claude::project_slug(&dir.path().to_string_lossy()));
    fs::create_dir_all(&project).unwrap();
    let id = "22222222-2222-4222-8222-000000000000";
    let transcript = project.join(format!("{id}.jsonl"));
    let first = json!({"type":"user","message":{"role":"user","content":"Plan the roadmap"}});
    fs::write(&transcript, format!("{first}\n")).unwrap();
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let busy = Arc::new(AtomicBool::new(false));
    svc.conversation_scans.running.insert(0, busy.clone());
    svc.list_conversations(0, "first".into(), Some(1));
    svc.list_conversations(0, "roadmap".into(), Some(2));
    assert_eq!(Arc::strong_count(&busy), 2, "no second scan starts while one runs");
    assert_eq!(svc.conversation_scans.waiting.get(&0), Some(&("roadmap".to_string(), Some(2))));
    svc.conversations_scanned(0);
    let (query, items) = next_list(&mut svc, &events);
    assert_eq!(query, "roadmap", "only the latest waiting request runs");
    assert!(items.iter().any(|c| c.id == id));
    fs::remove_file(&transcript).unwrap();
    svc.list_conversations(0, "plan".into(), Some(3));
    let (_, items) = next_list(&mut svc, &events);
    assert!(items.iter().any(|c| c.id == id), "a later query reuses the listing instead of rereading transcripts");
    svc.list_conversations(0, String::new(), None);
    let (_, items) = next_list(&mut svc, &events);
    assert!(!items.iter().any(|c| c.id == id));
    svc.list_conversations(0, "plan".into(), Some(4));
    let (_, items) = next_list(&mut svc, &events);
    assert!(!items.iter().any(|c| c.id == id), "a plain listing makes the next search read transcripts afresh");
}
