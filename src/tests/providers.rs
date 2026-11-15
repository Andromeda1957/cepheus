use super::*;
use serde_json::json;

fn provider() -> Provider {
    serde_json::from_value(json!({
        "id":"example-agent", "label":"Example", "command":["example-agent"],
        "model_flag":"--model", "effort_flag":"--effort",
        "new_session_args":["--session-id", "{conversation}"],
        "resume_args":["--resume", "{conversation}"]
    }))
    .unwrap()
}

#[test]
fn registration_and_runtime_values_cannot_become_shell_or_option_syntax() {
    let original = provider();
    for id in ["../escape", "claude", "-flag", "a;b", "a\n"] {
        let mut p = original.clone();
        p.id = id.into();
        assert!(p.validate().is_err(), "accepted {id:?}");
    }
    let mut shell = original.clone();
    shell.command = vec!["sh".into(), "-c".into(), "echo unsafe".into()];
    assert!(shell.validate().is_err());
    let mut flag = original.clone();
    flag.model_flag = Some("--model;echo".into());
    assert!(flag.validate().is_err());
    let mut placeholder = original.clone();
    placeholder.resume_args = vec!["--resume={conversation}".into()];
    assert!(placeholder.validate().is_err());
    assert!(original.flags(&StartModel { model: Some("--other-option".into()), effort: None }).is_err());
    assert!(original.launch(Some("../../conversation")).is_err());
    assert!(serde_json::from_str::<Tool>("\"../../agent\"").is_err());
    let cfg = Config::default();
    assert!(configured_launch(&cfg, Tool::Dynamic("unregistered-agent"), None).is_err());
}

#[test]
fn provider_resume_and_model_flags_survive_protocol_and_registry_round_trips() {
    let p = provider();
    let launch = p.launch(None).unwrap();
    let id = launch.conversation.unwrap();
    let tool: Tool = serde_json::from_value(json!(p.id)).unwrap();
    let saved = crate::registry::Entry::started(7, tool, Some(id.clone()), "Example task".into(), 1);
    let recovered: crate::registry::Entry = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    let resume = p.launch(recovered.conversation.as_deref()).unwrap();
    assert_eq!(resume.argv, ["example-agent", "--resume", &id]);
    assert_eq!(
        p.flags(&StartModel { model: Some("org/model".into()), effort: Some("high".into()) }).unwrap(),
        ["--model", "org/model", "--effort", "high"]
    );
    assert_eq!(recovered.tool, tool);
    // These strings are persisted by older builds and used by the phone.
    assert_eq!(serde_json::to_value(Tool::Antigravity).unwrap(), json!("antigravity"));
    assert_eq!(serde_json::to_value(Tool::OpenCode).unwrap(), json!("opencode"));
}

#[test]
fn cloud_discovery_never_exposes_the_local_snapshot_path() {
    let mut p = provider();
    p.cloud = true;
    p.chat_snapshot = Some("/private/dot/transcript.json".into());
    p.validate().unwrap();
    let safe = serde_json::to_value(Info::from(&p)).unwrap();
    assert_eq!(safe["cloud"], true);
    assert!(!safe.to_string().contains("/private"));
    assert!(safe.get("command").is_none());
    assert!(safe.get("chat_snapshot").is_none());
    p.chat_snapshot = Some("relative/path".into());
    assert!(p.validate().is_err());

    let directory = crate::testsupport::TempDir::new("dot-activity");
    p.id = "dot".into();
    p.chat_snapshot = Some(directory.path().join("transcript.json"));
    p.working_patterns = vec!["Dot is working".into()];
    let pid = Some(std::process::id());
    let now = 50_000;
    // These are indistinguishable from conversation/draft rows, even after wrapping.
    let lines = vec!["Dot is working".into(), "Message > Dot is working".into(), "😀 draft ".repeat(4_000)];
    let observed = || configured_screen_signals(&p, &lines, pid, now);
    assert!(!observed().working, "missing activity must never fall back to screen text");
    let active = json!({"pid":pid, "at":now, "known":true, "working":true});
    for (field, value) in [
        ("working", json!(false)),
        ("known", json!(false)),
        ("pid", json!(0)),
        ("at", json!(now - 10_001)),
        ("at", json!(now + 1)),
        ("working", json!("true")),
    ] {
        let mut invalid = active.clone();
        invalid[field] = value;
        directory.write("activity.json", &invalid.to_string());
        assert!(!observed().working, "accepted {invalid}");
    }
    directory.write("activity.json", &active.to_string());
    assert!(observed().working, "real work survives a long wrapped draft and transient terminal notices");
    assert!(!configured_screen_signals(&p, &lines, None, now).working);
    directory.write("activity.json", "not JSON");
    assert!(!observed().working);
    directory.write("activity.json", &"x".repeat(1025));
    assert!(!observed().working);
    std::fs::remove_file(directory.path().join("activity.json")).unwrap();
    directory.write("other.json", &active.to_string());
    std::os::unix::fs::symlink(directory.path().join("other.json"), directory.path().join("activity.json")).unwrap();
    assert!(!observed().working);
    p.chat_snapshot = None;
    assert!(!configured_screen_signals(&p, &lines, pid, now).working);
    assert!(!screen_signals(Tool::Dynamic("dot"), &["esc to interrupt".into()], None).working);
    p.id = "example-agent".into();
    p.prompt_patterns = vec!["Message >".into()];
    let signals = configured_screen_signals(&p, &lines, pid, now);
    assert!(signals.working, "other registered providers retain configured screen patterns");
    assert_eq!(signals.prompt, Some("question"));
}
