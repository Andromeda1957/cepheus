use super::*;
use std::io::Cursor;

#[test]
fn messages_round_trip_through_framing() {
    let msgs = vec![
        ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Tui },
        ClientMsg::Input { id: 3, data: "\x1b[13;2u".into() },
        ClientMsg::SetOrder { ids: vec![3, 1] },
        ClientMsg::Scroll { id: 3, op: ScrollOp::Lines(-5) },
        ClientMsg::MoveIn { tool: Tool::Codex, pid: 42, conversation: None },
        ClientMsg::Shutdown,
    ];
    let mut buf = Vec::new();
    for m in &msgs {
        write_msg(&mut buf, m).unwrap();
    }
    let mut r = Cursor::new(buf);
    for want in &msgs {
        let got: ClientMsg = read_msg(&mut r).unwrap().unwrap();
        assert_eq!(serde_json::to_value(&got).unwrap(), serde_json::to_value(want).unwrap());
    }
    assert!(read_msg::<_, ClientMsg>(&mut r).unwrap().is_none(), "clean end of stream");
}

#[test]
fn wire_format_is_tagged_snake_case() {
    let v = serde_json::to_value(ClientMsg::ListConversations { request: None, query: String::new() }).unwrap();
    assert_eq!(v, serde_json::json!({"t": "list_conversations"}));
    let v = serde_json::to_value(ClientMsg::Create {
        tool: Tool::Claude,
        model: None,
        effort: None,
        host: None,
        cwd: None,
        command: Vec::new(),
        name: None,
        request: None,
    })
    .unwrap();
    assert_eq!(v, serde_json::json!({"t": "create", "tool": "claude"}));
    let encoded = encode(&ServerMsg::Bell { id: 9 });
    let len = u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize;
    assert_eq!(len, encoded.len() - 4);
}

#[test]
fn a_state_from_an_older_service_reads_without_the_newer_fields() {
    // A deck updated before its service is restarted still gets the old
    // service's state: it must not drop the connection over the missing fields.
    let mut old =
        serde_json::to_value(DeckState { start_dir: "/srv".into(), session_order: vec![3, 1], ..DeckState::default() })
            .unwrap();
    let fields = old.as_object_mut().unwrap();
    assert!(fields.remove("dev_server").is_some() && fields.remove("errors").is_some());
    fields.remove("session_order");
    let state: DeckState = serde_json::from_value(old).unwrap();
    assert_eq!(state.start_dir, "/srv");
    assert_eq!(state.dev_server, DevServer::default());
    assert!(state.errors.is_empty());
    assert!(state.session_order.is_empty());
    let v = serde_json::to_value(ClientMsg::DevServer { run: true }).unwrap();
    assert_eq!(v, serde_json::json!({"t": "dev_server", "run": true}));
    assert_eq!(serde_json::to_value(Tool::Antigravity).unwrap(), serde_json::json!("antigravity"));
    assert!(Tool::Antigravity.since_version() > 1, "an older service cannot read it");
    // `ctl` and the wire name them as the owner types them.
    assert_eq!(serde_json::to_value(Tool::OpenCode).unwrap(), serde_json::json!("opencode"));
    assert_eq!(serde_json::from_value::<Tool>(serde_json::json!("grok")).unwrap(), Tool::Grok);
    assert_eq!((Tool::Grok.since_version(), Tool::OpenCode.since_version()), (4, 4));
}

