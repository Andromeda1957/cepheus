use super::*;
use crate::protocol::{read_msg, Alert, PROTOCOL_VERSION};
use crate::testsupport::TempDir;
use std::os::unix::net::UnixListener;

/// A service with the owner's client 7, and a socket the fake Claude
/// processes connect to, so their connections carry a hosted process's
/// identity as the plugin's listener and receipts do.
fn fixture() -> (TempDir, Service, Receiver<Out>, UnixListener, Receiver<Ev>) {
    let dir = TempDir::new("claude-input");
    let (tx, events) = mpsc::channel();
    let mut svc = Service::new(super::super::tests::options(&dir, "true"), tx);
    let (out, owner) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: 7, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    let peers = UnixListener::bind(dir.path().join("peer.sock")).unwrap();
    peers.set_nonblocking(true).unwrap();
    (dir, svc, owner, peers, events)
}

/// A hosted Claude whose child process connects to `peers` and stays, as
/// the plugin's listener and receipts run under Claude.
fn connected(svc: &mut Service, dir: &TempDir, peers: &UnixListener) -> (u64, UnixStream) {
    connected_as(svc, dir, peers, "")
}

/// `connected`, where `exec` ("exec " or "") makes the connecting process
/// replace the launch command instead of running under it. The connection
/// shows the program runs: the deck's launch can return before its exec.
fn connected_as(svc: &mut Service, dir: &TempDir, peers: &UnixListener, exec: &str) -> (u64, UnixStream) {
    let socket = dir.path().join("peer.sock");
    let script = format!(
        "{exec}python3 -c 'import socket,time; s=socket.socket(socket.AF_UNIX); s.connect(\"{}\"); time.sleep(60)'; true",
        socket.display()
    );
    let id = claude(svc, &script);
    let deadline = Instant::now() + crate::testsupport::scaled_timeout(Duration::from_secs(10));
    loop {
        match peers.accept() {
            Ok((stream, _)) => return (id, stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "the hosted process never connected");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn claude(svc: &mut Service, script: &str) -> u64 {
    svc.opts.config.claude_command = vec!["sh".into(), "-c".into(), script.into()];
    svc.create(Tool::Claude, None).unwrap()
}

/// A hook connection from the process at the other end of `peer`.
fn hook(svc: &mut Service, client: u64, peer: &UnixStream) -> Receiver<Out> {
    let (out, replies) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: client, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    let endpoint = super::super::attention_native::Endpoint::from_stream(peer).unwrap();
    svc.handle(Ev::NativePeer { client, endpoint });
    svc.handle(Ev::Client(client, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Hook }));
    while replies.try_recv().is_ok() {}
    replies
}

fn listen(svc: &mut Service, client: u64, peer: &UnixStream, conversation: &str) -> Receiver<Out> {
    listen_with(svc, client, peer, conversation, false)
}

/// A listener whose plugin ends a turn on the owner's Stop when `stop`.
fn listen_with(svc: &mut Service, client: u64, peer: &UnixStream, conversation: &str, stop: bool) -> Receiver<Out> {
    let replies = hook(svc, client, peer);
    svc.handle(Ev::Client(client, ClientMsg::OwnerInputListen { conversation: conversation.into(), stop }));
    replies
}

fn receipt(svc: &mut Service, client: u64, peer: &UnixStream, message: &str, outcome: OwnerInputOutcome) -> ServerMsg {
    let replies = hook(svc, client, peer);
    let reason = Some("a host's limit\nreached".to_string());
    svc.handle(Ev::Client(client, ClientMsg::OwnerInputReceipt { message: message.into(), outcome, reason }));
    let mut answer = messages(&replies);
    assert_eq!(answer.len(), 1, "one answer to a receipt");
    answer.remove(0)
}

fn messages(replies: &Receiver<Out>) -> Vec<ServerMsg> {
    replies
        .try_iter()
        .filter_map(|out| match out {
            Out::Msg(bytes) => read_msg(&mut bytes.as_slice()).unwrap(),
            _ => None,
        })
        .collect()
}

/// The owner message the listener was handed: its id, conversation and text.
fn handed(listener: &Receiver<Out>) -> (String, String, String) {
    let (message, conversation, text, _) = handed_files(listener);
    (message, conversation, text)
}

/// `handed`, and the files the message named.
fn handed_files(listener: &Receiver<Out>) -> (String, String, String, Vec<String>) {
    let (message, conversation, text, files, _) = handed_all(listener);
    (message, conversation, text, files)
}

/// `handed_files`, and whether the plugin was asked to steer the message
/// into the running turn.
fn handed_all(listener: &Receiver<Out>) -> (String, String, String, Vec<String>, bool) {
    let mut handed = messages(listener).into_iter().filter_map(|msg| match msg {
        ServerMsg::OwnerInput { message, conversation, text, files, steer } => {
            Some((message, conversation, text, files, steer))
        }
        _ => None,
    });
    let first = handed.next().expect("the listener was handed the message");
    assert!(handed.next().is_none(), "handed once");
    first
}

/// The notices the owner's client was sent.
fn notices(owner: &Receiver<Out>) -> Vec<(String, bool)> {
    messages(owner)
        .into_iter()
        .filter_map(|msg| match msg {
            ServerMsg::Notice { text, error } => Some((text, error)),
            _ => None,
        })
        .collect()
}

/// The owner Stops the listener was handed.
fn stops(listener: &Receiver<Out>) -> Vec<String> {
    messages(listener)
        .into_iter()
        .filter_map(|msg| match msg {
            ServerMsg::OwnerStop { message } => Some(message),
            _ => None,
        })
        .collect()
}

/// A phone gateway's connection, which alerts reach.
fn phone(svc: &mut Service, client: u64) -> Receiver<Out> {
    let (out, replies) = mpsc::channel();
    svc.handle(Ev::ClientNew { id: client, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    svc.handle(Ev::Client(client, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Remote }));
    while replies.try_recv().is_ok() {}
    replies
}

fn alerts(replies: &Receiver<Out>) -> Vec<Alert> {
    messages(replies)
        .into_iter()
        .filter_map(|msg| match msg {
            ServerMsg::Alert(alert) => Some(alert),
            _ => None,
        })
        .collect()
}

fn last_error(svc: &Service) -> String {
    svc.errors.borrow().last().map(|e| e.text.clone()).unwrap_or_default()
}

/// Hand the service the outcome of the files check it started.
fn checked(svc: &mut Service, events: &Receiver<Ev>) {
    let deadline = Instant::now() + crate::testsupport::scaled_timeout(Duration::from_secs(10));
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(ev @ Ev::ClaudeFilesChecked { .. }) => return svc.handle(ev),
            Ok(_) => {}
            Err(error) => panic!("the files were never checked: {error}"),
        }
    }
}

/// A hosted Claude whose process connects to `peers` and records every byte
/// the terminal is sent in `keys`.
fn connected_recording(svc: &mut Service, dir: &TempDir, peers: &UnixListener, keys: &Path) -> (u64, UnixStream) {
    let socket = dir.path().join("peer.sock");
    let script = format!(
        "exec python3 -c 'import os,socket,tty
s=socket.socket(socket.AF_UNIX); s.connect(\"{}\"); tty.setraw(0); f=open(\"{}\",\"ab\",0)
while True:
    b=os.read(0,64)
    if not b: break
    f.write(b)'",
        socket.display(),
        keys.display()
    );
    let id = claude(svc, &script);
    let deadline = Instant::now() + crate::testsupport::scaled_timeout(Duration::from_secs(10));
    loop {
        match peers.accept() {
            Ok((stream, _)) => return (id, stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "the hosted process never connected");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

const PHONE_COPY: &str = "4f1c2a9e-8b7d-4e3f-9a1b-2c3d4e5f6a7b";
const OTHER_COPY: &str = "0b8f1c9e-6c1a-4f43-9d3e-2a1b3c4d5e6f";

fn sent(owner: &Receiver<Out>) -> Vec<(u64, Option<String>, Option<Delivered>)> {
    messages(owner)
        .into_iter()
        .filter_map(|msg| match msg {
            ServerMsg::MessageSent { request, error, delivered, .. } => Some((request, error, delivered)),
            _ => None,
        })
        .collect()
}

fn refused(answer: ServerMsg, expected: &str) {
    assert!(
        matches!(&answer, ServerMsg::Notice { text, error: true } if text.contains(expected)),
        "{answer:?} should refuse with {expected:?}"
    );
}

#[test]
fn claude_owner_input_serves_only_its_own_session_and_reports_what_claude_answered() {
    let (dir, mut svc, owner, peers, _events) = fixture();
    let (id, peer) = connected(&mut svc, &dir, &peers);
    let (other, other_peer) = connected(&mut svc, &dir, &peers);
    let conversation = svc.sessions[&id].conversation.clone().unwrap();

    // A listener names a conversation of the session it runs under.
    let refusal = listen(&mut svc, 11, &peer, "00000000-0000-4000-8000-00000000dead");
    refused(messages(&refusal).remove(0), "conversation is not this session's");
    assert!(!svc.clients.contains_key(&11), "a refused listener is disconnected");
    let refusal = listen(&mut svc, 12, &other_peer, &conversation);
    refused(messages(&refusal).remove(0), "conversation is not this session's");
    let listener = listen(&mut svc, 13, &peer, &conversation);
    assert!(messages(&listener).is_empty());
    assert_eq!(svc.messages.inputs.listeners[&id].client, 13);
    assert!(!svc.messages.inputs.listeners.contains_key(&other));

    svc.send_message(7, id, 1, "exact owner text\nsecond line", false);
    let (message, addressed, text) = handed(&listener);
    assert_eq!((addressed.as_str(), text.as_str()), (conversation.as_str(), "exact owner text\nsecond line"));
    assert!(sent(&owner).is_empty(), "nothing is reported before Claude answers");

    // Another session's process cannot answer for this message.
    refused(
        receipt(&mut svc, 14, &other_peer, &message, OwnerInputOutcome::Submitted),
        "not from the message's session",
    );
    refused(receipt(&mut svc, 15, &peer, "unknown-message", OwnerInputOutcome::Submitted), "no owner message awaits");
    assert!(sent(&owner).is_empty());

    let taken = receipt(&mut svc, 16, &peer, &message, OwnerInputOutcome::Submitted);
    assert!(matches!(taken, ServerMsg::Notice { error: false, .. }), "{taken:?}");
    assert_eq!(sent(&owner), vec![(1, None, Some(Delivered::Submitted))]);

    // Claude's later refusal is reported; the delivery stays settled.
    let errors = svc.errors.borrow().len();
    receipt(&mut svc, 17, &peer, &message, OwnerInputOutcome::Dropped);
    let logged = svc.errors.borrow().last().unwrap().text.clone();
    assert_eq!(svc.errors.borrow().len(), errors + 1);
    assert!(logged.contains("Claude did not take the message (a host's limit reached)"), "{logged}");
    assert!(sent(&owner).is_empty());
    assert!(svc.messages.inputs.pending.is_empty());

    // A plugin loaded again replaces the listener and closes the old one.
    let replacement = listen(&mut svc, 18, &peer, &conversation);
    assert!(messages(&replacement).is_empty());
    refused(messages(&listener).remove(0), "another owner input listener");
    assert!(!svc.clients.contains_key(&13));
    svc.send_message(7, id, 2, "to the new listener", false);
    assert_eq!(handed(&replacement).2, "to the new listener");
}

#[test]
fn claude_owner_input_settles_each_answer_and_an_unanswered_message_as_unknown() {
    let (dir, mut svc, owner, peers, _events) = fixture();
    let (id, peer) = connected(&mut svc, &dir, &peers);
    let conversation = svc.sessions[&id].conversation.clone().unwrap();
    let listener = listen(&mut svc, 11, &peer, &conversation);

    svc.send_message(7, id, 1, "starts at once", false);
    let (message, ..) = handed(&listener);
    receipt(&mut svc, 12, &peer, &message, OwnerInputOutcome::Started);
    assert_eq!(sent(&owner), vec![(1, None, Some(Delivered::Started))]);

    svc.send_message(7, id, 2, "after a clear", false);
    let (message, ..) = handed(&listener);
    receipt(&mut svc, 13, &peer, &message, OwnerInputOutcome::Moved);
    let expected = "the Claude session moved to another conversation; nothing was sent";
    assert!(matches!(&sent(&owner)[..], [(2, Some(error), None)] if error.contains(expected)));

    svc.send_message(7, id, 3, "dropped", false);
    let (message, ..) = handed(&listener);
    receipt(&mut svc, 14, &peer, &message, OwnerInputOutcome::Dropped);
    let expected = "Claude did not take the message (a host's limit reached); nothing was sent";
    assert!(matches!(&sent(&owner)[..], [(3, Some(error), None)] if error.contains(expected)));

    svc.send_message(7, id, 4, "never answered", false);
    handed(&listener);
    svc.messages_due();
    assert!(sent(&owner).is_empty(), "the answer is awaited until the deadline");
    svc.owner_input_expire();
    svc.messages_due();
    let expected = "Claude did not confirm the owner message; check the conversation before resending";
    assert!(matches!(&sent(&owner)[..], [(4, Some(error), None)] if error.contains(expected)));
    assert!(svc.messages.inputs.pending.is_empty());

    // A closed listener connection is forgotten. A message waits for the
    // plugin to connect again, as it does after the service restarts, and
    // is handed to the new listener once.
    svc.handle(Ev::ClientGone(11));
    svc.messages_due();
    svc.send_message(7, id, 5, "while reconnecting", false);
    svc.messages_due();
    assert!(sent(&owner).is_empty(), "the message waits for the listener");
    let listener = listen(&mut svc, 15, &peer, &conversation);
    let (message, addressed, text) = handed(&listener);
    assert_eq!((addressed.as_str(), text.as_str()), (conversation.as_str(), "while reconnecting"));
    receipt(&mut svc, 16, &peer, &message, OwnerInputOutcome::Submitted);
    assert_eq!(sent(&owner), vec![(5, None, Some(Delivered::Submitted))]);
    svc.messages_due();
    assert!(messages(&listener).is_empty(), "handed once");

    // One no listener takes in time was never sent.
    svc.handle(Ev::ClientGone(15));
    svc.messages_due();
    svc.send_message(7, id, 6, "no listener", false);
    svc.owner_input_expire();
    svc.messages_due();
    let expected = "Claude's native owner input is not connected in this session; nothing was sent";
    assert!(matches!(&sent(&owner)[..], [(6, Some(error), None)] if error.contains(expected)));
}

/// A broadcast addresses working agents, and a plugin's prompt waits until
/// Claude is idle, so the plugin is asked to deliver a broadcast into the
/// running turn instead; a direct message keeps a turn of its own (owner,
/// 2026-11-13: "it needs to send the message asap, not sit in a queue").
#[test]
fn a_broadcast_joins_a_working_claudes_running_turn_and_a_direct_message_keeps_its_own() {
    let (dir, mut svc, owner, peers, _events) = fixture();
    let (id, peer) = connected(&mut svc, &dir, &peers);
    let conversation = svc.sessions[&id].conversation.clone().unwrap();
    let listener = listen(&mut svc, 11, &peer, &conversation);

    svc.send_message(7, id, 1, "a direct follow-up", false);
    let (message, .., steer) = handed_all(&listener);
    assert!(!steer, "a direct message stays the owner's own prompt");
    receipt(&mut svc, 12, &peer, &message, OwnerInputOutcome::Started);
    assert_eq!(sent(&owner), vec![(1, None, Some(Delivered::Started))]);

    svc.sessions.get_mut(&id).unwrap().tracker.status = crate::protocol::Status::Working;
    svc.start_fleet_broadcast(7, &[id], "stop editing the shared file");
    let (message, addressed, text, _, steer) = handed_all(&listener);
    assert!(steer, "a broadcast joins the running turn");
    assert_eq!(addressed, conversation);
    assert!(text.ends_with("Owner message:\nstop editing the shared file"), "{text}");
    assert!(notices(&owner).is_empty(), "nothing is reported before Claude answers");
    receipt(&mut svc, 13, &peer, &message, OwnerInputOutcome::Steered);
    assert_eq!(notices(&owner), vec![("Sent to 1 agent across 1 host".to_string(), false)]);
    assert!(svc.messages.inputs.pending.is_empty());
}

#[test]
fn claude_without_owner_input_says_how_to_enable_it_and_never_reports_sent() {
    let (dir, mut svc, owner, peers, _events) = fixture();
    let before_error = "this Claude session was started before native owner input; resume it to enable";
    let launched_error = "Claude's native owner input is not connected in this session; nothing was sent";

    // The deck launched this Claude with the plugin, whatever its process
    // shows yet: its message waits for the listener, and is not sent when
    // none connects.
    let launched = claude(&mut svc, "exec sleep 60");
    assert!(svc.sessions[&launched].hosted.as_ref().unwrap().argv.is_some());
    svc.send_message(7, launched, 1, "hello", false);
    svc.messages_due();
    assert!(sent(&owner).is_empty(), "the message waits for the listener");
    svc.owner_input_expire();
    svc.messages_due();
    let replies = sent(&owner);
    assert!(matches!(&replies[..], [(1, Some(error), None)] if error.contains(launched_error)), "{replies:?}");

    // A Claude adopted from an earlier run of the service is judged by the
    // command line it runs. `exec` replaces the launch command line, as a
    // Claude started by an earlier build has no plugin on it.
    let (before, _before_peer) = connected_as(&mut svc, &dir, &peers, "exec ");
    let (adopted, _adopted_peer) = connected(&mut svc, &dir, &peers);
    for id in [before, adopted] {
        svc.sessions.get_mut(&id).unwrap().hosted.as_mut().unwrap().argv = None;
    }
    svc.send_message(7, before, 2, "hello", false);
    let replies = sent(&owner);
    assert!(matches!(&replies[..], [(2, Some(error), None)] if error.contains(before_error)), "{replies:?}");
    svc.send_message(7, adopted, 3, "hello", false);
    svc.messages_due();
    assert!(sent(&owner).is_empty(), "an adopted Claude running with the plugin waits for its listener");
    svc.owner_input_expire();
    svc.messages_due();
    let replies = sent(&owner);
    assert!(matches!(&replies[..], [(3, Some(error), None)] if error.contains(launched_error)), "{replies:?}");
}

#[test]
fn the_owner_input_plugin_is_placed_once_for_each_version_and_never_changed_in_place() {
    let dir = TempDir::new("claude-input-plugin");
    let state = dir.path().join("state");
    let placed = materialize(&state).unwrap();
    assert_eq!(placed.parent().unwrap(), state.join("claude-plugins"));
    assert!(placed.file_name().unwrap().to_str().unwrap().starts_with("cepheus-owner-input-"));
    for (path, text) in FILES {
        assert_eq!(fs::read_to_string(placed.join(path)).unwrap(), text);
    }
    // Claude lays its own type declarations into the folder; they stay.
    fs::create_dir_all(placed.join(".claude-plugin/types")).unwrap();
    assert_eq!(materialize(&state).unwrap(), placed);
    assert_eq!(fs::read_dir(state.join("claude-plugins")).unwrap().count(), 1, "no staging folder is left");
    fs::write(placed.join("hooks/register.ts"), "export const register = () => {}").unwrap();
    let error = materialize(&state).unwrap_err().to_string();
    assert!(error.contains("was changed; remove that folder"), "{error}");
}

/// Owner, 2026-11-11, of phone messages that said "Submitted" but never
/// arrived: "this error is still an error too". A message Claude refuses
/// after it took it marks the phone's own copy, each time, not only the
/// error list.
#[test]
fn a_message_claude_refuses_after_taking_it_marks_the_phones_copy() {
    let (dir, mut svc, owner, peers, _events) = fixture();
    let (id, peer) = connected(&mut svc, &dir, &peers);
    let conversation = svc.sessions[&id].conversation.clone().unwrap();
    let listener = listen(&mut svc, 11, &peer, &conversation);
    let phone = phone(&mut svc, 20);
    for (request, copy, client) in [(1, PHONE_COPY, 12), (2, OTHER_COPY, 14)] {
        svc.send_owner_images(7, id, request, "go", &[], Some(copy));
        let (message, ..) = handed(&listener);
        receipt(&mut svc, client, &peer, &message, OwnerInputOutcome::Submitted);
        assert_eq!(sent(&owner), vec![(request, None, Some(Delivered::Submitted))]);
        assert!(alerts(&phone).is_empty());
        receipt(&mut svc, client + 1, &peer, &message, OwnerInputOutcome::Dropped);
        let alerts = alerts(&phone);
        assert!(
            matches!(&alerts[..], [alert] if alert.kind == AlertKind::Error
                && alert.id == Some(id)
                && alert.message.as_deref() == Some(copy)
                && alert.reason == "Claude did not take the message (a host's limit reached); nothing was sent"),
            "{alerts:?}"
        );
        assert!(last_error(&svc).contains("Claude did not take the message"));
    }
    assert!(svc.messages.inputs.pending.is_empty());
}

/// Owner, 2026-11-11: "dont revert it back to sendi g messages with the tui
/// from the phone". The phone's Stop had pressed Esc in Claude's terminal,
/// which removed the queued phone message and refused every later one until
/// someone typed there. The Stop now ends the turn through Claude's plugin.
#[test]
fn the_owners_stop_ends_a_claude_turn_through_its_plugin_without_touching_the_terminal() {
    let (dir, mut svc, owner, peers, _events) = fixture();
    let keys = dir.path().join("keys");
    let (id, peer) = connected_recording(&mut svc, &dir, &peers, &keys);
    let conversation = svc.sessions[&id].conversation.clone().unwrap();
    let listener = listen_with(&mut svc, 11, &peer, &conversation, true);

    // A message Claude holds behind its turn is still awaited after the Stop.
    svc.send_message(7, id, 1, "after the stop", false);
    let (message, ..) = handed(&listener);
    receipt(&mut svc, 12, &peer, &message, OwnerInputOutcome::Submitted);
    assert_eq!(sent(&owner), vec![(1, None, Some(Delivered::Submitted))]);
    svc.handle(Ev::Client(7, ClientMsg::Stop { id }));
    let stop = stops(&listener);
    assert_eq!(stop.len(), 1, "one Stop for the plugin");
    assert!(svc.messages.inputs.pending.contains_key(&message), "the queued message is kept");
    let errors = svc.errors.borrow().len();
    for outcome in [OwnerInputOutcome::Stopped, OwnerInputOutcome::Idle] {
        if outcome == OwnerInputOutcome::Idle {
            svc.handle(Ev::Client(7, ClientMsg::Stop { id }));
        }
        let stop = if outcome == OwnerInputOutcome::Idle { stops(&listener).remove(0) } else { stop[0].clone() };
        let answer = receipt(&mut svc, 13, &peer, &stop, outcome);
        assert!(matches!(answer, ServerMsg::Notice { error: false, .. }), "{answer:?}");
    }
    assert_eq!(svc.errors.borrow().len(), errors, "a Stop carried out is no error");

    // A Stop Claude could not carry out, or never answered, asks for another.
    svc.handle(Ev::Client(7, ClientMsg::Stop { id }));
    let stop = stops(&listener).remove(0);
    receipt(&mut svc, 14, &peer, &stop, OwnerInputOutcome::Failed);
    assert!(last_error(&svc).contains("Claude did not stop the turn (a host's limit reached); press Stop again"));
    svc.handle(Ev::Client(7, ClientMsg::Stop { id }));
    assert_eq!(stops(&listener).len(), 1);
    svc.messages_due();
    assert!(!last_error(&svc).contains("did not confirm the Stop"), "the answer is awaited until its deadline");
    svc.owner_input_expire();
    svc.messages_due();
    assert!(last_error(&svc).contains("Claude did not confirm the Stop; press Stop again"));
    assert!(svc.messages.inputs.pending.contains_key(&message), "the queued message is still kept");

    // A plugin that cannot end a turn (a Claude started by an older build)
    // gets the terminal's Esc, as before. The terminal then holds exactly
    // that one Esc, so none of the Stops above reached it.
    let older = listen_with(&mut svc, 15, &peer, &conversation, false);
    svc.handle(Ev::Client(7, ClientMsg::Stop { id }));
    assert!(stops(&older).is_empty());
    let deadline = Instant::now() + crate::testsupport::scaled_timeout(Duration::from_secs(10));
    while fs::read(&keys).unwrap_or_default() != b"\x1b" {
        assert!(Instant::now() < deadline, "the terminal never got the Esc: {:?}", fs::read(&keys));
        thread::sleep(Duration::from_millis(20));
    }
}

/// Owner, 2026-11-08: "pasting images must work along with text for every
/// provider that supports images". Claude takes the owner's uploads by the
/// paths this host checked, any kind of file, through its prompt interface.
#[test]
fn claude_takes_the_owners_files_by_the_paths_this_host_checked() {
    let (dir, mut svc, owner, peers, events) = fixture();
    let (id, peer) = connected(&mut svc, &dir, &peers);
    let conversation = svc.sessions[&id].conversation.clone().unwrap();
    let listener = listen(&mut svc, 11, &peer, &conversation);
    let folder = svc.opts.state_dir.join("uploads").join(id.to_string());
    fs::create_dir_all(&folder).unwrap();
    let (shot, notes) = (folder.join("shot.png"), folder.join("notes.pdf"));
    fs::write(&shot, b"\x89PNG\r\n\x1a\nimage").unwrap();
    fs::write(&notes, b"%PDF-1.7 owner notes").unwrap();
    let digest = |path: &Path| crate::remote::auth::sha256_hex(&fs::read(path).unwrap());
    let files = vec![(shot.clone(), digest(&shot)), (notes.clone(), digest(&notes))];

    svc.send_owner_images(7, id, 1, "read these", &files, Some(PHONE_COPY));
    assert!(messages(&listener).is_empty(), "the files are checked first");
    checked(&mut svc, &events);
    let (message, addressed, text, named) = handed_files(&listener);
    let paths: Vec<String> = [&shot, &notes].iter().map(|p| p.canonicalize().unwrap().display().to_string()).collect();
    assert_eq!(addressed, conversation);
    assert_eq!(text, format!("read these\n\n{}\n{}", paths[0], paths[1]));
    assert_eq!(named, paths);
    receipt(&mut svc, 12, &peer, &message, OwnerInputOutcome::Started);
    assert_eq!(sent(&owner), vec![(1, None, Some(Delivered::Started))]);

    // Files alone are a message too.
    svc.send_owner_images(7, id, 2, "", &files[..1], None);
    checked(&mut svc, &events);
    assert_eq!(handed_files(&listener).2, paths[0]);
    svc.owner_input_expire();
    svc.messages_due();
    sent(&owner);

    // A file changed since the gateway hashed it is refused, and Claude is
    // handed nothing.
    fs::write(&notes, b"changed").unwrap();
    svc.send_owner_images(7, id, 3, "again", &files, None);
    checked(&mut svc, &events);
    assert!(messages(&listener).is_empty());
    let replies = sent(&owner);
    assert!(
        matches!(&replies[..], [(3, Some(error), None)] if error.ends_with("(uploaded file changed; nothing was sent)")),
        "{replies:?}"
    );
}
