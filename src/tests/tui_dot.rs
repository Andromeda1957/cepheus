use super::*;
use crate::testsupport::TempDir;

#[test]
fn transient_snapshot_failure_keeps_the_visible_conversation_and_draft() {
    let dir = TempDir::new("dot-feed");
    let (mut app, _) = crate::tui::tests::app_with(Vec::new(), Vec::new());
    app.dot.items =
        vec![Item::User { client_id: None, text: "Keep this message".into(), images: Vec::new(), at: None }];
    app.dot.draft = "unsent draft".into();
    for contents in ["{partial", "not a snapshot"] {
        let path = dir.write("transcript.json", contents);
        app.read_dot_snapshot(path);
        assert!(matches!(&app.dot.items[..], [Item::User {text,..}] if text == "Keep this message"));
        assert_eq!(app.dot.draft, "unsent draft");
    }
}

fn ready() -> (App, Receiver<ClientMsg>) {
    let mut session = crate::tui::tests::session(7, Tool::Dynamic("dot"), Status::Idle, true);
    session.conversation = None;
    let (mut app, rx) = crate::tui::tests::app_with(vec![session], Vec::new());
    app.service_version = 19;
    (app, rx)
}

#[test]
fn pasted_image_and_multiline_draft_use_the_direct_provider_api_and_survive_refusal() {
    let (mut app, rx) = ready();
    let bytes = b"\x89PNG\r\n\x1a\nfixture".to_vec();
    app.dot_type("first\nsecond");
    app.dot_paste(crate::clipboard::Clip::Image { name: "pasted.png".into(), bytes: bytes.clone() });
    app.dot_submit();
    let (request, args) = rx
        .try_iter()
        .find_map(|message| match message {
            ClientMsg::Plugin { request, plugin, command, args } => {
                assert_eq!((plugin.as_str(), command.as_str()), ("provider-dot", "send"));
                Some((request, args))
            }
            ClientMsg::Input { .. } | ClientMsg::Broadcast { .. } | ClientMsg::SendMessage { .. } => {
                panic!("Dot must not use a terminal composer")
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(args["text"], "first\nsecond");
    assert_eq!(args["expected"]["pid"], 1007);
    assert!(args["expected"]["conversation"].is_null());
    let image: Image = serde_json::from_value(args["images"][0].clone()).unwrap();
    assert_eq!(image.bytes().unwrap().0, bytes);
    app.dot_type("new draft");
    app.handle_server(ServerMsg::PluginResult {
        request,
        data: serde_json::Value::Null,
        error: Some("Controller needs an update".into()),
    });
    assert_eq!(app.dot.draft, "first\nsecond\nnew draft");
    assert_eq!(app.dot.images[0].bytes().unwrap().0, bytes);
    assert!(app.dot.sending.is_none());
    assert!(app.dot.problem.as_ref().unwrap().contains("update"));
}

#[test]
fn accepted_message_stays_visible_until_its_exact_feed_identity_arrives() {
    let (mut app, _) = ready();
    app.dot_type("same words");
    app.dot_submit();
    let request = app.dot.sending.as_ref().unwrap().request;
    app.handle_server(ServerMsg::PluginResult { request, data: serde_json::json!({"id":"new"}), error: None });
    assert_eq!(app.dot.submitted.len(), 1);
    let dir = TempDir::new("dot-receipt-gap");
    let path = dir.write("transcript.json", r#"[{"k":"user","id":"older","text":"same words"}]"#);
    app.read_dot_snapshot(path);
    assert_eq!(app.dot.submitted.len(), 1, "repeated text is not a receipt");
    let path = dir.write(
        "transcript.json",
        r#"[{"k":"user","id":"older","text":"same words"},{"k":"user","id":"new","text":"same words"}]"#,
    );
    app.read_dot_snapshot(path);
    assert!(app.dot.submitted.is_empty());
    assert_eq!(app.dot.items.len(), 2);

    app.dot_type("feed before receipt");
    app.dot_submit();
    let request = app.dot.sending.as_ref().unwrap().request;
    let path = dir.write("transcript.json", r#"[{"k":"user","id":"early","text":"feed before receipt"}]"#);
    app.read_dot_snapshot(path);
    app.handle_server(ServerMsg::PluginResult { request, data: serde_json::json!({"id":"early"}), error: None });
    assert!(app.dot.submitted.is_empty(), "a receipt must not duplicate a message already in the feed");
}

#[test]
fn local_controller_receipts_do_not_become_chat_messages() {
    for control in
        [crate::dot::api::Control::Resolve, crate::dot::api::Control::Reconnect, crate::dot::api::Control::Quit]
    {
        let (mut app, _rx) = ready();
        let text = format!("/{}", serde_json::to_value(control).unwrap().as_str().unwrap());
        app.dot_type(&text);
        app.dot_submit();
        let request = app.dot.sending.as_ref().unwrap().request;
        app.handle_server(ServerMsg::PluginResult {
            request,
            data: serde_json::json!({"control":control}),
            error: None,
        });
        assert!(app.dot.sending.is_none() && app.dot.draft.is_empty() && app.dot.submitted.is_empty());
        assert!(app.dot.problem.is_none());
        app.dot_type(&text);
        app.dot_submit();
        let request = app.dot.sending.as_ref().unwrap().request;
        app.handle_server(ServerMsg::PluginResult { request, data: serde_json::json!({"id":"cloud"}), error: None });
        assert_eq!(app.dot.draft, text, "a cloud receipt cannot acknowledge a local control");
        assert!(app.dot.submitted.is_empty() && app.dot.problem.is_some());
    }
}

#[test]
fn scrolling_moves_through_the_held_conversation_without_asking_the_service() {
    let (mut app, rx) = ready();
    app.dot.items =
        (0..80).map(|n| Item::Assistant { text: format!("reply {n}"), questions: Vec::new(), at: None }).collect();
    app.set_pane(ratatui::layout::Rect::new(0, 0, 80, 24));
    app.mode = Mode::Insert;
    let _ = rx.try_iter().count();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.dot_key(key(KeyCode::PageUp));
    let page = app.dot.scroll;
    assert!(page > 0);
    app.dot_key(key(KeyCode::PageUp));
    assert_eq!(app.dot.scroll, 2 * page);
    // The mouse wheel's step.
    app.dot_scroll(ScrollOp::Lines(-3));
    assert_eq!(app.dot.scroll, 2 * page - 3);
    app.dot_key(key(KeyCode::End));
    assert_eq!(app.dot.scroll, 0);
    app.dot_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    assert_eq!(app.dot.scroll, crate::view::dot_scroll_limit(&app));
    assert!(app.dot.scroll > 2 * page);
    assert!(rx.try_iter().next().is_none(), "no service request per step");
}

#[test]
fn a_failure_shows_only_while_the_bridge_runs() {
    let (mut app, _) = ready();
    app.dot.items = vec![Item::Notice { text: "Cannot connect to Dot; retrying. Nothing is resent.".into() }];
    assert_eq!(app.dot_failure(), Some("Cannot connect to Dot; retrying. Nothing is resent."));
    app.dot.items.push(Item::Assistant { text: "back".into(), questions: Vec::new(), at: None });
    assert_eq!(app.dot_failure(), None, "only the bridge's current, last notice counts");
    app.dot.items.push(Item::Notice { text: "stale".into() });
    app.state.sessions[0].alive = false;
    assert_eq!(app.dot_failure(), None);
}
