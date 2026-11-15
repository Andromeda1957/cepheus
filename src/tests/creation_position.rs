use super::*;

fn created(app: &mut App, request: ClientMsg, id: u64, host: Option<&str>, rx: &std::sync::mpsc::Receiver<ClientMsg>) {
    // Begin with the fixture's server-confirmed manual order.
    if app.service_version >= 14 {
        app.state.session_order = app.order.clone();
    }
    let request = match crate::hosts::fed::route(request, |name| (name == "fedora").then_some(1)) {
        crate::hosts::fed::Route::Local(request) | crate::hosts::fed::Route::Host(_, request) => request,
        other => panic!("{other:?}"),
    };
    let request = serde_json::to_value(request).unwrap();
    let local_id = crate::protocol::split_id(id).1;
    let reply: ServerMsg =
        serde_json::from_value(serde_json::json!({"t": "created", "id": local_id, "request": request["request"]}))
            .unwrap();
    let reply = host.map_or(reply.clone(), |host| {
        crate::hosts::fed::inbound(crate::protocol::split_id(id).0, host, "Fedora", reply)
    });
    app.handle_server(reply);
    if app.service_version >= 14 {
        assert!(
            rx.try_iter().all(|m| !matches!(m, ClientMsg::SetOrder { .. })),
            "unknown peer ID must wait for its authoritative State"
        );
        // An older snapshot may arrive while the peer registers the new session.
        app.handle_server(ServerMsg::State(app.state.clone()));
        assert!(rx.try_iter().all(|m| !matches!(m, ClientMsg::SetOrder { .. })));
    }
    let mut state = app.state.clone();
    let mut s = session(id, Tool::Codex, Status::Idle, true);
    s.host = host.map(str::to_string);
    state.sessions.push(s);
    if app.service_version >= 14 {
        // The coordinator acknowledges the SetOrder emitted on Created.
        state.session_order = app.order.clone();
    }
    app.handle_server(ServerMsg::State(state));
    if app.service_version >= 14 {
        let orders: Vec<_> =
            rx.try_iter().filter_map(|m| if let ClientMsg::SetOrder { ids } = m { Some(ids) } else { None }).collect();
        assert!(orders.len() <= 1, "one canonical order write per creation");
        if let Some(ids) = orders.first() {
            assert!(ids.contains(&id));
            let mut acknowledged = app.state.clone();
            acknowledged.session_order = ids.clone();
            app.handle_server(ServerMsg::State(acknowledged));
        }
    }
}

#[test]
fn created_session_becomes_selected_in_normal_mode() {
    let sessions = [1, 2, 3].map(|id| session(id, Tool::Codex, Status::Idle, true)).to_vec();
    let (mut app, rx) = app_with(sessions, vec![]);
    app.order = vec![3, 1, 2];
    app.select(Sel::Session(1));
    command(&mut app, "codex");
    let request = rx.try_recv().unwrap();
    app.select(Sel::Session(2));
    created(&mut app, request, 7, None, &rx);
    assert_eq!(app.rows(), [Sel::Session(3), Sel::Session(1), Sel::Session(7), Sel::Session(2)]);
    assert_eq!(app.current, Some(Sel::Session(7)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn overlapping_hosts_keep_their_own_anchors_when_replies_arrive_in_reverse_order() {
    let remote = |id| crate::protocol::host_id(1, id);
    let mut sessions = [1, 2].map(|id| session(id, Tool::Codex, Status::Idle, true)).to_vec();
    for id in [1, 2] {
        let mut s = session(remote(id), Tool::Codex, Status::Idle, true);
        s.host = Some("fedora".into());
        sessions.push(s);
    }
    let (mut app, rx) = app_with(sessions, vec![]);
    app.hosts.push(host("fedora", "Fedora", true));
    app.select(Sel::Session(remote(1)));
    command(&mut app, "codex fedora");
    let remote_request = rx.try_recv().unwrap();
    app.select(Sel::Session(1));
    command(&mut app, "claude");
    let local_request = rx.try_recv().unwrap();
    created(&mut app, local_request, 7, None, &rx);
    created(&mut app, remote_request, remote(7), Some("fedora"), &rx);
    assert_eq!(app.rows(), [1, 7, 2, remote(1), remote(7), remote(2)].map(Sel::Session));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn starting_without_a_selection_does_not_capture_a_later_selected_row() {
    let (mut app, rx) = app_with(vec![], vec![]);
    command(&mut app, "codex");
    let request = rx.try_recv().unwrap();
    let mut state = app.state.clone();
    state.sessions = [1, 2].map(|id| session(id, Tool::Codex, Status::Idle, true)).to_vec();
    app.handle_server(ServerMsg::State(state));
    app.select(Sel::Session(1));
    created(&mut app, request, 7, None, &rx);
    assert_eq!(app.rows(), [1, 2, 7].map(Sel::Session));
}
