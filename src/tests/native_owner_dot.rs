use super::*;
use crate::service::native_message_tests::{fixture, until};
use std::os::unix::fs::PermissionsExt;

#[test]
fn dot_owner_messages_without_a_conversation_use_only_the_native_controller() {
    for text in ["exact owner text", "/reconnect"] {
        let (dir, mut service, events, replies, id, _native) = fixture();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let _dot = crate::dot::api::Server::start(dir.path(), Arc::new(AtomicBool::new(false)), |_| {
            panic!("the controller PID mismatch must refuse before writing")
        })
        .unwrap();
        service.opts.config.providers.push(
            serde_json::from_value(serde_json::json!({
                "id":"dot", "label":"Dot", "command":["cepheus","dot","chat"],
                "cloud":true, "chat_snapshot":dir.path().join("transcript.json")
            }))
            .unwrap(),
        );
        let session = service.sessions.get_mut(&id).unwrap();
        session.tool = Tool::Dynamic("dot");
        session.conversation = None;
        service.send_message(7, id, 803, text, false);
        let job = service.messages.queues[&id].front().unwrap();
        assert!(matches!(&job.native, OwnerNative::Dot(_)));
        assert!(!job.cancel.load(Ordering::SeqCst));
        service.messages_due();
        assert!(!service.messages.queues[&id].front().unwrap().cancel.load(Ordering::SeqCst));
        let mut refused = false;
        until(&mut service, &events, |_| {
            while let Ok(Out::Msg(bytes)) = replies.try_recv() {
                if let Some(ServerMsg::MessageSent { id: got, request: 803, error: Some(error), .. }) =
                    crate::protocol::read_msg(&mut bytes.as_slice()).unwrap()
                {
                    assert_eq!(got, id);
                    assert!(error.contains("controller changed"), "{error}");
                    refused = true;
                }
            }
            refused
        });
        assert!(!dir.path().join("typed").exists(), "native refusal cannot fall back to the PTY");
        service.begin_shutdown(false);
    }
}
