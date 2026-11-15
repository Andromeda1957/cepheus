use super::*;

use crate::protocol::read_msg;

#[test]
fn the_main_connection_uses_the_hosts_protocol_while_it_updates() {
    for version in [PROTOCOL_VERSION - 2, PROTOCOL_VERSION - 1, PROTOCOL_VERSION] {
        let (hub, mut host) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (tx, rx) = mpsc::channel();
        let connection = thread::spawn(move || serve_main(1, hub, Welcome { version, ..Welcome::default() }, &tx));
        let hello = read_msg::<_, ClientMsg>(&mut host).unwrap().unwrap();
        let _ = host.shutdown(Shutdown::Both);
        connection.join().unwrap();
        drop(rx);
        assert!(matches!(hello, ClientMsg::Hello { version: v, role: ClientRole::Peer } if v == version), "{hello:?}");
    }
}

#[test]
fn client_upstream_preserves_legacy_view_protocol_behind_a_new_hub() {
    for (source, view, expected) in [(18, 17, 17), (17, 18, 17), (18, 18, 18)] {
        let (hub, mut host) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (tx, _) = mpsc::channel();
        let (out, outbound) = mpsc::channel();
        let connection = thread::spawn(move || {
            serve_upstream(
                (1, 2),
                hub,
                Welcome { version: source, ..Welcome::default() },
                (ClientRole::Tui, view),
                &tx,
                outbound,
            )
        });
        let hello = read_msg::<_, ClientMsg>(&mut host).unwrap().unwrap();
        let _ = host.shutdown(Shutdown::Both);
        drop(out);
        connection.join().unwrap();
        assert!(
            matches!(hello, ClientMsg::Hello { version, role: ClientRole::Tui } if version == expected),
            "{hello:?}"
        );
    }
}

#[test]
fn history_is_refused_if_the_actual_source_connection_negotiates_an_older_build() {
    let (hub, mut host) = UnixStream::pair().unwrap();
    host.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let (tx, events) = mpsc::channel();
    let (out, outbound) = mpsc::channel();
    let connection = thread::spawn(move || {
        serve_upstream((1, 2), hub, Welcome { version: 17, ..Welcome::default() }, (ClientRole::Tui, 18), &tx, outbound)
    });
    assert!(matches!(read_msg::<_, ClientMsg>(&mut host).unwrap().unwrap(), ClientMsg::Hello { version: 17, .. }));
    out.send(crate::protocol::encode(&ClientMsg::History { id: 1, request: 7, epoch: 9, cols: 80, rows: 24 })).unwrap();
    out.send(crate::protocol::encode(&ClientMsg::Focus { focused: false })).unwrap();
    assert_eq!(
        read_msg::<_, ClientMsg>(&mut host).unwrap().unwrap(),
        ClientMsg::Focus { focused: false },
        "the old source receives the next compatible message, never History"
    );
    let Ev::Host(event) = events.recv_timeout(Duration::from_secs(10)).unwrap() else { panic!("source refusal") };
    assert!(matches!(
        *event,
        HostEv::UpMsg {
            cid: 1,
            tag: 2,
            msg: ServerMsg::History { id: 1, request: 7, epoch: 9, cols: 80, rows: 24, snapshot: None, error: Some(_) }
        }
    ));
    let _ = host.shutdown(Shutdown::Both);
    drop(out);
    connection.join().unwrap();
}

/// The owner's Stop reaches a source that has it as itself, and an older
/// source as the Esc it took before.
#[test]
fn the_owners_stop_reaches_an_older_source_as_its_esc() {
    for (source, expected) in [
        (crate::protocol::NATIVE_STOP_PROTOCOL, ClientMsg::Stop { id: 100 }),
        (crate::protocol::NATIVE_STOP_PROTOCOL - 1, ClientMsg::Input { id: 100, data: "\x1b".into() }),
    ] {
        let (hub, mut host) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (tx, _events) = mpsc::channel();
        let (out, outbound) = mpsc::channel();
        let connection = thread::spawn(move || {
            let welcome = Welcome { version: source, ..Welcome::default() };
            serve_upstream((7, 2), hub, welcome, (ClientRole::Remote, crate::protocol::PROTOCOL_VERSION), &tx, outbound)
        });
        assert!(matches!(read_msg::<_, ClientMsg>(&mut host).unwrap().unwrap(), ClientMsg::Hello { .. }));
        out.send(crate::protocol::encode(&ClientMsg::Stop { id: 100 })).unwrap();
        out.send(crate::protocol::encode(&ClientMsg::Focus { focused: false })).unwrap();
        assert_eq!(read_msg::<_, ClientMsg>(&mut host).unwrap().unwrap(), expected, "source {source}");
        assert_eq!(read_msg::<_, ClientMsg>(&mut host).unwrap().unwrap(), ClientMsg::Focus { focused: false });
        let _ = host.shutdown(Shutdown::Both);
        drop(out);
        connection.join().unwrap();
    }
}

