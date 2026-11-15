use super::tests::{key_recorder_service, recorded_keys};
use super::*;
use crate::testsupport::TempDir;

#[test]
fn send_now_refuses_replay_without_writing_terminal_input() {
    let dir = TempDir::new("svc-send-now-second");
    let (mut svc, id) = key_recorder_service(&dir, Tool::Claude, false);
    svc.broadcast_message(0, &[id], "first");
    assert!(svc.send_now(id).unwrap_err().to_string().contains("Native Send now is unavailable"));
    svc.broadcast_message(0, &[id], "second");
    let sent = Instant::now();
    svc.submit_due(sent + SUBMIT_DELAY);
    svc.keys_due(sent + SUBMIT_DELAY * 2);
    svc.messages_due();
    svc.submit_due(sent + SUBMIT_DELAY * 4);
    let keys = recorded_keys(&svc, &dir, id, 1);
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    assert_eq!(keys, b"!", "unsupported native owner sends and promotion never type or replay into the terminal");
}
