use super::*;
use crate::testsupport::TempDir;

#[test]
fn placed_clear_writer_rechecks_epoch_after_admission() {
    let dir = TempDir::new("placed-writer-custody");
    let (mut svc, id) = super::tests::key_recorder_service(&dir, Tool::Codex, false);
    svc.sessions.get_mut(&id).unwrap().hosted.as_mut().unwrap().pause().unwrap();
    let (done, written) = mpsc::channel();
    svc.client_input_with_receipt(
        id,
        "/model\r",
        Some(Box::new(move |accepted| {
            let _ = done.send(accepted);
        })),
    )
    .unwrap();
    svc.sessions.get_mut(&id).unwrap().enter_conversation(Some("replacement-conversation".into()));
    svc.sessions.get_mut(&id).unwrap().hosted.as_mut().unwrap().resume().unwrap();
    let admitted = written.recv_timeout(Duration::from_secs(5)).unwrap();
    let keys = super::tests::recorded_keys(&svc, &dir, id, 1);
    // A targeted Stop owns a newer epoch than an older admitted Enter.
    svc.sessions.get_mut(&id).unwrap().hosted.as_mut().unwrap().pause().unwrap();
    let (done, stale_enter) = mpsc::channel();
    let hosted = svc.sessions[&id].hosted.as_ref().unwrap();
    hosted
        .write_guarded(
            b"\r".to_vec(),
            Instant::now() + Duration::from_secs(1),
            hosted.input_epoch(),
            Arc::new(Mutex::new(1)),
            1,
            |_| true,
            move |accepted| {
                let _ = done.send(accepted);
            },
        )
        .unwrap();
    let expected = provider_placement::PlacedTarget::new(&svc.sessions[&id], 1, 7).unwrap().expected;
    svc.handle_plugin(
        7,
        1,
        "provider-codex",
        "target",
        &serde_json::json!({"id":id,"expected":expected,"operation":"input","data":"\u{3}"}),
    );
    svc.sessions.get_mut(&id).unwrap().hosted.as_mut().unwrap().resume().unwrap();
    let old_enter = stale_enter.recv_timeout(Duration::from_secs(5)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read(dir.path().join("keys")).unwrap_or_default() != b"!\x03" && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let stopped_keys = fs::read(dir.path().join("keys")).unwrap_or_default();
    svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
    assert!(!admitted, "a queued old-conversation write must fail at the writer");
    assert_eq!(keys, b"!", "the new conversation must receive no model command or Enter");
    assert!(!old_enter, "Stop must revoke the older queued Enter before queuing itself");
    assert_eq!(stopped_keys, b"!\x03");
}

#[test]
fn placed_clear_propagates_custody_failure_during_effort_picker() {
    struct LostTarget {
        reads: u8,
    }
    impl crate::codexpick::SessionIo for LostTarget {
        fn screen(&mut self, _: u64) -> Result<(Vec<String>, u64)> {
            self.reads += 1;
            if self.reads == 1 {
                Ok((
                    vec![
                        "Select a model".into(),
                        "› 1. GPT-6.1-Sol".into(),
                        "  2. GPT-6.1-Terra".into(),
                        "Enter to confirm or esc to cancel".into(),
                    ],
                    0,
                ))
            } else {
                bail!("native target changed before effort confirmation")
            }
        }
        fn input(&mut self, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn type_message(&mut self, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
    }
    let result = crate::codexpick::pick(&mut LostTarget { reads: 0 }, 1, Some("gpt-6.1-sol"), Some("xhigh"));
    assert!(result.unwrap_err().to_string().contains("target changed"));
}
