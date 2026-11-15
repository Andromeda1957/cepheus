//! Existing direct manual input keeps its intentional steering behavior.
use super::*;
#[test]
fn direct_manual_messages_and_running_broadcast_remain_owner_input() {
    let scratch = Scratch::new("attention-manual");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let fake = install_fake(root);
    let mut fleet = AttentionFleet::new(root, &binary);
    configure(&fleet.hub, root, &fake, "hub");
    configure(&fleet.peer, root, &fake, "peer");
    fleet.hub.start();
    ready(&fleet.hub);
    let first = start(&fleet.hub, "claude", "@here", &fleet.hub.root.join("work"), "private-manual");
    let second = start(&fleet.hub, "claude", "@here", &fleet.hub.root.join("work"), "private-steering");
    wait("manual recipients finished", || {
        [first, second].iter().all(|id| session(&fleet.hub, *id)["status"] == "finished")
    });
    let mut ui = Wire::new(&fleet.hub, "remote", 15);
    ui.send(&json!({"t":"send_message","id":first,"request":77,"text":"direct owner message","attachments":false}));
    wait("actual correlated manual submission", || {
        ui.poll();
        ui.seen.iter().any(|v| v["t"] == "message_sent" && v["request"] == 77 && v["error"].is_null())
    });
    assert_eq!(captures(root, "hub", first)[0]["text"], "direct owner message");
    control(root, "hub", second, json!({"mode":"working","hook":false}));
    wait("actual working broadcast target", || session(&fleet.hub, second)["status"] == "working");
    ui.send(&json!({"t":"broadcast","ids":[second],"running_only":true,"text":"owner steering while working"}));
    wait("manual working steering actual submit", || captures(root, "hub", second).len() == 1);
    let expected="[Owner broadcast to all running agents]\nThis is a shared instruction, not a new task. Apply it only to your own current work. If it concerns behavior you are doing, stop or correct that behavior yourself. Otherwise continue your assigned task without replying to it. Do not acknowledge it, investigate which other agent it concerns, inspect other sessions, or coordinate a response unless this broadcast explicitly asks you to.\n\nOwner message:\nowner steering while working";
    assert_eq!(captures(root, "hub", second)[0]["text"], expected);

    control(root, "hub", second, json!({"mode":"idle"}));
    ui.send(
        &json!({"t":"send_message","id":second,"request":78,"text":"owner multiline\nsecond line","attachments":false}),
    );
    wait("the multiline owner message submitted through Claude's prompt interface", || {
        ui.poll();
        ui.seen.iter().any(|v| v["t"] == "message_sent" && v["request"] == 78 && v["error"].is_null())
    });
    let capture = captures(root, "hub", second);
    assert_eq!(capture.len(), 2, "each owner message is submitted once");
    assert_eq!(capture[1]["text"], "owner multiline\nsecond line", "one message, its lines intact");
    assert!(
        attention(&fleet.hub)["messages"].as_array().unwrap().is_empty(),
        "manual steering does not impersonate automatic queue admission"
    );
}
