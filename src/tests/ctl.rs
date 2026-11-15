use super::*;

#[test]
fn ordinary_ctl_messages_require_native_protocol_and_the_matching_receipt() {
    for version in [18, PROTOCOL_VERSION] {
        let (client, mut server) = UnixStream::pair().unwrap();
        server.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let id = crate::protocol::host_id(2, 100);
        let host = std::thread::spawn(move || {
            assert!(matches!(read_msg::<_, ClientMsg>(&mut server).unwrap(), Some(ClientMsg::Hello { .. })));
            write_msg(&mut server, &ServerMsg::Welcome { version, pid: 42, start_dir: String::new() }).unwrap();
            write_msg(&mut server, &ServerMsg::State(DeckState::default())).unwrap();
            if version >= crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL {
                let Some(ClientMsg::SendMessage { id: target, request, text, attachments: false }) =
                    read_msg(&mut server).unwrap()
                else {
                    panic!("ordinary ctl send must use native correlation")
                };
                assert_eq!(target, id);
                assert_eq!(text, "exact owner text\nsecond line");
                write_msg(
                    &mut server,
                    &ServerMsg::MessageSent { id, request: request + 1, error: None, delivered: None },
                )
                .unwrap();
                write_msg(
                    &mut server,
                    &ServerMsg::MessageSent { id, request, error: Some("native refusal".into()), delivered: None },
                )
                .unwrap();
            }
            assert!(
                read_msg::<_, ClientMsg>(&mut server).unwrap().is_none(),
                "no terminal Input or Screen request follows"
            );
        });
        let mut conn = Conn::over(client).unwrap();
        let error = conn.native_message(id, "exact owner text\nsecond line").unwrap_err().to_string();
        assert!(if version == 18 { error.contains("Update needed") } else { error == "native refusal" }, "{error}");
        drop(conn);
        host.join().unwrap();
    }
}

#[test]
fn agent_messages_count_only_a_currently_linked_host_as_online() {
    use crate::protocol::{HostInfo, Status};
    let local = crate::tui::tests::session(3, Tool::Claude, Status::Idle, true);
    let mut remote = local.clone();
    remote.id = crate::protocol::host_id(1, 27);
    remote.host = Some("fedora".into());
    let fedora = |online| HostInfo { name: "fedora".into(), label: "Fedora".into(), online, ..HostInfo::default() };
    let mut state = DeckState { sessions: vec![local.clone(), remote.clone()], ..DeckState::default() };
    assert!(agent_controls::host_online(&state, &local), "a session on the service itself");
    assert!(!agent_controls::host_online(&state, &remote), "an unknown host is not online");
    state.hosts = vec![fedora(false)];
    assert!(!agent_controls::host_online(&state, &remote), "a host without a link is offline");
    state.hosts = vec![fedora(true)];
    assert!(agent_controls::host_online(&state, &remote));
}

#[test]
fn stop_preserves_a_session_created_after_cli_preflight_without_waiting_for_eof() {
    let (client, server) = UnixStream::pair().unwrap();
    server.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let host = std::thread::spawn(move || {
        let mut reader = BufReader::new(server.try_clone().unwrap());
        assert!(matches!(read_msg::<_, ClientMsg>(&mut reader).unwrap(), Some(ClientMsg::Hello { .. })));
        let mut writer = server;
        write_msg(
            &mut writer,
            &ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: std::process::id(), start_dir: String::new() },
        )
        .unwrap();
        write_msg(&mut writer, &ServerMsg::State(DeckState::default())).unwrap();
        assert!(matches!(read_msg::<_, ClientMsg>(&mut reader).unwrap(), Some(ClientMsg::Shutdown)));
        write_msg(
            &mut writer,
            &ServerMsg::Notice { text: "A session started before shutdown; left running.".into(), error: true },
        )
        .unwrap();
        let _ = read_msg::<_, ClientMsg>(&mut reader);
    });
    let (answer, result) = std::sync::mpsc::channel();
    let command = std::thread::spawn(move || {
        let mut conn = Conn::over(client).unwrap();
        answer.send(stop_connection(&mut conn, true).is_err()).unwrap();
    });
    assert!(result.recv_timeout(crate::testsupport::scaled_timeout(Duration::from_secs(2))).unwrap());
    command.join().unwrap();
    host.join().unwrap();
}

