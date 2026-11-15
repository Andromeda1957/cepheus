use super::*;
use crate::protocol::{ClientMsg, ClientRole, DeckState, ServerMsg, PROTOCOL_VERSION};
use crate::service::{Ev, Out};
use crate::testsupport::TempDir;
use std::sync::{atomic::AtomicBool, mpsc, Arc};

fn service(dir: &TempDir) -> Service {
    let (tx, _) = mpsc::channel();
    Service::new(crate::service::tests::options(dir, "true"), tx)
}

/// What a client was sent, in order.
fn sent(rx: &mpsc::Receiver<Out>) -> Vec<ServerMsg> {
    rx.try_iter()
        .filter_map(|out| match out {
            Out::Msg(bytes) => crate::protocol::read_msg(&mut bytes.as_slice()).unwrap(),
            _ => None,
        })
        .collect()
}

/// Whether `msg` was a deck state, which the client's `state` then takes on:
/// a protocol-20 client is sent the whole state once, then patches of it.
fn take_state(state: &mut DeckState, msg: &ServerMsg) -> bool {
    match msg {
        ServerMsg::State(whole) => *state = whole.clone(),
        ServerMsg::StatePatch(patch) => state.apply(patch.clone()),
        _ => return false,
    }
    true
}

#[test]
fn clear_errors_are_published_to_every_client_before_ack_and_new_alerts() {
    let dir = TempDir::new("clear-ack-order");
    let mut svc = service(&dir);
    let mut receivers = Vec::new();
    for id in 1..=2 {
        let (out, rx) = mpsc::channel();
        svc.handle(Ev::ClientNew { id, out, frame_busy: Arc::new(AtomicBool::new(false)) });
        svc.handle_client(id, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Remote });
        receivers.push(rx);
    }
    svc.announce_error("old error");
    let held: Vec<DeckState> = receivers
        .iter()
        .map(|rx| {
            let mut state = DeckState::default();
            for msg in sent(rx) {
                take_state(&mut state, &msg);
            }
            state
        })
        .collect();
    svc.handle_client(1, ClientMsg::ClearErrors);
    svc.announce_error("new error");
    for (index, (rx, mut state)) in receivers.into_iter().zip(held).enumerate() {
        let messages = sent(&rx);
        let clear = messages
            .iter()
            .position(|msg| take_state(&mut state, msg) && state.errors.is_empty() && !state.error_clears.is_empty())
            .expect("every client gets the clear");
        assert!(
            !messages.iter().any(|msg| matches!(msg, ServerMsg::Alert(alert) if alert.reason == "old error")),
            "a collected error must not reappear after Clear"
        );
        let fresh = messages
            .iter()
            .position(|msg| {
                matches!(msg, ServerMsg::Alert(alert)
            if alert.reason == "new error")
            })
            .expect("new errors are still announced");
        assert!(clear < fresh, "the marker retires only notifications preceding Clear");
        if index == 0 {
            let ack = messages.iter().position(|msg| matches!(msg, ServerMsg::ErrorsCleared)).unwrap();
            assert!(clear < ack, "phone success must follow authoritative state publication");
        }
    }
    // The version-4 request remains usable without the version-14 acknowledgment.
    for (index, version) in [4, 12, 13].into_iter().enumerate() {
        let id = 3 + index as u64;
        let (out, rx) = mpsc::channel();
        svc.handle(Ev::ClientNew { id, out, frame_busy: Arc::new(AtomicBool::new(false)) });
        svc.handle_client(id, ClientMsg::Hello { version, role: ClientRole::Remote });
        svc.log_error("legacy clear still works");
        rx.try_iter().for_each(drop);
        svc.handle_client(id, ClientMsg::ClearErrors);
        let messages: Vec<ServerMsg> = rx
            .try_iter()
            .filter_map(|out| match out {
                Out::Msg(bytes) => crate::protocol::read_msg(&mut bytes.as_slice()).unwrap(),
                _ => None,
            })
            .collect();
        assert!(messages.iter().any(|msg| matches!(msg, ServerMsg::State(state) if state.errors.is_empty())));
        assert!(
            !messages.iter().any(|msg| matches!(msg, ServerMsg::ErrorsCleared)),
            "protocol {version} cannot read the new acknowledgment"
        );
    }
}

#[test]
fn clear_errors_from_independent_offline_hosts_merge_without_clearing_again_on_echo() {
    let left_dir = TempDir::new("clear-left");
    let right_dir = TempDir::new("clear-right");
    let mut left = service(&left_dir);
    let mut right = service(&right_dir);
    left.handle_client(0, ClientMsg::ClearErrors);
    right.handle_client(0, ClientMsg::ClearErrors);
    let left_clear = left.error_clears.clone();
    let right_clear = right.error_clears.clone();
    left.handle_client(0, ClientMsg::ClearErrorsThrough { clears: right_clear });
    right.handle_client(0, ClientMsg::ClearErrorsThrough { clears: left_clear });
    assert_eq!(left.error_clears, right.error_clears);
    assert_eq!(left.error_clears.len(), 2);
    left.log_error("new error");
    left.handle_client(0, ClientMsg::ClearErrorsThrough { clears: right.error_clears.clone() });
    assert_eq!(left.build_state().errors.len(), 1, "replayed clear events do not hide new occurrences");
}

#[test]
fn an_error_that_keeps_happening_alerts_the_phone_once_until_it_is_cleared() {
    // Owner, 2026-11: "non stop errors that i shouldnt be getting". The
    // same failure again is one F2 entry with its count, and one alert.
    let dir = TempDir::new("clear-error-repeats");
    let mut svc = service(&dir);
    let (out, rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 2, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(2, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Remote });
    let alerts = || -> Vec<String> {
        rx.try_iter()
            .filter_map(|out| match out {
                Out::Msg(bytes) => match crate::protocol::read_msg(&mut bytes.as_slice()) {
                    Ok(Some(ServerMsg::Alert(alert))) if alert.kind == crate::protocol::AlertKind::Error => {
                        Some(alert.reason)
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    let gave_up = "Codex #3 gave up after 5 tries";
    for _ in 0..5 {
        svc.announce_error(gave_up);
    }
    assert_eq!(alerts(), vec![gave_up.to_string()]);
    let errors = svc.build_state().errors;
    assert_eq!(errors.len(), 1, "one entry: {errors:?}");
    assert_eq!(errors[0].repeats, 4);
    svc.announce_error("Dev server failed to start: no such file");
    svc.announce_error(gave_up);
    assert_eq!(alerts(), vec!["Dev server failed to start: no such file".to_string()], "only what is new");
    assert_eq!(svc.build_state().errors.last().map(|e| e.text.clone()), Some(gave_up.to_string()), "latest last");
    assert!(svc.clear_errors(None));
    assert!(svc.build_state().errors.is_empty(), "a cleared error stays cleared");
    svc.announce_error(gave_up);
    assert_eq!(alerts(), vec![gave_up.to_string()], "after Clear, it happening again is news");
}
