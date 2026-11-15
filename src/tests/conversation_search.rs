use super::*;
use crate::testsupport::TempDir;
use serde_json::json;

#[test]
fn resume_search_finds_a_message_in_an_older_saved_chat() {
    let dir = TempDir::new("resume-search");
    let opts = super::tests::options(&dir, "true");
    let project = opts.claude_dir.join("projects").join(claude::project_slug(&dir.path().to_string_lossy()));
    fs::create_dir_all(&project).unwrap();
    for n in 0..43 {
        let id = format!("11111111-1111-4111-8111-{n:012}");
        let path = project.join(format!("{id}.jsonl"));
        let first = json!({"type":"user","message":{"role":"user","content":"Split brief into tasks"}});
        let message = if n == 0 { "make a roadmap like we did for sample" } else { "ordinary work" };
        let later = json!({"type":"user","message":{"role":"user","content":message}});
        fs::write(&path, format!("{first}\n{later}\n")).unwrap();
        fs::File::open(&path).unwrap().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(100 + n)).unwrap();
    }
    fs::write(
        opts.claude_dir.join("history.jsonl"),
        format!(
            "{}\n",
            json!({
                "sessionId":"11111111-1111-4111-8111-000000000000", "display":"make a roadmap like we did for sample"
            })
        ),
    )
    .unwrap();
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    let request = serde_json::from_value(json!({"t":"list_conversations","query":"roadmap"})).unwrap();
    svc.handle_client(0, request);
    let items = loop {
        match events.recv_timeout(Duration::from_secs(30)).unwrap() {
            Ev::Conversations(_, _, _, items) => break items,
            event => svc.handle(event),
        }
    };
    let old = items
        .iter()
        .find(|c| c.id == "11111111-1111-4111-8111-000000000000")
        .expect("search must include saved chats outside the recent forty");
    let wire = serde_json::to_value(old).unwrap();
    assert_eq!(wire["search_words"], json!(["roadmap"]), "match comes from a later owner message, not its title");
}

#[test]
fn a_slow_local_history_scan_is_not_finished_by_the_remote_host_deadline() {
    let dir = TempDir::new("local-history-deadline");
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(super::tests::options(&dir, "true"), tx);
    let (out, rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 1, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(1, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
    while rx.try_recv().is_ok() {}
    assert!(matches!(
        svc.route_client(1, ClientMsg::ListConversations { request: None, query: String::new() }),
        Some(ClientMsg::ListConversations { .. })
    ));
    svc.lists_due(Instant::now() + Duration::from_secs(5));
    assert!(rx.try_recv().is_err(), "the local history scan still owns its reply");
    svc.conversations_part(
        1,
        0,
        None,
        None,
        vec![Conversation {
            search_words: Vec::new(),
            tool: Tool::Grok,
            id: "local-conversation".into(),
            title: "Local work".into(),
            created_at: 1,
            updated_at: 2,
            open_in: None,
            host: None,
        }],
    );
    let Out::Msg(reply) = rx.try_recv().unwrap() else { panic!("expected a conversation list") };
    let reply = crate::protocol::read_msg::<_, ServerMsg>(&mut reply.as_slice()).unwrap().unwrap();
    assert!(
        matches!(reply, ServerMsg::Conversations { items, .. } if items.len() == 1 && items[0].id == "local-conversation")
    );
    assert!(rx.try_recv().is_err(), "one complete list is delivered");
}

#[test]
fn a_late_conversation_part_does_not_finish_a_newer_query() {
    let dir = TempDir::new("search-query-fence");
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(super::tests::options(&dir, "true"), tx);
    let (out, rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 1, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(1, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
    while rx.try_recv().is_ok() {}
    for (query, request) in [("roadmap", 1), ("other", 2), ("roadmap", 3)] {
        svc.route_client(1, ClientMsg::ListConversations { query: query.into(), request: Some(request) });
    }
    svc.conversations_part(1, 0, Some("roadmap".into()), Some(1), vec![]);
    assert!(rx.try_recv().is_err());
    svc.conversations_part(1, 0, Some("roadmap".into()), Some(3), vec![]);
    let Out::Msg(reply) = rx.try_recv().unwrap() else { panic!("missing reply") };
    let reply = crate::protocol::read_msg::<_, ServerMsg>(&mut reply.as_slice()).unwrap().unwrap();
    assert!(
        matches!(reply, ServerMsg::Conversations { query: Some(query), request: Some(3), .. } if query == "roadmap")
    );
}
