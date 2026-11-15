use super::*;
use crate::protocol::{encode, ScrollOp, Status, Tool};

fn tags(name: &str) -> Option<u16> {
    match name {
        "fedora" => Some(1),
        "controller" => Some(2),
        _ => None,
    }
}

#[test]
fn a_message_naming_a_remote_session_goes_to_its_host_with_the_hosts_id() {
    let id = host_id(2, 17);
    assert_eq!(split_id(id), (2, 17));
    assert_eq!(
        route(ClientMsg::Input { id, data: "hi".into() }, tags),
        Route::Host(2, ClientMsg::Input { id: 17, data: "hi".into() })
    );
    assert_eq!(route(ClientMsg::Stop { id }, tags), Route::Host(2, ClientMsg::Stop { id: 17 }));
    assert_eq!(
        route(ClientMsg::Scroll { id, op: ScrollOp::Top }, tags),
        Route::Host(2, ClientMsg::Scroll { id: 17, op: ScrollOp::Top })
    );
    assert_eq!(
        route(ClientMsg::Attach { id: Some(id), cols: 80, rows: 24 }, tags),
        Route::Host(2, ClientMsg::Attach { id: Some(17), cols: 80, rows: 24 })
    );
    // An image from the deck's clipboard is kept on the session's host.
    let paste = |id| ClientMsg::PasteFile { id, name: "clipboard-1.png".into(), data: "iVBO".into() };
    assert_eq!(route(paste(id), tags), Route::Host(2, paste(17)));
    assert_eq!(route(paste(5), tags), Route::Local(paste(5)));
    let cd = |id| ClientMsg::ChangeDirectory { id, path: "../other folder".into() };
    assert_eq!(route(cd(id), tags), Route::Host(2, cd(17)));
    assert_eq!(route(cd(5), tags), Route::Local(cd(5)));
    let history = |id| ClientMsg::History { id, request: 7, epoch: 33, cols: 80, rows: 24 };
    assert_eq!(route(history(id), tags), Route::Host(2, history(17)));
    assert_eq!(route(history(host_id(1, 17)), tags), Route::Host(1, history(17)));
    let reply = |id| ServerMsg::History {
        id,
        request: 7,
        epoch: 33,
        cols: 80,
        rows: 24,
        snapshot: None,
        error: Some("retired".into()),
    };
    assert!(matches!(inbound(2, "controller", "Controller", reply(17)),
        ServerMsg::History { id: got, request: 7, epoch: 33, .. } if got == id));
}

#[test]
fn the_hubs_own_sessions_stay_here_unchanged() {
    for msg in [
        ClientMsg::Input { id: 5, data: "x".into() },
        ClientMsg::Close { id: 5 },
        ClientMsg::Attach { id: None, cols: 80, rows: 24 },
        ClientMsg::Create {
            tool: Tool::Codex,
            model: None,
            effort: None,
            host: None,
            cwd: None,
            command: Vec::new(),
            name: None,
            request: None,
        },
        ClientMsg::MoveIn { tool: Tool::Claude, pid: 4000, conversation: None },
    ] {
        let text = format!("{msg:?}");
        assert!(matches!(route(msg, tags), Route::Local(_)), "{text}");
    }
}

#[test]
fn starts_go_to_the_named_host_and_an_unknown_host_is_refused() {
    let create = ClientMsg::Create {
        tool: Tool::Claude,
        model: Some("opus".into()),
        effort: None,
        host: None,
        cwd: None,
        command: Vec::new(),
        name: None,
        request: Some(host_id(2, 17)),
    };
    let named = ClientMsg::Create {
        tool: Tool::Claude,
        model: Some("opus".into()),
        effort: None,
        host: Some("fedora".into()),
        cwd: None,
        command: Vec::new(),
        name: None,
        request: Some(host_id(2, 17)),
    };
    assert_eq!(route(named, tags), Route::Host(1, create));
    let resume = ClientMsg::Resume { tool: Tool::Codex, conversation: "c1".into(), host: Some("controller".into()) };
    assert_eq!(
        route(resume, tags),
        Route::Host(2, ClientMsg::Resume { tool: Tool::Codex, conversation: "c1".into(), host: None })
    );
    let stray = ClientMsg::Create {
        tool: Tool::Claude,
        model: None,
        effort: None,
        host: Some("nowhere".into()),
        cwd: None,
        command: Vec::new(),
        name: None,
        request: None,
    };
    assert_eq!(route(stray, tags), Route::UnknownHost("nowhere".into()));
}

#[test]
fn an_outside_process_on_a_host_moves_in_there() {
    let pid = host_pid(1, 4_000_000);
    assert_eq!(split_pid(pid), (1, 4_000_000));
    assert_eq!(
        route(ClientMsg::MoveIn { tool: Tool::Claude, pid, conversation: None }, tags),
        Route::Host(1, ClientMsg::MoveIn { tool: Tool::Claude, pid: 4_000_000, conversation: None })
    );
}

#[test]
fn a_broadcast_splits_by_host_hub_first() {
    let ids = [host_id(2, 3), 7, host_id(1, 9), 8, host_id(2, 4)];
    assert_eq!(split_ids(&ids), vec![(0, vec![7, 8]), (1, vec![9]), (2, vec![3, 4])]);
}