#[test]
fn unescape_expands_terminal_escapes() {
    assert_eq!(unescape(r"hello\r"), "hello\r");
    assert_eq!(unescape(r"a\nb\tc"), "a\nb\tc");
    assert_eq!(unescape(r"\e[13;2u"), "\x1b[13;2u");
    assert_eq!(unescape(r"\x03"), "\x03");
    assert_eq!(unescape(r"back\\slash"), "back\\slash");
}

#[test]
fn unescape_keeps_unknown_and_incomplete_escapes() {
    assert_eq!(unescape(r"\q"), "\\q");
    assert_eq!(unescape(r"\xZZ"), "\\xZZ");
    assert_eq!(unescape("end\\"), "end\\");
    assert_eq!(unescape("日本"), "日本");
}

#[test]
fn own_session_resolution_never_confuses_same_native_ids_on_different_hosts() {
    use crate::protocol::{host_id, Status};
    let mut native = crate::tui::tests::session(3, Tool::Codex, Status::Idle, true);
    native.metadata.insert("fleet-workspace".into(), serde_json::json!({"source":"aa".repeat(32),"session":3}));
    let mut wrong = native.clone();
    wrong.metadata.insert("fleet-workspace".into(), serde_json::json!({"source":"bb".repeat(32),"session":3}));
    let mut correct = native.clone();
    correct.id = host_id(7, 113);
    let mut state = DeckState { sessions: vec![wrong, correct], ..DeckState::default() };
    assert_eq!(shared_self_id(&native, &state).unwrap(), host_id(7, 113));
    state.sessions.pop();
    assert!(shared_self_id(&native, &state).is_err());
}

#[test]
fn slot_values_are_never_reparsed_as_ownership_flags() {
    let args = vec!["--label".into(), "--session".into(), "--pid".into(), "42".into(), "--request-key=abc".into()];
    let options = slot_options(&args).unwrap();
    assert_eq!(options["--label"], "--session");
    assert!(!options.contains_key("--session"));
    assert_eq!(options["--pid"], "42");
    assert_eq!(options["--request-key"], "abc");
    assert!(slot_options(&["--pid".into()]).is_err());
    assert!(slot_options(&["--pid".into(), "42".into(), "--pid".into(), "43".into()]).is_err());
}

#[test]
fn own_session_resolution_uses_exact_codex_thread_when_shared_server_lacks_session_env() {
    use crate::protocol::Status;
    use std::ffi::OsStr;
    let thread = "11111111-2222-3333-4444-555555555555";
    let mut native = crate::tui::tests::session(31, Tool::Codex, Status::Working, true);
    native.conversation = Some(thread.into());
    let mut dead = native.clone();
    dead.id = 32;
    dead.alive = false;
    let mut other_provider = native.clone();
    other_provider.id = 33;
    other_provider.tool = Tool::Claude;
    let mut other_thread = native.clone();
    other_thread.id = 34;
    other_thread.conversation = Some("66666666-7777-8888-9999-000000000000".into());
    let mut state = DeckState { sessions: vec![dead, other_provider, other_thread, native], ..DeckState::default() };
    assert_eq!(native_self_session(&state, None, Some(OsStr::new(thread))).unwrap().id, 31);
    assert_eq!(native_self_session(&state, Some(OsStr::new("34")), Some(OsStr::new(thread))).unwrap().id, 34);
    for explicit in ["", "invalid", "99"] {
        assert!(native_self_session(&state, Some(OsStr::new(explicit)), Some(OsStr::new(thread))).is_err());
    }
    assert!(native_self_session(&state, None, None).is_err());
    assert!(native_self_session(&state, None, Some(OsStr::new(""))).is_err());
    assert!(native_self_session(&state, None, Some(OsStr::new("unknown"))).is_err());
    let mut ambiguous = state.sessions.last().unwrap().clone();
    ambiguous.id = 35;
    state.sessions.push(ambiguous);
    assert!(native_self_session(&state, None, Some(OsStr::new(thread))).is_err());
}
