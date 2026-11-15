use super::*;
use crate::service::native_message_tests::{fixture, until};
use crate::testsupport::TempDir;
use serde_json::json;

fn expected(service: &Service, id: u64) -> ExpectedTarget {
    let session = &service.sessions[&id];
    ExpectedTarget {
        spawned_ms: session.spawned_ms,
        pid: session.hosted.as_ref().and_then(|hosted| hosted.pid),
        conversation: session.conversation.clone(),
    }
}

fn clear(service: &mut Service, request: u64, plugin: &str, id: u64) {
    let args = json!({"id":id,"expected":expected(service, id),"operation":"clear"});
    service.handle_plugin(7, request, plugin, "target", &args);
}

fn results(replies: &Receiver<Out>) -> Vec<ServerMsg> {
    replies
        .try_iter()
        .filter_map(|out| match out {
            Out::Msg(bytes) => read_msg(&mut bytes.as_slice()).unwrap(),
            _ => None,
        })
        .collect()
}

fn answer(messages: &[ServerMsg], request: u64) -> Option<(Value, Option<String>)> {
    messages.iter().find_map(|message| match message {
        ServerMsg::PluginResult { request: got, data, error } if *got == request => Some((data.clone(), error.clone())),
        _ => None,
    })
}

// Owner, 2026-11-03: "i want a button for clear under the 3 dots so i dont
// have to type out / clear if i dont want to".
#[test]
fn a_phone_clear_opens_a_fresh_claude_in_place_of_the_cleared_one() {
    let dir = TempDir::new("native-clear");
    let (tx, _events) = mpsc::channel();
    let mut service = Service::new(crate::service::tests::options(&dir, "exec sleep 30"), tx);
    let (out, replies) = mpsc::channel();
    service.handle(Ev::ClientNew { id: 7, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    let start = StartModel { model: Some("opus".into()), effort: Some("high".into()) };
    let old = service
        .create_with(Tool::Claude, None, &start, &Placement { cwd: Some(dir.path().into()), ..Placement::default() })
        .unwrap();
    let inputs = service.sessions[&old].inputs;

    service.sessions.get_mut(&old).unwrap().tracker.status = Status::Working;
    clear(&mut service, 1, "provider-claude", old);
    let (_, error) = answer(&results(&replies), 1).expect("a busy session answers the clear");
    assert!(error.is_some_and(|error| error.contains("nothing was cleared")));
    assert_eq!(service.sessions.len(), 1);
    assert!(service.sessions[&old].closing.is_none());

    service.sessions.get_mut(&old).unwrap().tracker.status = Status::Idle;
    clear(&mut service, 2, "provider-claude", old);
    let (data, error) = answer(&results(&replies), 2).expect("the clear is answered once settled");
    assert!(error.is_none(), "{error:?}");
    let fresh = data["id"].as_u64().unwrap();
    assert_ne!(fresh, old);
    let session = &service.sessions[&fresh];
    assert!(session.alive && session.tool == Tool::Claude);
    assert!(session.cwd.is_some() && session.cwd == service.sessions[&old].cwd);
    let entry = service.registry.sessions.iter().find(|entry| entry.id == fresh).unwrap();
    assert_eq!((entry.start_model.as_deref(), entry.start_effort.as_deref()), (Some("opus"), Some("high")));
    let cleared = &service.sessions[&old];
    assert!(cleared.remove_when_ended && cleared.closing.is_some(), "the cleared session closes");
    assert_eq!(cleared.inputs, inputs, "nothing is typed into the cleared session");
    assert!(service.native_clears.is_empty());
    service.begin_shutdown(false);
}

#[test]
fn a_phone_clear_opens_a_native_codex_thread_and_answers_its_failures() {
    let (dir, mut service, events, replies, old, _server) = fixture();
    // Native New needs a protocol-19 client; the clear says so, not a notice.
    clear(&mut service, 1, "provider-codex", old);
    let messages = results(&replies);
    let (_, error) = answer(&messages, 1).expect("a refused native New answers the clear");
    assert!(error.is_some_and(|error| error.contains("protocol-19") && error.contains("nothing was cleared")));
    assert!(!messages.iter().any(|message| matches!(message, ServerMsg::Notice { error: true, .. })));
    assert!(service.native_clears.is_empty() && service.sessions[&old].closing.is_none());

    service.handle_client(7, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
    let conversation = service.sessions[&old].conversation.clone();
    let inputs = service.sessions[&old].inputs;
    // The fake Codex appends one line for each launch once it runs.
    let recorded = || fs::read_to_string(dir.path().join("launches")).unwrap_or_default().matches('\n').count();
    let launched = recorded();
    clear(&mut service, 2, "provider-codex", old);
    let mut answered = None;
    until(&mut service, &events, |_| {
        answered = answered.take().or_else(|| answer(&results(&replies), 2));
        answered.is_some()
    });
    let (data, error) = answered.unwrap();
    assert!(error.is_none(), "{error:?}");
    let fresh = data["id"].as_u64().unwrap();
    let session = &service.sessions[&fresh];
    assert!(session.alive && session.tool == Tool::Codex);
    assert!(session.conversation.is_some() && session.conversation != conversation, "a new native thread");
    let thread = session.conversation.clone().unwrap();
    let cleared = &service.sessions[&old];
    assert!(cleared.remove_when_ended, "the cleared session closes");
    assert_eq!(cleared.inputs, inputs);
    // The clear is answered once the fresh client's process is confirmed,
    // which can be before that process has recorded its launch. Waiting runs
    // the service, which forgets the cleared session once it has ended.
    until(&mut service, &events, |_| recorded() > launched);
    let launches = fs::read_to_string(dir.path().join("launches")).unwrap();
    assert!(launches.lines().any(|line| line.contains("\"resume\"") && line.contains(&thread)), "{launches}");
    assert!(fs::read_to_string(dir.path().join("typed")).unwrap_or_default().is_empty(), "nothing typed");
    assert!(service.native_clears.is_empty());
    service.begin_shutdown(false);
}