fn session(id: u64) -> SessionInfo {
    crate::tui::tests::session(id, Tool::Claude, Status::Working, true)
}

#[test]
fn a_hosts_state_and_replies_carry_the_hubs_ids_and_the_host() {
    let mut outside = crate::tui::tests::outside(4242, None);
    outside.kitty_pid = Some(9);
    let state = DeckState { sessions: vec![session(3)], outside: vec![outside], ..DeckState::default() };
    let ServerMsg::State(state) = inbound(1, "fedora", "Fedora", ServerMsg::State(state)) else { panic!() };
    assert_eq!(state.sessions[0].id, host_id(1, 3));
    assert_eq!(state.sessions[0].host.as_deref(), Some("fedora"));
    assert_eq!(state.outside[0].pid, host_pid(1, 4242));
    assert_eq!(state.outside[0].kitty_pid, None, "the hub cannot focus a host's kitty window");
    assert!(
        matches!(inbound(1, "fedora", "Fedora", ServerMsg::Created { id: 3, request: Some(host_id(2, 17)) }), ServerMsg::Created { id, request } if id == host_id(1, 3) && request == Some(host_id(2, 17)))
    );
    let ServerMsg::Notice { text, .. } =
        inbound(1, "fedora", "Fedora", ServerMsg::Notice { text: "Could not start".into(), error: true })
    else {
        panic!()
    };
    assert_eq!(text, "Fedora: Could not start");
    let conv = Conversation {
        search_words: Vec::new(),
        tool: Tool::Codex,
        id: "c".into(),
        title: "t".into(),
        created_at: 0,
        updated_at: 0,
        open_in: Some(3),
        host: None,
    };
    let ServerMsg::Conversations { items, .. } = inbound(
        2,
        "controller",
        "Controller",
        ServerMsg::Conversations { request: None, query: None, items: vec![conv] },
    ) else {
        panic!()
    };
    assert_eq!(items[0].open_in, Some(host_id(2, 3)));
    assert_eq!(items[0].host.as_deref(), Some("controller"));
}

#[test]
fn a_frame_is_renumbered_without_being_parsed() {
    let frame = crate::screen::ScreenFrame::default();
    let encoded = encode(&ServerMsg::Frame { id: 12, frame: frame.clone() });
    let rewritten = rewrite_frame(&encoded[4..], 3).expect("a frame as serde writes it");
    let len = u32::from_be_bytes(rewritten[..4].try_into().unwrap()) as usize;
    assert_eq!(len, rewritten.len() - 4, "the length prefix covers the new body");
    let msg: ServerMsg = serde_json::from_slice(&rewritten[4..]).unwrap();
    match msg {
        ServerMsg::Frame { id, frame: back } => {
            assert_eq!(id, host_id(3, 12));
            assert_eq!(back, frame);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(kind_of(&encoded[4..]), Some("frame"));
    assert!(rewrite_frame(&encode(&ServerMsg::Bell { id: 1 })[4..], 3).is_none());
}

#[test]
fn usage_keeps_the_newest_reading_of_each_window() {
    let window = |label: &str, used: f64, at: u64| UsageWindow {
        label: label.into(),
        used_percent: Some(used),
        resets_at: None,
        updated_at: at,
    };
    let mut hub = Usage { claude: vec![window("5h", 10.0, 100)], codex: vec![], ..Usage::default() };
    let host = Usage {
        claude: vec![window("5h", 30.0, 200), window("week", 5.0, 50)],
        codex: vec![window("5h", 1.0, 9)],
        ..Usage::default()
    };
    merge_usage(&mut hub, &host);
    assert_eq!(hub.claude.iter().find(|w| w.label == "5h").unwrap().used_percent.unwrap(), 30.0);
    assert_eq!(hub.claude.len(), 2);
    assert_eq!(hub.codex.len(), 1);
    let older = Usage { claude: vec![window("5h", 99.0, 150)], codex: vec![], ..Usage::default() };
    merge_usage(&mut hub, &older);
    assert_eq!(
        hub.claude.iter().find(|w| w.label == "5h").unwrap().used_percent.unwrap(),
        30.0,
        "an older reading loses"
    );

    // New providers cross the same host boundary, including their reset times.
    let providers: Usage = serde_json::from_value(serde_json::json!({
        "claude": [], "codex": [],
        "grok": [{"label":"weekly", "used_percent":25.0, "resets_at":900, "updated_at":200}],
        "gemini": [{"label":"Gemini Models 5h", "used_percent":10.0, "resets_at":800, "updated_at":200}]
    }))
    .unwrap();
    merge_usage(&mut hub, &providers);
    let exported = serde_json::to_value(&hub).unwrap();
    assert_eq!(exported["grok"][0]["resets_at"], 900);
    assert_eq!(exported["gemini"][0]["used_percent"], 10.0);
    assert_eq!(exported["gemini"][0]["resets_at"], 800);
}

#[test]
fn errors_from_every_host_merge_oldest_first_named_and_capped() {
    let e = |at: u64, text: &str| LogEntry::new(at, text);
    let merged = merge_errors(
        vec![e(1, "hub a"), e(5, "hub b")],
        &[("Fedora".into(), vec![e(3, "lost")]), ("Controller".into(), vec![e(9, "late")])],
        3,
    );
    let texts: Vec<&str> = merged.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(texts, ["Fedora: lost", "hub b", "Controller: late"]);
}
