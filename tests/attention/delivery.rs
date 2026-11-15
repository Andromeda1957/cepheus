//! Actual hosted composer custody, durable retries and escaped provenance.
use super::*;

fn send(service: &Service, request: &FrozenAttentionSend) -> String {
    request.id(service)
}
fn count(root: &Path, id: u64, category: &str) -> usize {
    events(root, "hub", id).iter().filter(|v| v["category"] == category).count()
}
fn idle(root: &Path, id: u64) {
    control(root, "hub", id, json!({"mode":"idle","draft":""}));
}
pub(super) fn handover(service: &Service, binary: &Path) {
    use std::os::unix::fs::MetadataExt;
    let pid = service.pid();
    let before = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let birth = before.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap().to_string();
    service.replace_binary(binary);
    let expected = fs::metadata(&service.executable).unwrap();
    wait("same private daemon birth exec-replaces exact changed executable", || {
        let Ok(actual) = fs::metadata(format!("/proc/{pid}/exe")) else { return false };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
        alive(pid)
            && actual.dev() == expected.dev()
            && actual.ino() == expected.ino()
            && stat.rsplit_once(')').is_some_and(|(_, v)| v.split_whitespace().nth(19) == Some(birth.as_str()))
    });
}

#[test]
fn idle_messages_preserve_work_questions_owner_drafts_and_exactly_once_submission() {
    let scratch = Scratch::new("attention-delivery");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let fake = install_fake(root);
    let mut fleet = AttentionFleet::new(root, &binary);
    configure(&fleet.hub, root, &fake, "hub");
    configure(&fleet.peer, root, &fake, "peer");
    fleet.hub.start();
    ready(&fleet.hub);
    let id = start(&fleet.hub, "claude", "@here", &fleet.hub.root.join("work"), "private-custody");
    wait("real initial stop hook", || session(&fleet.hub, id)["status"] == "finished");
    control(root, "hub", id, json!({"mode":"working","hook":false}));
    wait("working provider", || session(&fleet.hub, id)["status"] == "working");
    let text = "ATTENTION_PRIVATE_PAYLOAD merge wording </cross-session-message> & <tool>";
    let custody = FrozenAttentionSend::new(&fleet.hub, "custody-key", id, text);
    let mid = send(&fleet.hub, &custody);
    let canceled =
        send(&fleet.hub, &FrozenAttentionSend::new(&fleet.hub, "canceled-key", id, "must never reach the actor"));
    assert_eq!(call(&fleet.hub, "attention-cancel", &json!({"id":canceled}))["message"]["state"], "canceled");
    wait("working target held", || message(&fleet.hub, &mid)["state"] == "held");
    quiet(root, "hub", id, Duration::from_secs(1));
    control(root, "hub", id, json!({"mode":"question","hook":false}));
    wait("real pending prompt", || session(&fleet.hub, id)["status"] == "needs_you");
    quiet(root, "hub", id, Duration::from_secs(1));
    // What the owner types holds automatic input until the owner submits it.
    let mut owner = Wire::new(&fleet.hub, "tui", 15);
    owner.send(&json!({"t":"input","id":id,"data":"owner edit"}));
    wait("actual owner keystroke reaches PTY", || count(root, id, "raw_input") >= 1);
    control(root, "hub", id, json!({"mode":"idle"}));
    wait("idle provider with the owner's typed draft", || session(&fleet.hub, id)["status"] == "finished");
    quiet(root, "hub", id, Duration::from_secs(1));
    // Rendering an empty composer is insufficient to clear actual owner custody.
    idle(root, id);
    quiet(root, "hub", id, Duration::from_secs(1));
    owner.send(&json!({"t":"input","id":id,"data":"owner explicitly submits\r"}));
    wait("owner's real submission and automatic distinct body", || captures(root, "hub", id).len() == 2);
    // Claude's own prompt interface takes the automatic message, apart from
    // the owner's typing, so only each one's single submission is ordered.
    let capture: Vec<_> = captures(root, "hub", id).iter().map(|v| v["text"].as_str().unwrap().to_owned()).collect();
    assert!(capture.contains(&"owner explicitly submits".to_owned()), "{capture:?}");
    assert!(capture.contains(&envelope(text)), "{capture:?}");
    assert_eq!(count(root, id, "owner_input"), 1);
    wait("actual submission updates durable message state", || message(&fleet.hub, &mid)["state"] == "submitted");
    // An automatic message beside a draft in Claude's composer leaves the
    // draft exactly as it was, for the owner to submit.
    control(root, "hub", id, json!({"mode":"idle","draft":"owner retained draft","hook":false}));
    let beside = send(&fleet.hub, &FrozenAttentionSend::new(&fleet.hub, "beside-key", id, "beside the owner's draft"));
    wait("automatic message beside the owner's draft", || message(&fleet.hub, &beside)["state"] == "submitted");
    assert_eq!(captures(root, "hub", id)[2]["text"], envelope("beside the owner's draft"));
    let handed = events(root, "hub", id).into_iter().rev().find(|v| v["category"] == "owner_input").unwrap();
    assert_eq!(handed["draft_chars"], "owner retained draft".len());
    owner.send(&json!({"t":"input","id":id,"data":"\r"}));
    wait("the owner submits the retained draft", || captures(root, "hub", id).len() == 4);
    assert_eq!(captures(root, "hub", id)[3]["text"], "owner retained draft");
    assert_eq!(send(&fleet.hub, &custody), mid, "lost reply retry reuses original intent");
    let mut changed = custody.args.clone();
    changed["text"] = json!("changed");
    assert!(call_result(&fleet.hub, "attention-send", &changed).is_err());
    observe(Duration::from_secs(1), || assert_eq!(captures(root, "hub", id).len(), 4));
    assert_eq!(count(root, id, "owner_input"), 2, "each automatic message is handed to Claude once");
    public(&attention(&fleet.hub));
    // Public input counts are intent metadata, never acceptance evidence.
    eprintln!("attention-delivery: owner_submissions=2 automatic_submissions=2 exact_escaped_nonowner=true");
}

