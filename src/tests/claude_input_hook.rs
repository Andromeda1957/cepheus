use super::*;
use std::os::unix::net::UnixStream;

/// The lines `serve` prints for what the service sends, and the error it
/// ends with, after it announced the listener as `stop` says.
fn served(stop: bool, sent: Vec<ServerMsg>) -> (Vec<serde_json::Value>, String) {
    let (client, mut service) = UnixStream::pair().unwrap();
    service.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let host = std::thread::spawn(move || {
        let listen: Option<ClientMsg> = read_msg(&mut service).unwrap();
        assert_eq!(listen, Some(ClientMsg::OwnerInputListen { conversation: "conversation-1".into(), stop }));
        write_msg(&mut service, &ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: String::new() })
            .unwrap();
        for msg in sent {
            write_msg(&mut service, &msg).unwrap();
        }
        let refusal =
            ServerMsg::Notice { text: "another owner input listener serves this session now".into(), error: true };
        write_msg(&mut service, &refusal).unwrap();
    });
    let mut out = Vec::new();
    let error = serve(client, "conversation-1", stop, &mut out).unwrap_err().to_string();
    host.join().unwrap();
    let lines = String::from_utf8(out).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    (lines, error)
}

fn input(text: &str, files: &[&str]) -> ServerMsg {
    owner_input(text, files, false)
}

fn owner_input(text: &str, files: &[&str], steer: bool) -> ServerMsg {
    ServerMsg::OwnerInput {
        message: format!("message-{}", text.len()),
        conversation: "conversation-1".into(),
        text: text.into(),
        files: files.iter().map(|f| f.to_string()).collect(),
        steer,
    }
}

#[test]
fn the_listener_prints_each_owner_message_as_one_line_until_the_service_refuses_it() {
    // A broadcast's line asks the plugin to deliver it into the running turn.
    let sent = vec![input("first line\nsecond line", &[]), input("two", &[]), owner_input("all agents", &[], true)];
    let (lines, error) = served(false, sent);
    assert_eq!(error, "another owner input listener serves this session now");
    assert_eq!(
        lines,
        vec![
            serde_json::json!({"message":"message-22","conversation":"conversation-1","text":"first line\nsecond line","files":[]}),
            serde_json::json!({"message":"message-3","conversation":"conversation-1","text":"two","files":[]}),
            serde_json::json!({"message":"message-10","conversation":"conversation-1","text":"all agents","files":[],"steer":true}),
        ]
    );

    // A connection the service closes is left for the next connection.
    let (client, service) = UnixStream::pair().unwrap();
    drop(service);
    let mut out = Vec::new();
    serve(client, "conversation-1", false, &mut out).unwrap();
    assert!(out.is_empty());
}

/// A plugin that ends turns says so when it listens, and is handed the
/// owner's Stop; one that does not is never handed one. Files the host
/// checked go with their message.
#[test]
fn a_stopping_listener_prints_each_stop_and_every_listener_prints_the_files() {
    let file = "/state/uploads/7/shot.png";
    let sent = || vec![input(&format!("look\n\n{file}"), &[file]), ServerMsg::OwnerStop { message: "stop-1".into() }];
    let (lines, _) = served(true, sent());
    assert_eq!(
        lines,
        vec![
            serde_json::json!({"message":"message-31","conversation":"conversation-1","text":format!("look\n\n{file}"),"files":[file]}),
            serde_json::json!({"message":"stop-1","stop":true}),
        ]
    );
    let (lines, _) = served(false, sent());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["files"], serde_json::json!([file]));
}

#[test]
fn a_receipt_reaches_the_service_exactly_as_the_plugin_wrote_it_and_waits_for_its_answer() {
    for (input, expected, answer) in [
        (
            r#"{"message":"m-1","outcome":"submitted"}"#,
            ClientMsg::OwnerInputReceipt { message: "m-1".into(), outcome: OwnerInputOutcome::Submitted, reason: None },
            ServerMsg::Notice { text: String::new(), error: false },
        ),
        (
            r#"{"message":"m-2","outcome":"dropped","reason":"a Stop stands"}"#,
            ClientMsg::OwnerInputReceipt {
                message: "m-2".into(),
                outcome: OwnerInputOutcome::Dropped,
                reason: Some("a Stop stands".into()),
            },
            ServerMsg::Notice { text: "the receipt is not from the message's session".into(), error: true },
        ),
    ] {
        let (client, mut service) = UnixStream::pair().unwrap();
        service.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let refused = matches!(answer, ServerMsg::Notice { error: true, .. });
        let host = std::thread::spawn(move || {
            assert_eq!(read_msg::<_, ClientMsg>(&mut service).unwrap(), Some(expected));
            write_msg(
                &mut service,
                &ServerMsg::Welcome { version: PROTOCOL_VERSION, pid: 1, start_dir: String::new() },
            )
            .unwrap();
            write_msg(&mut service, &answer).unwrap();
        });
        let result = report(client, input);
        host.join().unwrap();
        assert_eq!(result.is_err(), refused, "{result:?}");
    }
    for invalid in [
        r#"{"message":"m","outcome":"ran"}"#,
        r#"{"message":"m","outcome":"started","extra":1}"#,
        r#"{"outcome":"started"}"#,
    ] {
        let (client, service) = UnixStream::pair().unwrap();
        let error = report(client, invalid).unwrap_err().to_string();
        assert_eq!(error, "invalid receipt");
        drop(service);
    }
}
