use super::*;
use crate::service::tests::{options, TestClient};
use crate::testsupport::{shutdown_service, TempDir};

#[test]
fn tui_stop_preserves_live_pty_and_draft_without_a_confirmation() {
    let dir = TempDir::new("stop-tui");
    let opts = options(&dir, "echo stand-in ready; read x; echo accepted:$x; read y");
    let socket = opts.socket.clone();
    let service = thread::spawn(move || crate::service::run(opts));
    let mut client = TestClient::connect(&socket);
    client.send(ClientMsg::Create {
        tool: Tool::Claude,
        model: None,
        effort: None,
        host: None,
        cwd: None,
        command: Vec::new(),
        name: None,
        request: None,
    });
    let id = client.wait("fixture session", |m, _| match m {
        ServerMsg::Created { id, .. } => Some(*id),
        _ => None,
    });
    client.wait_screen(id, "stand-in ready");
    client.send(ClientMsg::Input { id, data: "unsent draft".into() });
    client.wait_screen(id, "unsent draft");
    client.wait_state("live fixture state", |s| s.sessions.iter().any(|s| s.id == id && s.alive));
    let (mut app, messages) = tests::app_with(client.state.sessions.clone(), Vec::new());
    app.run_hotkey(Hotkey::Stop);
    assert!(app.dialog.is_none() && !app.quit);
    assert!(app.notice.as_ref().is_some_and(|n| n.1));
    assert!(!messages.try_iter().any(|m| matches!(m, ClientMsg::Shutdown)));
    let (mut stale, messages) = tests::app_with(Vec::new(), Vec::new());
    stale.run_hotkey(Hotkey::Stop);
    assert!(matches!(stale.dialog, Some(Dialog::Confirm { action: Action::Shutdown, .. })));
    stale.state.sessions = client.state.sessions.clone();
    tests::press(&mut stale, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(!stale.quit && !stale.stopping_service);
    assert!(!messages.try_iter().any(|m| matches!(m, ClientMsg::Shutdown)));
    let (mut raced, _) = tests::app_with(Vec::new(), Vec::new());
    raced.run_hotkey(Hotkey::Stop);
    tests::press(&mut raced, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(raced.stopping_service);
    raced.handle_server(ServerMsg::Notice { text: crate::protocol::STOP_REFUSED_NOTICE.into(), error: true });
    assert!(!raced.stopping_service && !raced.quit, "service refusal cancels a stale Stop request");
    client.wait_screen(id, "unsent draft");
    client.send(ClientMsg::Input { id, data: "\r".into() });
    client.wait_screen(id, "accepted:unsent draft");
    shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}