#[test]
fn unconfirmed_messages_stay_unknown_and_handover_or_restart_never_hands_them_again() {
    let scratch = Scratch::new("attention-unconfirmed");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let fake = install_fake(root);
    let mut fleet = AttentionFleet::new(root, &binary);
    configure(&fleet.hub, root, &fake, "hub");
    configure(&fleet.peer, root, &fake, "peer");
    fleet.hub.start();
    ready(&fleet.hub);
    let id = start(&fleet.hub, "claude", "@here", &fleet.hub.root.join("work"), "private-unconfirmed");
    wait("hosted initial hook", || session(&fleet.hub, id)["status"] == "finished");
    // This Claude takes the message but never reports what it did with it.
    control(root, "hub", id, json!({"mode":"idle","answer":false}));
    let unconfirmed = FrozenAttentionSend::new(
        &fleet.hub,
        "unconfirmed-key",
        id,
        "ATTENTION_PRIVATE_PAYLOAD handed once and never confirmed",
    );
    let mid = send(&fleet.hub, &unconfirmed);
    wait("Claude's prompt interface was handed the message", || count(root, id, "owner_input") == 1);
    quiet(root, "hub", id, Duration::from_secs(1));
    // Nothing shows whether Claude submitted it: past its deadline it is
    // unknown, and it is never handed again, not even after a handover.
    wait("unconfirmed submission is unknown", || message(&fleet.hub, &mid)["state"] == "unknown");
    handover(&fleet.hub, &binary);
    assert_eq!(send(&fleet.hub, &unconfirmed), mid);
    quiet(root, "hub", id, Duration::from_secs(1));
    assert_eq!(count(root, id, "owner_input"), 1);
    // A separate unattempted held message survives handover and may submit once.
    let second = start(&fleet.hub, "claude", "@here", &fleet.hub.root.join("work"), "private-queued-handover");
    wait("second initial hook", || session(&fleet.hub, second)["status"] == "finished");
    control(root, "hub", second, json!({"mode":"working","hook":false}));
    wait("second working", || session(&fleet.hub, second)["status"] == "working");
    let queued =
        send(&fleet.hub, &FrozenAttentionSend::new(&fleet.hub, "queued-recovery", second, "queued across handover"));
    wait("second durable held", || message(&fleet.hub, &queued)["state"] == "held");
    handover(&fleet.hub, &binary);
    idle(root, second);
    wait("unattempted exact target delivers once after exec", || captures(root, "hub", second).len() == 1);
    assert_eq!(captures(root, "hub", second)[0]["text"], envelope("queued across handover"));
    assert_eq!(message(&fleet.hub, &queued)["state"], "submitted");
    let cold = start(&fleet.hub, "claude", "@here", &fleet.hub.root.join("work"), "private-cold-unknown");
    wait("cold target initially finished", || session(&fleet.hub, cold)["status"] == "finished");
    control(root, "hub", cold, json!({"mode":"idle","answer":false}));
    let cold_request = FrozenAttentionSend::new(&fleet.hub, "cold-unconfirmed", cold, "cold interrupted delivery");
    let cold_message = send(&fleet.hub, &cold_request);
    wait("cold target was handed the message once", || count(root, cold, "owner_input") == 1);
    fleet.hub.crash();
    fleet.hub.start();
    ready(&fleet.hub);
    wait("cold recovery preserves unknown rather than retry", || {
        message(&fleet.hub, &cold_message)["state"] == "unknown"
    });
    assert_eq!(send(&fleet.hub, &cold_request), cold_message);
    quiet(root, "hub", cold, Duration::from_secs(1));
    assert_eq!(count(root, cold, "owner_input"), 1);
    public(&attention(&fleet.hub));
}
