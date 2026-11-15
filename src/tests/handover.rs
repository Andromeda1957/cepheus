use super::*;
use crate::protocol::Status;
use crate::testsupport::TempDir;
use std::os::unix::fs::symlink;

fn sample() -> Handover {
    let mut tracker = Tracker::new(1_000, true);
    tracker.status = Status::Working;
    tracker.unseen = true;
    Handover {
        host_outages: vec![HostOutageHandover {
            config: crate::config::HostConfig {
                role: Default::default(),
                name: "fixture".into(),
                label: None,
                tag: 1,
                addresses: vec![],
                ssh: None,
                fingerprint: String::new(),
            },
            down_for_ms: 1200,
            reported: false,
            reason: Some("connection refused".into()),
        }],
        scheduler: Default::default(),
        scheduler_fds: Vec::new(),
        extensions: Default::default(),
        version: HANDOVER_VERSION,
        listener_fd: 5,
        lock_fd: 4,
        view: (120, 40),
        errors: vec![LogEntry::new(7, "an error")],
        usage: Usage::default(),
        dev: Some(DevHandover { pid: 99, started_at: 3, log: PathBuf::from("/state/dev-server.log"), stopping: false }),
        sessions: vec![SessionHandover {
            account_root: None,
            account: None,
            id: 2,
            tool: Tool::Codex,
            pid: 1234,
            fd: 9,
            cols: 120,
            rows: 40,
            screen: "\x1b[0mhello\r\n\x1b[2;1H".into(),
            conversation: Some("019a".into()),
            term_title: Some("title".into()),
            title: "Finish the work".into(),
            created_at: 10,
            spawned_ms: 11,
            inputs: Some(7),
            tracker,
            facts: SessionFacts { first_prompt: Some("Finish the work".into()), ..SessionFacts::default() },
            remove_when_ended: false,
            retry_seen_ago_ms: Some(500),
            retry_sent_ago_ms: vec![1000, 2000],
            retry_gave_up: false,
            retry_quota_reset: None,
            retry_quota_unknown: false,
            owner_draft: true,
            left: vec!["0199".into()],
            update_notice: true,
            name: Some("Ship the parser".into()),
            cwd: Some(PathBuf::from("/srv/elsewhere")),
            command: vec!["htop".into(), "-d".into(), "10".into()],
            job: Some("scan:20261101-040506-0a1b".into()),
            recovery: crate::service::recovery::ClientRecovery {
                paused: true,
                interrupt_pending: true,
                draft_files: vec!["/private/uploads/held.jpg".into()],
                draft_submit_requested: true,
                draft_restored: true,
                draft: Some("an unsent draft\n/private/uploads/image.jpg".into()),
                pending: vec!["a queued message".into()],
                attempts_ms: vec![1234],
                ..Default::default()
            },
        }],
    }
}

#[test]
fn the_state_survives_the_file_and_the_file_does_not() {
    let dir = TempDir::new("handover-file");
    let path = dir.path().join("handover-1.json");
    let state = sample();
    state.write(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "screens are private");
    assert_eq!(Handover::take(&path).unwrap(), state);
    assert!(!path.exists(), "the screens are not left on disk");

    // Earlier version-2 builds omit outage metadata.
    state.write(&path).unwrap();
    let mut legacy = serde_json::to_value(&state).unwrap();
    legacy.as_object_mut().unwrap().remove("host_outages");
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let mut expected = state;
    expected.host_outages.clear();
    assert_eq!(Handover::take(&path).unwrap(), expected, "older live services can still hand over");
}

#[test]
fn a_handover_from_before_client_recovery_uses_an_empty_input_ledger() {
    let dir = TempDir::new("handover-old-client-input");
    let path = dir.path().join("handover.json");
    let mut value = serde_json::to_value(sample()).unwrap();
    value["sessions"][0].as_object_mut().unwrap().remove("recovery");
    value["sessions"][0].as_object_mut().unwrap().remove("inputs");
    std::fs::write(&path, value.to_string()).unwrap();
    let restored = Handover::read(&path).unwrap();
    assert_eq!(restored.sessions[0].recovery, Default::default());
    assert_eq!(restored.sessions[0].inputs, None, "legacy state cannot attest input custody");
}

#[test]
fn another_handover_version_is_refused() {
    let dir = TempDir::new("handover-version");
    let path = dir.path().join("handover.json");
    let mut value = serde_json::to_value(sample()).unwrap();
    value["version"] = serde_json::json!(HANDOVER_VERSION + 1);
    std::fs::write(&path, value.to_string()).unwrap();
    let err = Handover::read(&path).unwrap_err().to_string();
    assert!(err.contains(&format!("handover version {}", HANDOVER_VERSION + 1)), "{err}");
    std::fs::write(&path, "{\"version\": 1}").unwrap();
    assert!(Handover::read(&path).is_err(), "missing fields are refused, not defaulted");
    assert_eq!(check_command(&path), 1);
}

#[test]
fn a_replaced_binary_is_found_from_the_deleted_link() {
    let dir = TempDir::new("handover-binary");
    let exe = dir.path().join("cepheus");
    // A link to an existing program: writing an executable here could race
    // a fork in another test thread and fail with "text file busy".
    symlink("/bin/sh", &exe).unwrap();
    let deleted = PathBuf::from(format!("{} (deleted)", exe.display()));
    let (path, id) = replacement_of(&deleted).expect("the new binary");
    assert_eq!(path, exe);
    let meta = std::fs::metadata("/bin/sh").unwrap();
    assert_eq!((id.dev, id.ino), (meta.dev(), meta.ino()));
    assert_eq!(replacement_of(&exe), None, "a binary still in place is not an update");
    let missing = PathBuf::from(format!("{} (deleted)", dir.path().join("gone").display()));
    assert_eq!(replacement_of(&missing), None, "nothing installed there yet");
    let data = dir.write("data", "not a program");
    assert_eq!(replacement_of(&PathBuf::from(format!("{} (deleted)", data.display()))), None, "not executable");
}

#[test]
fn a_binary_that_does_not_accept_the_state_is_not_run() {
    let dir = TempDir::new("handover-check");
    let path = dir.path().join("handover.json");
    sample().write(&path).unwrap();
    // `sh service --check-handover …` looks for a script named service.
    let err = check(Path::new("/bin/sh"), &path).unwrap_err().to_string();
    assert!(err.contains("refused"), "{err}");
    // A program that says nothing has not accepted anything.
    let err = check(Path::new("/bin/true"), &path).unwrap_err().to_string();
    assert!(err.contains("no answer"), "{err}");
    assert!(check(&dir.path().join("missing"), &path).is_err());
}

#[test]
fn an_older_handover_holds_unknown_owner_drafts() {
    let mut value = serde_json::to_value(sample()).unwrap();
    value["sessions"][0].as_object_mut().unwrap().remove("owner_draft");
    let read: Handover = serde_json::from_value(value).unwrap();
    assert!(read.sessions[0].owner_draft);
}