#[test]
fn oversized_length_is_rejected() {
    let mut buf = ((MAX_MESSAGE + 1) as u32).to_be_bytes().to_vec();
    buf.extend_from_slice(b"{}");
    let err = read_msg::<_, ServerMsg>(&mut Cursor::new(buf)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn truncated_body_is_an_error_not_eof() {
    let mut buf = 10u32.to_be_bytes().to_vec();
    buf.extend_from_slice(b"{\"t\"");
    assert!(read_msg::<_, ServerMsg>(&mut Cursor::new(buf)).is_err());
}

#[test]
fn invalid_json_is_invalid_data() {
    let mut buf = 3u32.to_be_bytes().to_vec();
    buf.extend_from_slice(b"nop");
    let err = read_msg::<_, ServerMsg>(&mut Cursor::new(buf)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn attention_snapshots_keep_the_empty_inbox_and_stalls_older_phones_require_and_read_older_peers() {
    let json = serde_json::to_value(crate::attention::Snapshot::default()).unwrap();
    assert_eq!(json["inbox"], serde_json::json!([]));
    assert_eq!(json["stalls"], serde_json::json!([]));
    assert!(json.get("away").is_none());
    let older = serde_json::json!({
        "inbox": [{"id": "entry", "session": 2, "job": null, "reason": "stall", "blocked_since": 10}],
        "stalls": [{"session": 2, "kind": "files", "since": 11}],
        "queued": {"2": 1},
        "messages": [],
        "away": {"id": "away", "created_at": 1, "counts": {}, "model_state": "off"},
        "supervisor": {"enabled": false, "revision": 1, "rules": 0, "pending": []}
    });
    let mut outer = serde_json::to_value(ServerMsg::State(DeckState::default())).unwrap();
    outer["attention"] = older;
    let body = serde_json::to_vec(&outer).unwrap();
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend(body);
    let Some(ServerMsg::State(state)) = read_msg::<_, ServerMsg>(&mut Cursor::new(frame)).unwrap() else {
        panic!("state frame expected")
    };
    assert_eq!(state.attention.queued.get(&2), Some(&1));
    assert_eq!(serde_json::to_value(&state.attention).unwrap()["inbox"], serde_json::json!([]));
}

#[test]
fn attention_session_counts_survive_outer_state_and_fleet_frames_losslessly() {
    let ids = [1, host_id(17, 46), host_id(1023, 999), u64::MAX];
    let mut state = DeckState::default();
    state.attention.queued = ids.into_iter().enumerate().map(|(i, id)| (id, i + 1)).collect();
    let expected = state.attention.clone();
    let encoded = encode(&ServerMsg::State(state));
    let Some(ServerMsg::State(decoded)) = read_msg::<_, ServerMsg>(&mut Cursor::new(encoded)).unwrap() else {
        panic!("state frame expected")
    };
    assert_eq!(decoded.attention, expected);
    let data = serde_json::json!({"attention": expected});
    let encoded = encode(&ServerMsg::FleetResult { request: 19, data, error: None });
    let Some(ServerMsg::FleetResult { request: 19, data, error: None }) =
        read_msg::<_, ServerMsg>(&mut Cursor::new(encoded)).unwrap()
    else {
        panic!("fleet frame expected")
    };
    let snapshot: crate::attention::Snapshot = serde_json::from_value(data["attention"].clone()).unwrap();
    assert_eq!(snapshot, expected);
    let json = serde_json::to_value(snapshot).unwrap();
    for id in ids {
        assert!(json["queued"].get(id.to_string()).is_some());
    }
    for bad in ["01", "-1", "18446744073709551616"] {
        let mut value = json.clone();
        value["queued"] = serde_json::json!({bad:1});
        assert!(serde_json::from_value::<crate::attention::Snapshot>(value.clone()).is_err());
        let mut outer = serde_json::to_value(ServerMsg::State(DeckState::default())).unwrap();
        outer["attention"] = value;
        let body = serde_json::to_vec(&outer).unwrap();
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend(body);
        assert!(read_msg::<_, ServerMsg>(&mut Cursor::new(frame)).is_err());
    }
    let outer = serde_json::to_string(&ServerMsg::State(DeckState::default())).unwrap();
    assert!(outer.contains("\"queued\":{}"));
    let body = outer.replace("\"queued\":{}", "\"queued\":{\"1\":1,\"1\":2}").into_bytes();
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend(body);
    assert!(
        read_msg::<_, ServerMsg>(&mut Cursor::new(frame)).is_err(),
        "duplicate session keys cannot silently replace a count"
    );
}

#[test]
fn a_state_patch_carries_only_what_changed_and_rebuilds_the_state() {
    let session = |id| crate::tui::tests::session(id, Tool::Claude, Status::Idle, true);
    let base = DeckState {
        sessions: (1..=20).map(session).collect(),
        // A repository with many branches: the bulk of a whole state.
        branches: (0..1000)
            .map(|i| BranchInfo {
                branch: format!("agent/20261108-topic-{i}"),
                subject: Some(format!("Change number {i} to the deck")),
                ..BranchInfo::default()
            })
            .collect(),
        native_host: Some(HostInfo { name: "omarchy".into(), ..HostInfo::default() }),
        start_dir: "/repo".into(),
        ..DeckState::default()
    };
    let mut next = base.clone();
    next.sessions[3].status = Status::Working;
    next.native_host = None;

    let patch = next.diff(&base);
    assert_eq!(patch.fields(), ["native_host", "sessions"]);
    assert_eq!(next.changed(&base), patch.fields());
    assert!(next.diff(&next).is_empty());
    // A cleared part travels as null; a part that did not change not at all.
    let wire = serde_json::to_value(ServerMsg::StatePatch(patch.clone())).unwrap();
    assert_eq!(wire["t"], "state_patch");
    assert_eq!(wire["native_host"], serde_json::Value::Null);
    assert!(wire.get("branches").is_none() && wire.get("start_dir").is_none(), "{wire}");

    let mut r = Cursor::new(encode(&ServerMsg::StatePatch(patch.clone())));
    let Some(ServerMsg::StatePatch(got)) = read_msg::<_, ServerMsg>(&mut r).unwrap() else { panic!("a patch") };
    assert_eq!(got, patch, "a cleared part arrives as cleared, not as unchanged");
    let mut rebuilt = base.clone();
    rebuilt.apply(got);
    assert_eq!(rebuilt, next);

    let whole = encode(&ServerMsg::State(next.clone())).len();
    let part = encode(&ServerMsg::StatePatch(patch)).len();
    eprintln!("one session's status changed: whole state {whole} bytes, patch {part} bytes");
    assert!(part * 10 < whole, "whole {whole}, patch {part}");
}
