use super::*;
use crate::protocol::BranchInfo;
use crate::testsupport::TempDir;

fn states(rx: &mpsc::Receiver<String>) -> Vec<Value> {
    rx.try_iter()
        .map(|text| serde_json::from_str::<Value>(&text).unwrap())
        .filter(|message| message["t"] == "state")
        .collect()
}

#[test]
fn a_patch_reaches_the_phones_only_when_what_they_show_changed() {
    let dir = TempDir::new("hub-state-patch");
    let hub = Hub::new(crate::remote::tests::hub_options(&dir, vec![]), "patch-test".into());
    // A patch before any whole state has nothing to apply to.
    hub.on_service(ServerMsg::StatePatch(StatePatch { start_dir: Some("/early".into()), ..StatePatch::default() }));
    assert!(lock(&hub.inner).state.is_none());
    hub.on_service(ServerMsg::State(DeckState { start_dir: "/a".into(), ..DeckState::default() }));
    let (tx, rx) = mpsc::channel();
    hub.add_conn("phone", tx, None, None);
    assert_eq!(states(&rx).len(), 1, "a phone that connects gets the state");

    // The branch list is not on the phone: nothing is sent, but the state
    // the gateway keeps takes it.
    let branches = vec![BranchInfo { branch: "topic".into(), ..BranchInfo::default() }];
    hub.on_service(ServerMsg::StatePatch(StatePatch { branches: Some(branches.clone()), ..StatePatch::default() }));
    assert!(states(&rx).is_empty(), "an unchanged phone state was sent again");
    assert_eq!(lock(&hub.inner).state.as_ref().unwrap().branches, branches);

    hub.on_service(ServerMsg::StatePatch(StatePatch { start_dir: Some("/b".into()), ..StatePatch::default() }));
    let sent = states(&rx);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["start_dir"], "/b");
    let inner = lock(&hub.inner);
    let state = inner.state.as_ref().unwrap();
    assert_eq!(
        (state.start_dir.as_str(), state.branches.len()),
        ("/b", 1),
        "a patch keeps the parts it does not carry"
    );
}
