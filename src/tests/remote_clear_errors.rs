use super::*;
use crate::protocol::{Alert, AlertKind, LogEntry};
use crate::testsupport::TempDir;

fn error_alert(reason: &str) -> ServerMsg {
    ServerMsg::Alert(Alert {
        kind: AlertKind::Error,
        id: None,
        title: "error".into(),
        reason: reason.into(),
        desk_focused: false,
        message: None,
    })
}

#[test]
fn cleared_errors_do_not_replay_as_phone_notifications_but_new_errors_do() {
    let dir = TempDir::new("clear-phone-errors");
    let hub = Hub::new(crate::remote::tests::hub_options(&dir, vec![]), "clear-test".into());
    hub.on_service(ServerMsg::State(DeckState { errors: vec![LogEntry::new(10, "old")], ..DeckState::default() }));
    hub.on_service(error_alert("old"));
    hub.on_service(ServerMsg::State(DeckState {
        error_clears: [("a".repeat(64), 1)].into_iter().collect(),
        ..DeckState::default()
    }));
    hub.on_service(error_alert("new"));
    let (tx, rx) = mpsc::channel();
    hub.add_conn("phone", tx, None, None);
    let messages: Vec<Value> = rx.try_iter().map(|text| serde_json::from_str(&text).unwrap()).collect();
    assert!(messages.iter().any(|message| message["t"] == "state" && message["errors"] == json!([])));
    let alerts: Vec<_> = messages.iter().filter(|message| message["t"] == "alert").collect();
    assert_eq!(alerts.len(), 1, "a reconnect must replay only the new error");
    assert_eq!(alerts[0]["reason"], "new");
}

#[test]
fn a_withdrawn_error_is_not_replayed_but_one_not_yet_listed_is() {
    // A login warning a renewal answered, or a host's failed sign-in copy a
    // later copy put right, is taken back from the list; a phone that
    // connects later is not alerted to it (2026-11-12: "windows still
    // reports login issue" after the renewed login reached every host).
    let dir = TempDir::new("withdrawn-phone-errors");
    let hub = Hub::new(crate::remote::tests::hub_options(&dir, vec![]), "withdraw-test".into());
    let listing = |texts: &[&str]| {
        ServerMsg::State(DeckState {
            errors: texts.iter().map(|text| LogEntry::new(10, *text)).collect(),
            ..DeckState::default()
        })
    };
    hub.on_service(listing(&["renewal soon", "fedora: copy failed", "kept"]));
    for reason in ["renewal soon", "copy failed", "kept", "fresh"] {
        hub.on_service(error_alert(reason));
    }
    hub.on_service(listing(&["kept"]));
    let (tx, rx) = mpsc::channel();
    hub.add_conn("phone", tx, None, None);
    let reasons: Vec<String> = rx
        .try_iter()
        .map(|text| serde_json::from_str::<Value>(&text).unwrap())
        .filter(|message| message["t"] == "alert")
        .map(|message| message["reason"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(reasons, ["kept", "fresh"], "an error its state does not list yet still replays");
}
