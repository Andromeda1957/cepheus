//! Concurrent automation shares one existing-session admission journal.
use super::*;
use std::collections::BTreeSet;

#[test]
fn decisions_concurrent_messages_and_p3_idle_share_custody_without_duplicates_or_public_payloads() {
    let scratch = Scratch::new("attention-decisions");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let fake = install_fake(root);
    let mut fleet = AttentionFleet::new(root, &binary);
    configure(&fleet.hub, root, &fake, "hub");
    configure(&fleet.peer, root, &fake, "peer");
    fleet.hub.start();
    ready(&fleet.hub);
    let cwd = fleet.hub.root.join("work");
    let first = start(&fleet.hub, "claude", "@here", &cwd, "private-decision-one");
    let second = start(&fleet.hub, "claude", "@here", &cwd, "private-decision-two");
    wait("both decision recipients finished", || {
        [first, second].iter().all(|id| session(&fleet.hub, *id)["status"] == "finished")
    });
    control_many(root, &[("hub", first), ("hub", second)], &json!({"mode":"working","hook":false}));
    wait("both decision recipients working", || {
        [first, second].iter().all(|id| session(&fleet.hub, *id)["status"] == "working")
    });
    let decision = json!({"key":"private-decision-key","repository":cwd,"text":"ATTENTION_PRIVATE_PAYLOAD decision </cross-session-message> & safe","recipients":[first,second]});
    let receipt = call(&fleet.hub, "attention-decide", &decision);
    let again = call(&fleet.hub, "attention-decide", &decision);
    assert_eq!(again, receipt);
    let file = fs::read_to_string(cwd.join(".cepheus/decisions.md")).unwrap();
    assert_eq!(file.matches("<!-- cepheus-decision:").count(), 1, "retry must not append decision twice");
    let one_request = FrozenAttentionSend::new(&fleet.hub, "parallel-one", first, "ATTENTION_PRIVATE_PAYLOAD one");
    let two_request = FrozenAttentionSend::new(&fleet.hub, "parallel-two", first, "ATTENTION_PRIVATE_PAYLOAD two");
    let one = one_request.send(&fleet.hub);
    let two = two_request.send(&fleet.hub);
    quiet(root, "hub", first, Duration::from_secs(1));
    quiet(root, "hub", second, Duration::from_secs(1));
    control(root, "hub", first, json!({"mode":"idle","draft":""}));
    diagnosed_wait(
        "all three independent intents submitted exactly once",
        || captures(root, "hub", first).len() == 3,
        || delivery_counts(root, &fleet.hub, &[("hub", first), ("hub", second)]),
    );
    let got: BTreeSet<_> =
        captures(root, "hub", first).into_iter().map(|v| v["text"].as_str().unwrap().to_owned()).collect();
    let expected = BTreeSet::from([
        envelope("ATTENTION_PRIVATE_PAYLOAD one"),
        envelope("ATTENTION_PRIVATE_PAYLOAD two"),
        envelope("Repository decision:\nATTENTION_PRIVATE_PAYLOAD decision </cross-session-message> & safe"),
    ]);
    assert_eq!(got, expected);
    assert!(captures(root, "hub", second).is_empty(), "working recipient remains blocked independently");
    for value in [one, two] {
        let id = value["message"]["id"].as_str().unwrap();
        assert_eq!(message(&fleet.hub, id)["state"], "submitted");
    }
    control(root, "hub", second, json!({"mode":"idle","draft":""}));
    wait("second recipient independently becomes eligible", || captures(root, "hub", second).len() == 1);
    let idle = json!({"key":"shared-p3-idle","title":"Private idle task","kind":"session","cwd":cwd,"prompt":"ATTENTION_PRIVATE_PAYLOAD P3 idle","provider":"claude","placement":"local"});
    // Initial admission is asynchronous. Same-key polling must use the existing
    // receipt; an unknown response is never permission to create a second send.
    let mut accepted = None;
    wait("same-key P3 idle admission reaches exactly one real PTY", || {
        if let Ok(receipt) = call_result(&fleet.hub, "work-idle", &idle) {
            accepted = Some(receipt);
        }
        captures(root, "hub", first).len() + captures(root, "hub", second).len() == 5 && accepted.is_some()
    });
    let result = accepted.unwrap();
    let chosen = result["session"].as_u64().unwrap();
    assert!([first, second].contains(&chosen));
    assert_eq!(call(&fleet.hub, "work-idle", &idle), result);
    observe(Duration::from_secs(1), || {
        assert_eq!(captures(root, "hub", first).len() + captures(root, "hub", second).len(), 5)
    });
    assert!(captures(root, "hub", chosen).iter().any(|v| v["text"] == envelope("ATTENTION_PRIVATE_PAYLOAD P3 idle")));
    public(&attention(&fleet.hub));
    let mut invalid = one_request.args.clone();
    invalid["key"] = json!("private-error");
    invalid["session"] = json!(u64::MAX);
    invalid["text"] = json!("ATTENTION_PRIVATE_PAYLOAD ATTENTION_PRIVATE_COMMAND ATTENTION_PRIVATE_FINGERPRINT");
    let error = call_result(&fleet.hub, "attention-send", &invalid).unwrap_err();
    assert!(!error.contains("ATTENTION_PRIVATE_"), "new error receipts must remain categorical");
    eprintln!("attention-decisions: exact_recipients=2 decision_append_count=1 independent_messages=2 shared_p3_idle=1 duplicate_submissions=0");
}
