//! A late peer send must not resurrect a conversation the owner has closed.
use super::*;
use crate::providers::peers::{Request, Target};
use crate::testsupport::TempDir;
use std::os::unix::net::UnixListener;

#[test]
fn native_peer_close_and_conversation_change_cancel_before_submission() {
    let dir = TempDir::new("peer-close");
    let (tx, _events) = mpsc::channel();
    let mut service = Service::new(crate::service::tests::options(&dir, "true"), tx);
    let conversation = "11111111-1111-4111-8111-111111111111";
    let mut session = crate::service::tests::fake_session(9, Tool::Claude);
    session.conversation = Some(conversation.into());
    session.spawned_ms = 1;
    service.sessions.insert(9, session);
    let socket = dir.path().join("peer.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let pid = std::process::id();
    dir.write(
        &format!("sessions/{pid}.json"),
        serde_json::json!({
            "sessionId":conversation,"messagingSocketPath":socket,
            "procStart":crate::procs::stat(pid).unwrap().start_ticks.to_string()
        })
        .to_string(),
    );
    let prepare = || {
        Request::prepare(
            Target { tool: Tool::Claude, pid, conversation, cwd: dir.path(), account_root: dir.path() },
            &Config::default(),
            "private finding",
            "22222222-2222-4222-8222-222222222222",
        )
        .unwrap()
    };
    let first = prepare();
    let stop = service.messages.peer_stop(9, conversation, 1);
    service.close(9);
    assert!(first.deliver(&stop).is_err());
    // The second case must be live so conversation change is the only cancellation trigger.
    let mut replacement = crate::service::tests::fake_session(9, Tool::Claude);
    replacement.conversation = Some(conversation.into());
    replacement.spawned_ms = 2;
    service.sessions.insert(9, replacement);
    let second = prepare();
    let stop = service.messages.peer_stop(9, conversation, 2);
    service.sessions.get_mut(&9).unwrap().conversation = Some("33333333-3333-4333-8333-333333333333".into());
    service.messages_due();
    assert!(second.deliver(&stop).is_err());
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}
