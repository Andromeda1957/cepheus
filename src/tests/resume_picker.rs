use super::*;

pub(super) fn conversation(tool: Tool, id: &str, title: &str, created_at: u64, open_in: Option<u64>) -> Conversation {
    Conversation {
        search_words: Vec::new(),
        tool,
        id: id.into(),
        title: title.into(),
        created_at,
        updated_at: created_at,
        open_in,
        host: None,
    }
}

#[test]
fn resume_picker_filters_and_resumes() {
    let (mut app, rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    command(&mut app, "resume");
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "list_conversations"})]);
    app.handle_server(ServerMsg::Conversations {
        request: None,
        query: None,
        items: vec![
            conversation(Tool::Claude, "aaa", "Fix login", 10, Some(1)),
            conversation(Tool::Codex, "bbb", "Port parser", 20, None),
            conversation(Tool::Claude, "ccc", "Parser docs", 30, None),
        ],
    });
    for c in "/parser".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    if let Some(Dialog::Picker { items, filter, .. }) = &app.dialog {
        assert_eq!(App::picker_matches(items, filter), vec![1, 2]);
    } else {
        panic!("picker closed");
    }
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.dialog.is_none());
    assert_eq!(sent(&rx), vec![serde_json::json!({"t": "resume", "tool": "claude", "conversation": "ccc"})]);
}

#[test]
fn picking_an_open_conversation_selects_its_session() {
    let (mut app, rx) = app_with(
        vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Claude, Status::Idle, true)],
        vec![],
    );
    app.select(Sel::Session(2));
    command(&mut app, "resume");
    app.handle_server(ServerMsg::Conversations {
        request: None,
        query: None,
        items: vec![conversation(Tool::Claude, "aaa", "Fix login", 10, Some(1))],
    });
    let _ = sent(&rx);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Session(1)));
    assert!(sent(&rx).is_empty());
}

#[test]
fn the_resume_picker_also_filters_by_program() {
    let items = vec![
        conversation(Tool::Claude, "aaa", "Fix login", 10, None),
        conversation(Tool::Codex, "bbb", "Port parser", 20, None),
        conversation(Tool::Claude, "ccc", "Parser docs", 30, None),
        conversation(Tool::Antigravity, "ddd", "Try agy", 40, None),
        conversation(Tool::Grok, "01a16bf6", "Grok the parser", 50, None),
        conversation(Tool::OpenCode, "ses_ead42ac29ffei5Xcr4QiFwE9uu", "Compare", 60, None),
    ];
    assert_eq!(App::picker_matches(&items, "codex"), vec![1]);
    assert_eq!(App::picker_matches(&items, "claude parser"), vec![2]);
    assert_eq!(App::picker_matches(&items, "agy"), vec![3]);
    assert_eq!(App::picker_matches(&items, "bb"), vec![1], "an id prefix still finds it");
    assert_eq!(App::picker_matches(&items, ""), vec![0, 1, 2, 3, 4, 5]);
    assert_eq!(App::picker_matches(&items, "grok parser"), vec![4]);
    assert_eq!(App::picker_matches(&items, "open"), vec![5]);
    // An opencode id mixes cases; typing it in either case finds it.
    assert_eq!(App::picker_matches(&items, "ses_ead42ac29ffei5xcr4"), vec![5]);
    assert_eq!(App::picker_matches(&items, "SES_EAD42AC29FFEI5XCR4"), vec![5]);
}

#[test]
fn resume_picker_searches_instructions_and_keeps_the_latest_query() {
    let (mut app, rx) = app_with(vec![], vec![]);
    command(&mut app, "resume");
    let _ = sent(&rx);
    for c in "/roadmap codex worker".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    assert!(sent(&rx).is_empty(), "typing only schedules one query");
    app.poll_conversation_search(Instant::now() + Duration::from_secs(1));
    let requests = sent(&rx);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["query"], "roadmap codex worker");
    assert!(requests[0]["request"].is_u64());
    let mut found = conversation(Tool::Codex, "saved-chat", "Split brief into tasks", 1, None);
    found.host = Some("worker".into());
    found.search_words = vec!["roadmap".into()];
    app.handle_server(ServerMsg::Conversations {
        request: None,
        query: Some("roadmap".into()),
        items: vec![found.clone()],
    });
    assert!(
        matches!(&app.dialog, Some(Dialog::Picker { items, loading: true, .. }) if items.is_empty()),
        "late replies cannot replace the current search"
    );
    app.handle_server(ServerMsg::Conversations {
        request: None,
        query: Some("roadmap codex worker".into()),
        items: vec![found],
    });
    let Some(Dialog::Picker { items, filter, loading: false, .. }) = &app.dialog else { panic!("not loaded") };
    assert_eq!(App::picker_matches(items, filter), vec![0]);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
    term.draw(|f| {
        crate::view::render(f, &mut app);
    })
    .unwrap();
    let visible = term.backend().buffer().content().iter().map(|cell| cell.symbol()).collect::<String>();
    assert!(visible.contains("Split brief into tasks"), "the actual picker renders the instruction match");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        sent(&rx),
        vec![serde_json::json!({"t":"resume","tool":"codex","conversation":"saved-chat","host":"worker"})]
    );
}

#[test]
fn resume_picker_accepts_legacy_metadata_replies() {
    let (mut app, _rx) = app_with(vec![], vec![]);
    command(&mut app, "resume");
    for c in "/roadmap".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    app.handle_server(ServerMsg::Conversations {
        request: None,
        query: None,
        items: vec![conversation(Tool::Codex, "saved", "Roadmap", 1, None)],
    });
    let Some(Dialog::Picker { items, filter, loading: false, .. }) = &app.dialog else {
        panic!("legacy reply left loading")
    };
    assert_eq!(App::picker_matches(items, filter), vec![0]);
}

#[test]
fn repeated_search_text_rejects_an_older_request_generation() {
    let (mut app, rx) = app_with(vec![], vec![]);
    command(&mut app, "resume");
    let _ = sent(&rx);
    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    let mut first = serde_json::Value::Null;
    let mut previous = 0;
    for text in ["roadmap", "other", "roadmap"] {
        for _ in 0..previous {
            press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        }
        for c in text.chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.poll_conversation_search(Instant::now() + Duration::from_secs(1));
        let request = sent(&rx);
        if first.is_null() {
            first = request[0].get("request").cloned().unwrap_or(serde_json::json!(1));
        }
        previous = text.len();
    }
    let old = serde_json::from_value(serde_json::json!({"t":"conversations","query":"roadmap","request":first,"items":[{
        "tool":"codex","id":"old-chat","title":"Roadmap obsolete result","created_at":1,"updated_at":1,"open_in":null
    }]})).unwrap();
    app.handle_server(old);
    assert!(
        matches!(&app.dialog, Some(Dialog::Picker { items, loading: true, .. }) if items.is_empty()),
        "first A cannot replace the third search A"
    );
}
