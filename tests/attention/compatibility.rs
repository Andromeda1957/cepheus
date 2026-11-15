//! Old authenticated clients get an explicit update error for new controls.
use super::*;
#[test]
fn protocol14_preserves_jobs_queue_and_explicitly_declines_attention() {
    let scratch = Scratch::new("attention-old-client");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let fake = install_fake(root);
    let mut fleet = AttentionFleet::new(root, &binary);
    configure(&fleet.hub, root, &fake, "hub");
    configure(&fleet.peer, root, &fake, "peer");
    fleet.hub.start();
    ready(&fleet.hub);
    let mut old = Wire::new(&fleet.hub, "control", 14);
    let jobs = old.fleet("job-status", json!({}));
    assert!(jobs["error"].is_null());
    assert!(jobs["data"]["jobs"].is_array());
    let queue = old.fleet("queue-list", json!({}));
    assert!(queue["error"].is_null());
    assert!(queue["data"]["items"].is_array());
    let attention = old.fleet("attention-status", json!({}));
    let error = attention["error"].as_str().expect("explicit old-client error").to_lowercase();
    assert!(error.contains("update") && error.contains("15"), "precise update-needed evidence: {attention}");
    let off = old.fleet("attention-supervisor-set", json!({"enabled":true}));
    assert!(off["error"].is_string());
    assert_eq!(super::support::attention(&fleet.hub)["supervisor"]["enabled"], false);
}