#[test]
fn native_owner_messages_follow_the_actual_upstream_protocol_without_terminal_downgrade() {
    for (source, client) in [(18, 19), (19, 18), (19, 19)] {
        let (hub, mut host) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (tx, events) = mpsc::channel();
        let (out, outbound) = mpsc::channel();
        let connection = thread::spawn(move || {
            serve_upstream(
                (7, 2),
                hub,
                Welcome { version: source, ..Welcome::default() },
                (ClientRole::Control, client),
                &tx,
                outbound,
            )
        });
        assert!(
            matches!(read_msg::<_, ClientMsg>(&mut host).unwrap().unwrap(), ClientMsg::Hello { version, .. } if version == source.min(client))
        );
        let messages = [
            ClientMsg::SendMessage { id: 100, request: 301, text: "owner text".into(), attachments: false },
            ClientMsg::SendNow { id: 100 },
            ClientMsg::Broadcast { ids: vec![100], text: "owner text".into(), running_only: false },
            ClientMsg::BroadcastPart { request: 302, ids: Some(vec![100, 101]), text: "owner text".into() },
            ClientMsg::BroadcastPart { request: 303, ids: None, text: "owner text".into() },
            ClientMsg::Plugin {
                request: 304,
                plugin: "provider-codex".into(),
                command: "target".into(),
                args: serde_json::json!({"id":100,"operation":"send","text":"owner text"}),
            },
            ClientMsg::Plugin {
                request: 305,
                plugin: "provider-codex".into(),
                command: "target".into(),
                args: serde_json::json!({"operation":"broadcast","text":"owner text"}),
            },
            ClientMsg::Plugin {
                request: 306,
                plugin: "provider-codex".into(),
                command: "control".into(),
                args: serde_json::json!({"id":100,"action":"settings"}),
            },
            ClientMsg::Plugin {
                request: 307,
                plugin: "provider-dot".into(),
                command: "send".into(),
                args: serde_json::json!({"id":100,"text":"owner text"}),
            },
            ClientMsg::Create {
                tool: crate::protocol::Tool::Codex,
                model: Some("gpt-6.1-sol".into()),
                effort: Some("high".into()),
                host: None,
                cwd: None,
                command: vec![],
                name: None,
                request: Some(308),
            },
        ];
        for message in &messages {
            out.send(crate::protocol::encode(message)).unwrap();
        }
        let raw = ClientMsg::Input { id: 100, data: "explicit raw keyboard".into() };
        out.send(crate::protocol::encode(&raw)).unwrap();
        if source.min(client) >= crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL {
            for message in messages {
                assert_eq!(read_msg::<_, ClientMsg>(&mut host).unwrap().unwrap(), message);
            }
            assert!(events.try_recv().is_err());
        } else {
            for index in 0..messages.len() {
                let Ev::Host(event) = events.recv_timeout(Duration::from_secs(10)).unwrap() else {
                    panic!("source refusal")
                };
                let HostEv::UpMsg { cid: 7, tag: 2, msg } = *event else { panic!("correlated upstream refusal") };
                let error = match (index, msg) {
                    (0, ServerMsg::MessageSent { id: 100, request: 301, error: Some(error), .. })
                    | (1, ServerMsg::SentNow { id: 100, error: Some(error) })
                    | (2 | 9, ServerMsg::Notice { error: true, text: error }) => error,
                    (3 | 4, ServerMsg::BroadcastDone { request, sent, failed }) => {
                        assert_eq!(request, if index == 3 { 302 } else { 303 });
                        assert!(sent.is_empty());
                        assert_eq!(
                            failed.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                            if index == 3 { vec![100, 101] } else { vec![0] }
                        );
                        failed[0].1.clone()
                    }
                    (5..=8, ServerMsg::PluginResult { request, error: Some(error), .. }) => {
                        assert_eq!(request, index as u64 + 299);
                        error
                    }
                    (_, msg) => panic!("unexpected refusal: {msg:?}"),
                };
                assert!(error.contains("protocol 19"));
            }
        }
        assert_eq!(
            read_msg::<_, ClientMsg>(&mut host).unwrap().unwrap(),
            raw,
            "old connections receive only explicitly requested raw keys"
        );
        let _ = host.shutdown(Shutdown::Both);
        drop(out);
        connection.join().unwrap();
    }
}
