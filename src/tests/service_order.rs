use super::tests::{fake_session, options};
use super::*;
use crate::testsupport::TempDir;

fn replies(rx: &Receiver<Out>) -> Vec<ServerMsg> {
    rx.try_iter()
        .filter_map(|out| match out {
            Out::Msg(bytes) => read_msg(&mut bytes.as_slice()).unwrap(),
            _ => None,
        })
        .collect()
}

#[test]
fn canonical_order_persists_and_reaches_every_viewer() {
    let dir = TempDir::new("shared-order");
    let (tx, _events) = mpsc::channel();
    let mut svc = Service::new(options(&dir, "true"), tx);
    svc.sessions.insert(1, fake_session(1, Tool::Claude));
    svc.sessions.insert(2, fake_session(2, Tool::Codex));
    let mut viewers = Vec::new();
    for id in 1..=2 {
        let (out, rx) = mpsc::channel();
        svc.handle(Ev::ClientNew { id, out, frame_busy: Arc::new(AtomicBool::new(false)) });
        svc.handle_client(id, ClientMsg::Hello { version: 14, role: ClientRole::Tui });
        replies(&rx);
        viewers.push(rx);
    }
    svc.handle_client(1, ClientMsg::SetOrder { ids: vec![2, 1, 2] });
    let first = replies(&viewers[0]);
    let second = replies(&viewers[1]);
    assert!(first
        .iter()
        .any(|msg| matches!(msg, ServerMsg::Notice { text, error: false } if text == "Session order saved.")));
    for messages in [&first, &second] {
        assert!(
            messages.iter().any(|msg| matches!(msg, ServerMsg::State(state) if state.session_order == [2, 1])),
            "both viewers receive the canonical order: {messages:?}"
        );
    }
    let file = dir.path().join("state/order.json");
    assert_eq!(serde_json::from_slice::<Vec<u64>>(&fs::read(&file).unwrap()).unwrap(), [2, 1]);

    for ids in [vec![9, 1], vec![1; crate::protocol::MAX_SESSION_ORDER + 1]] {
        svc.handle_client(1, ClientMsg::SetOrder { ids });
        assert!(replies(&viewers[0]).iter().any(|msg| matches!(msg, ServerMsg::Notice { error: true, .. })));
        assert_eq!(svc.saved_session_order(), [2, 1], "invalid requests preserve the saved order");
    }

    let mut ended = fake_session(3, Tool::Claude);
    ended.alive = false;
    svc.sessions.insert(3, ended);
    util::write_atomic(&file, b"[2,1,4]").unwrap();
    svc.handle_client(1, ClientMsg::SetOrder { ids: vec![3, 2, 4, 2, 1] });
    assert!(replies(&viewers[0]).iter().any(|msg| matches!(msg, ServerMsg::Notice { error: false, .. })));
    assert_eq!(
        svc.saved_session_order(),
        [2, 1],
        "known ended and stale saved IDs are accepted, filtered and deduplicated"
    );

    let (tx, _events) = mpsc::channel();
    let restarted = Service::new(options(&dir, "true"), tx);
    assert_eq!(restarted.own_state().session_order, [2, 1], "service restarts preserve the shared order");
    util::write_atomic(&file, b"[1,2]").unwrap();
    assert_eq!(restarted.own_state().session_order, [1, 2], "legacy local edits are watched");
    util::write_atomic(&file, b"not-json").unwrap();
    assert_eq!(restarted.own_state().session_order, [1, 2], "invalid files keep the last valid order");
}
