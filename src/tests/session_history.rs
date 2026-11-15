use super::*;
use crate::{
    screen::HISTORY_PROTOCOL,
    service::{self, Ev},
    testsupport::TempDir,
};
use std::sync::mpsc;

fn fixture(version: u32) -> (TempDir, Service, mpsc::Receiver<super::super::Out>) {
    let dir = TempDir::new("session-history-source");
    let (tx, _) = mpsc::channel();
    let mut svc = Service::new(service::tests::options(&dir, "true"), tx);
    let (out, rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 1, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(1, ClientMsg::Hello { version, role: ClientRole::Tui });
    rx.try_iter().for_each(drop);
    (dir, svc, rx)
}

#[test]
fn history_source_versions_are_checked_before_forwarding_any_new_message() {
    let (_dir, mut svc, rx) = fixture(HISTORY_PROTOCOL);
    let config = HostConfig {
        role: Default::default(),
        name: "old".into(),
        tag: 2,
        label: None,
        addresses: vec![],
        ssh: None,
        fingerprint: "a".repeat(64),
    };
    let (out, host_rx) = mpsc::channel();
    svc.hosts.insert(
        2,
        HostSlot {
            config,
            stop: Arc::new(AtomicBool::new(false)),
            out: Some(out),
            welcome: Some(Welcome { version: 17, ..Welcome::default() }),
            state: None,
            reason: None,
            stats: None,
            outage: Default::default(),
            reached: false,
        },
    );
    let id = crate::protocol::host_id(2, 1);
    let request = ClientMsg::History { id, request: 9, epoch: 10, cols: 80, rows: 24 };
    assert!(svc.route_client(1, request).is_none());
    assert!(host_rx.try_recv().is_err(), "old sources never receive an unknown enum");
    let super::super::Out::Msg(bytes) = rx.try_recv().unwrap() else { panic!("history refusal") };
    let msg = crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap().unwrap();
    assert!(matches!(msg, ServerMsg::History { id: got, request: 9, epoch: 10, cols: 80, rows: 24,
        snapshot: None, error: Some(_) } if got == id));

    let (_dir, mut old_view, rx) = fixture(17);
    old_view.handle_client(1, ClientMsg::History { id: 1, request: 2, epoch: 3, cols: 80, rows: 24 });
    let super::super::Out::Msg(bytes) = rx.try_recv().unwrap() else { panic!("legacy notice") };
    assert!(matches!(
        crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap().unwrap(),
        ServerMsg::Notice { error: true, .. }
    ));
}

#[test]
fn session_history_source_reads_styled_rows_and_rejects_stale_epochs_and_sizes() {
    let (dir, mut svc, rx) = fixture(HISTORY_PROTOCOL);
    let (events, _) = mpsc::channel();
    let argv = vec!["sh".into(), "-c".into(), "exit 0".into()];
    let mut hosted = crate::session::Hosted::spawn(
        crate::session::SpawnSpec {
            id: 1,
            argv: &argv,
            cwd: dir.path(),
            env_set: &[],
            env_remove: &[],
            cols: 80,
            rows: 24,
            scrollback: 100,
        },
        events,
    )
    .unwrap();
    hosted.pause().unwrap();
    {
        let mut emu = hosted.lock();
        let crate::session::Emu { term, parser, .. } = &mut *emu;
        parser.advance(term, ("\x1b[31mred\x1b[0m 日e\u{301}\r\n".repeat(40)).as_bytes());
    }
    hosted.scroll(crate::protocol::ScrollOp::Top);
    let epoch = hosted.history_epoch;
    let before = crate::screen::build_frame(&hosted.lock().term);
    let mut session = service::tests::fake_session(1, crate::protocol::Tool::Shell);
    session.hosted = Some(hosted);
    svc.sessions.insert(1, session);
    svc.extensions.metadata.entry(1).or_default().insert("_deck_terminal_epoch".into(), serde_json::json!(0));
    assert_eq!(svc.build_state().sessions[0].metadata["_deck_terminal_epoch"], epoch);
    for (request, requested_epoch, cols) in [(1, epoch, 80), (2, epoch.wrapping_add(1), 80), (3, epoch, 81)] {
        svc.handle_client(1, ClientMsg::History { id: 1, request, epoch: requested_epoch, cols, rows: 24 });
        let super::super::Out::Msg(bytes) = rx.try_recv().unwrap() else { panic!("history reply") };
        let msg = crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap().unwrap();
        if request == 1 {
            let ServerMsg::History { snapshot: Some(snapshot), error: None, .. } = msg else {
                panic!("valid snapshot")
            };
            assert!(snapshot.lines.iter().flatten().any(|run| run.t == "red" && run.fg == crate::screen::indexed(1)));
        } else {
            assert!(matches!(msg, ServerMsg::History { snapshot: None, error: Some(_), .. }));
        }
        assert_eq!(crate::screen::build_frame(&svc.sessions[&1].hosted.as_ref().unwrap().lock().term), before);
    }
    let (out, legacy_rx) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 2, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle_client(2, ClientMsg::Hello { version: 17, role: ClientRole::Tui });
    legacy_rx.try_iter().for_each(drop);
    for id in [1, 2] {
        svc.clients.get_mut(&id).unwrap().attached = Some(1);
    }
    svc.pump_frames();
    for (view, frames) in [(18, &rx), (17, &legacy_rx)] {
        let super::super::Out::Frame(bytes) = frames.try_recv().unwrap() else { panic!("source frame") };
        let ServerMsg::Frame { id: 1, frame } = crate::protocol::read_msg(&mut bytes.as_slice()).unwrap().unwrap()
        else {
            panic!("source frame")
        };
        assert_eq!(frame.epoch, epoch);
        assert_eq!(frame.offset == 0, view == 18, "new views get live frames while legacy views keep their viewport");
    }
    assert_eq!(crate::screen::build_frame(&svc.sessions[&1].hosted.as_ref().unwrap().lock().term), before);
    let (events, _) = mpsc::channel();
    let mut reopened = crate::session::Hosted::spawn(
        crate::session::SpawnSpec {
            id: 1,
            argv: &argv,
            cwd: dir.path(),
            env_set: &[],
            env_remove: &[],
            cols: 80,
            rows: 24,
            scrollback: 100,
        },
        events,
    )
    .unwrap();
    reopened.pause().unwrap();
    assert_ne!(reopened.history_epoch, epoch, "the same numeric slot gets a new terminal incarnation");
    svc.sessions.get_mut(&1).unwrap().hosted = Some(reopened);
    svc.handle_client(1, ClientMsg::History { id: 1, request: 4, epoch, cols: 80, rows: 24 });
    let super::super::Out::Msg(bytes) = rx.try_recv().unwrap() else { panic!("retired terminal refusal") };
    assert!(matches!(
        crate::protocol::read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap().unwrap(),
        ServerMsg::History { request: 4, snapshot: None, error: Some(_), .. }
    ));
}
