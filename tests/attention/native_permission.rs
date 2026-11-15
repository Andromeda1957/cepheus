//! Native Claude hook approval has one-request scope and never types into PTY.
use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn responses(root: &Path, id: u64) -> Vec<Value> {
    records(&root.join(format!("native-requests-{}.jsonl", key("hub", id))))
}
fn native_request(root: &Path, id: u64, prompt: &str, overrides: Value) -> Value {
    let count = responses(root, id).len();
    control(root, "hub", id, json!({"prompt_id":prompt,"overrides":overrides}));
    let values = responses(root, id);
    assert_eq!(values.len(), count + 1, "one original hook request has one bounded result");
    let result = values.last().unwrap().clone();
    assert_eq!(result["exit_code"], 0, "native hook errors must remain neutral");
    assert_eq!(result["stderr_bytes"], 0);
    result
}
fn neutral(root: &Path, id: u64, prompt: &str, overrides: Value) {
    let result = native_request(root, id, prompt, overrides);
    assert_eq!(result["stdout"], "", "unsupported or revoked request must yield no approval object");
    assert!(result["response"].is_null());
}
fn reads(root: &Path, id: u64) -> Vec<Value> {
    records(&root.join(format!("native-read-{}.jsonl", key("hub", id))))
}

#[test]
fn bundled_native_permission_hook_approves_only_one_exact_read_without_terminal_input() {
    let scratch = Scratch::new("attention-native-permission");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let fake = install_fake(root);
    let native = root.join("native-agent.py");
    fs::write(&native, include_bytes!("native_agent.py")).unwrap();
    let mut fleet = AttentionFleet::new(root, &binary);
    configure(&fleet.hub, root, &fake, "hub");
    configure(&fleet.peer, root, &fake, "peer");
    // The native guard correctly excludes every Deck/provider config root.
    // Keep this real synthetic repository beside, never inside, those roots.
    let cwd = root.join("native-worktree");
    fs::create_dir(&cwd).unwrap();
    let git = std::process::Command::new("git").args(["init", "-q"]).arg(&cwd).output().unwrap();
    assert!(git.status.success(), "private native Git fixture: {}", String::from_utf8_lossy(&git.stderr));
    let cwd = cwd.canonicalize().unwrap();
    for private in [&fleet.hub.root, &fleet.peer.root] {
        let private = private.canonicalize().unwrap();
        assert!(
            !cwd.starts_with(&private) && !private.starts_with(&cwd),
            "native worktree must not overlap configured Deck or provider private roots"
        );
    }
    fleet.hub.configure(|config| {
        config["start_dir"] = json!(cwd);
        config["claude_hooks"] = json!(true);
        config["claude_command"] = json!(["/usr/bin/python3", native, root, fleet.hub.executable]);
    });
    let safe = cwd.join("safe.txt");
    let changed = cwd.join("different.txt");
    let credential = cwd.join(".env");
    let outside = root.join("outside.txt");
    for path in [&safe, &changed, &credential, &outside] {
        fs::write(path, "OWNED_NATIVE_READ_OUTPUT\n").unwrap();
    }
    fleet.hub.start();
    ready(&fleet.hub);
    let caps: Value = serde_json::from_slice(&fleet.hub.run(&["ctl", "fleet", "capabilities"], None).unwrap()).unwrap();
    assert!(
        caps["protocol"].as_u64().is_some_and(|version| version >= 16),
        "dedicated native permission hook requires protocol16: {caps}"
    );
    let id = start(&fleet.hub, "claude", "@here", &cwd, "private-native-permission");
    wait("fake Claude has real monitored conversation and bundled helper", || {
        session(&fleet.hub, id)["conversation"].is_string()
            && events(root, "hub", id).iter().any(|event| event["category"] == "native_ready")
    });
    let manifest = read_json(&root.join(format!("native-helper-{}.json", key("hub", id)))).unwrap();
    let helper = PathBuf::from(manifest["helper"].as_str().unwrap());
    let metadata = fs::metadata(&helper).unwrap();
    assert!(helper.starts_with(fleet.hub.root.join("state/attention/helpers")));
    assert!(metadata.is_file() && metadata.nlink() == 1);
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(metadata.permissions().mode() & 0o077, 0, "materialized hook is private");
    assert_eq!(manifest["sha256"], hash(&helper));
    let rules =
        json!([{"id":"read-source","tool":"Read","pattern":{"kind":"native_exact","input":{"file_path":safe}}}]);
    call(&fleet.hub, "attention-supervisor-rules", &json!({"rules":rules}));
    neutral(root, id, "native-off", json!({}));
    assert!(reads(root, id).is_empty());
    call(&fleet.hub, "attention-supervisor-set", &json!({"enabled":true}));
    // Claude prompt_id is turn-grain: an unmatched Read in this turn does
    // not consume the later first eligible attempt for the same native id.
    neutral(root, id, "native-one", json!({"tool_input":{"file_path":changed}}));
    let allowed = native_request(root, id, "native-one", json!({}));
    let expected = json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}});
    if allowed["response"] != expected {
        let receipts = fs::read_dir(fleet.hub.root.join("state/attention/permission-requests"))
            .ok()
            .into_iter()
            .flatten()
            .take(8)
            .filter_map(|entry| entry.ok().and_then(|entry| read_json(&entry.path())))
            .collect::<Vec<_>>();
        panic!("exact native Read must be allowed once: helper_result={allowed}; helper_manifest={manifest}; session={}; attention={}; categorical_receipts={}",session(&fleet.hub,id),attention(&fleet.hub),json!(receipts));
    }
    let stdout = allowed["stdout"].as_str().unwrap();
    assert!(stdout.ends_with('\n') && stdout.lines().count() == 1);
    assert_eq!(reads(root, id), vec![json!({"prompt_id":"native-one","text":"OWNED_NATIVE_READ_OUTPUT\n"})]);
    neutral(root, id, "native-one", json!({}));
    neutral(root, id, "native-one", json!({"tool_input":{"file_path":changed}}));
    // The same actual fake process survives private exec handover. A duplicate
    // original prompt must still never grant a second read after recovery.
    super::delivery::handover(&fleet.hub, &binary);
    neutral(root, id, "native-one", json!({}));
    call(&fleet.hub, "attention-supervisor-set", &json!({"enabled":false}));
    neutral(root, id, "native-disabled", json!({}));
    call(&fleet.hub, "attention-supervisor-set", &json!({"enabled":true}));
    for (prompt, overrides) in [
        ("native-changed", json!({"tool_input":{"file_path":changed}})),
        ("native-unknown-input", json!({"tool_input":{"file_path":safe,"unknown":true}})),
        ("native-bash", json!({"tool_name":"Bash","tool_input":{"command":"cat safe.txt"}})),
        ("native-unknown-tool", json!({"tool_name":"UnrecognizedTool"})),
        ("native-outside", json!({"tool_input":{"file_path":outside}})),
        ("native-credential", json!({"tool_input":{"file_path":credential}})),
        ("native-conversation", json!({"session_id":"00000000-0000-0000-0000-000000000000"})),
        ("native-missing-prompt", json!({"prompt_id":""})),
    ] {
        neutral(root, id, prompt, overrides);
    }
    call(
        &fleet.hub,
        "attention-supervisor-rules",
        &json!({"rules":[{"id":"replace-read-source","tool":"Read","pattern":{"kind":"native_exact","input":{"file_path":changed}}}]}),
    );
    neutral(root, id, "native-rule-replaced", json!({}));
    assert_eq!(reads(root, id).len(), 1, "one exact request authorizes one fixed harmless read");
    assert!(
        events(root, "hub", id).iter().all(|event| event["category"] != "raw_input"),
        "native hook never selects options, pastes, or presses Enter"
    );
    public(&attention(&fleet.hub));
    let audit = fleet.hub.root.join("state/attention/permission-requests");
    if audit.is_dir() {
        for entry in fs::read_dir(audit).unwrap() {
            let value = read_json(&entry.unwrap().path()).unwrap();
            let metadata = value.as_object().unwrap();
            for field in ["file_path", "tool_input", "token", "fingerprint"] {
                assert!(!metadata.contains_key(field), "native one-request audit excludes private {field}");
            }
            let text = value.to_string();
            for forbidden in ["OWNED_NATIVE_READ_OUTPUT", root.to_str().unwrap()] {
                assert!(!text.contains(forbidden), "native one-request audit retains categories and ids only");
            }
        }
    }
    eprintln!(
        "attention-native-permission: {}",
        json!({"bundled_hook_sha256":manifest["sha256"],"actual_fake_parent_and_helper_child":true,"native_exact_read_count":1,"unmatched_same_turn_then_eligible":true,"duplicate_after_handover":0,"pty_input_bytes":0,"neutral_forbidden_and_revoked":true})
    );
}
