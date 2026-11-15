use super::native_message_tests::{fixture, until};
use super::*;

const THREAD: &str = "00000000-0000-4000-8000-000000000001";

fn notices(replies: &mpsc::Receiver<Out>) -> Vec<(String, bool)> {
    replies
        .try_iter()
        .filter_map(|out| match out {
            Out::Msg(bytes) => match crate::protocol::read_msg(&mut bytes.as_slice()).unwrap() {
                Some(ServerMsg::Notice { text, error }) => Some((text, error)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn inbox(dir: &crate::testsupport::TempDir) -> Vec<String> {
    fs::read_to_string(dir.path().join("inbox"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn broadcast_rechecks_working_state_and_direct_messages_can_start_idle_agents() {
    let (dir, mut svc, events, replies, working, _server) = fixture();
    let mut ids = vec![working];
    for _ in 0..3 {
        let id = svc.create(Tool::Codex, None).unwrap();
        until(&mut svc, &events, |s| {
            s.sessions[&id]
                .hosted
                .as_ref()
                .is_some_and(|h| screen::screen_text(&h.lock().term).join("\n").contains("ready"))
        });
        svc.sessions.get_mut(&id).unwrap().conversation = Some(THREAD.into());
        ids.push(id);
    }
    for (id, status) in ids.iter().zip([Status::Working, Status::Idle, Status::Finished, Status::Starting]) {
        svc.sessions.get_mut(id).unwrap().tracker.status = status;
    }
    // IDs can have been selected while all sessions were working. The service
    // must use their current status, rather than trusting that stale selection.
    svc.handle_client(
        7,
        ClientMsg::Broadcast { ids: ids.clone(), text: "Stop the behavior if it is yours".into(), running_only: true },
    );
    let mut done = Vec::new();
    until(&mut svc, &events, |_| {
        done.extend(notices(&replies).into_iter().filter(|(text, _)| text.starts_with("Sent to")));
        !done.is_empty()
    });
    let delivered = inbox(&dir);
    svc.broadcast_message(7, &[ids[1]], "A direct new task");
    until(&mut svc, &events, |_| inbox(&dir).len() == 2);
    let after = inbox(&dir);
    for id in &ids {
        svc.sessions[id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    }
    assert_eq!(delivered.len(), 1, "a broadcast must not start idle, finished or starting agents: {delivered:?}");
    let message = &delivered[0];
    assert!(message.starts_with("[Owner broadcast to all running agents]\n"), "{message}");
    assert!(message.contains("Apply it only to your own current work"), "{message}");
    assert!(message.contains("stop or correct that behavior yourself"), "{message}");
    assert!(message.contains("Do not acknowledge it, investigate which other agent it concerns"), "{message}");
    assert!(message.ends_with("Owner message:\nStop the behavior if it is yours"), "{message}");
    // Leaving the others alone is the requested outcome, not a failure.
    assert_eq!(
        done,
        [(
            "Sent to 1 agent across 1 host; 3 agents stopped working before delivery and were left alone".to_string(),
            false
        )]
    );
    assert_eq!(after[1], "A direct new task", "a direct message may still start an idle agent");
    assert!(!dir.path().join("typed").exists(), "broadcasts never write terminal input");
}
