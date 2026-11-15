use super::*;

#[test]
fn historical_clear_does_not_hide_a_new_desktops_startup_errors() {
    let (mut app, _requests) = tests::app_with(vec![], vec![]);
    app.have_state = false;
    app.set_notice("new startup error", true);
    app.handle_server(ServerMsg::State(DeckState {
        error_clears: [("a".repeat(64), 1)].into_iter().collect(),
        ..DeckState::default()
    }));
    assert_eq!(app.error_list().len(), 1);
}

#[test]
fn clearing_from_another_client_clears_local_errors_and_keeps_new_errors() {
    let (mut app, requests) = tests::app_with(vec![], vec![]);
    app.service_version = PROTOCOL_VERSION;
    app.set_notice("old desktop error", true);
    tests::press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    tests::press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(requests.try_iter().any(|message| message == ClientMsg::ClearErrors));
    let second = util::now_secs();
    app.handle_server(ServerMsg::State(DeckState {
        error_clears: [("a".repeat(64), 1)].into_iter().collect(),
        errors: vec![LogEntry::new(second, "new error in the same second")],
        ..DeckState::default()
    }));
    assert_eq!(app.error_list().len(), 1);
    app.set_notice("another desktop error", true);
    let mut cleared = app.state.clone();
    cleared.error_clears.insert("b".repeat(64), 1);
    cleared.errors.clear();
    app.handle_server(ServerMsg::State(cleared));
    assert!(app.error_list().is_empty(), "phone Clear must also clear other desktops' local errors");
}

/// The service's errors before the clear: one origin's single clear and an
/// error from hours ago.
fn before_the_clear() -> DeckState {
    DeckState {
        error_clears: [("a".repeat(64), 1)].into_iter().collect(),
        errors: vec![LogEntry::new(100, "error from hours ago")],
        ..DeckState::default()
    }
}

/// The service's state once it has applied a clear: its own counter rose.
fn after_the_clear(errors: Vec<LogEntry>) -> DeckState {
    let mut state = before_the_clear();
    state.error_clears.insert("s".repeat(64), 1);
    state.errors = errors;
    state
}

/// The connection closed and the deck connected again, as when the service
/// hands over to an updated build (`hand_over` closes every connection);
/// what the deck sends from now on arrives on the returned receiver.
fn reconnect(app: &mut App) -> Receiver<ClientMsg> {
    let (events, _) = mpsc::channel();
    app.link = Some(Link {
        tx: events,
        connect: Arc::new(|| Err(io::Error::other("fixture connector unused"))),
        start: Arc::new(|| Err(io::Error::other("fixture starter unused"))),
    });
    let (client, _service) = UnixStream::pair().unwrap();
    app.reconnected(client);
    let (out, requests) = mpsc::channel();
    app.out = out;
    app.handle_server(ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: "/srv/repo".into() });
    requests
}

fn clear_requests(requests: &Receiver<ClientMsg>) -> usize {
    requests.try_iter().filter(|message| *message == ClientMsg::ClearErrors).count()
}

// Owner (relayed 2026-11-09): "errors come back after i clear them, cleared them
// recently. see errors from hours ago". A clear the service never received
// left its errors for every deck and for this one's next start.
#[test]
fn a_clear_made_while_disconnected_reaches_the_service_after_reconnecting() {
    let (mut app, requests) = tests::app_with(vec![], vec![]);
    app.handle_server(ServerMsg::State(before_the_clear()));
    assert_eq!(app.error_list().len(), 1);
    // The connection is gone: nothing sent now reaches a service.
    drop(requests);
    tests::press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    tests::press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(app.error_list().is_empty());
    let requests = reconnect(&mut app);
    app.handle_server(ServerMsg::State(before_the_clear()));
    assert!(app.error_list().is_empty(), "the cleared error stays hidden while the service lacks the clear");
    assert_eq!(clear_requests(&requests), 1, "the clear goes to the service over the new connection");
    app.handle_server(ServerMsg::State(after_the_clear(vec![LogEntry::new(100, "new error in the same second")])));
    assert_eq!(app.error_list().len(), 1, "an error after the applied clear shows");
    assert_eq!(clear_requests(&requests), 0);
}

#[test]
fn a_reconnect_whose_state_predates_the_clear_sends_it_again_and_an_applied_one_never() {
    let (mut app, requests) = tests::app_with(vec![], vec![]);
    app.handle_server(ServerMsg::State(before_the_clear()));
    tests::press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    tests::press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert_eq!(clear_requests(&requests), 1);
    // The service handed over before it read the request.
    let requests = reconnect(&mut app);
    app.handle_server(ServerMsg::State(before_the_clear()));
    assert!(app.error_list().is_empty(), "the old error must not come back");
    assert_eq!(clear_requests(&requests), 1, "the lost clear is sent again");
    // This time the service applied it before the connection closed: a
    // second clear would hide errors that came after the first.
    let requests = reconnect(&mut app);
    app.handle_server(ServerMsg::State(after_the_clear(vec![LogEntry::new(100, "new error in the same second")])));
    assert_eq!(clear_requests(&requests), 0, "an applied clear is not repeated");
    assert_eq!(app.error_list().len(), 1);
}

// The service's clear empties its whole list: sent again over an error that
// came after it, it would remove that error before the owner saw it.
#[test]
fn a_clear_is_not_sent_again_over_an_error_that_came_after_it() {
    let (mut app, requests) = tests::app_with(vec![], vec![]);
    app.handle_server(ServerMsg::State(before_the_clear()));
    drop(requests);
    tests::press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    tests::press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    let requests = reconnect(&mut app);
    let mut state = before_the_clear();
    state.errors.push(LogEntry::new(util::now_secs() + 60, "the new service could not reach fedora"));
    app.handle_server(ServerMsg::State(state));
    assert_eq!(clear_requests(&requests), 0, "the new error must reach the owner");
    let shown: Vec<String> = app.error_list().into_iter().map(|e| e.text).collect();
    assert_eq!(shown, ["the new service could not reach fedora"], "the cleared error stays hidden");
}
