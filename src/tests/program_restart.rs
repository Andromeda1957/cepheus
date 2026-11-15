//! A session running the deck's own program, such as the Dot chat bridge,
//! moves onto the installed program once that program's file is replaced.
//! A Dot chat started on 2026-11-07 kept its deleted build after the
//! 2026-11-08 native protocol change and showed "Dot needs attention: The
//! native chat host needs the matching Cepheus build" (owner, 2026-11-09:
//! "and dot is still messsed up"; "the phone /service need to be able to
//! figure it out").
use super::*;
use crate::testsupport::TempDir;
use std::os::unix::fs::MetadataExt;

/// A loop that stays until its terminal goes away, printing the state
/// directory it was given.
const CHAT: &str = "echo \"ready $1\"; while read line; do :; done";

struct Deck {
    dir: TempDir,
    program: PathBuf,
    state: PathBuf,
    svc: Service,
    events: Receiver<Ev>,
    /// The upkeep's clock, which only moves forward.
    clock: Instant,
}

/// A service whose installed program is a copy of `/bin/sh` and a provider
/// registered to run that program with a private state directory.
fn deck(name: &str) -> Deck {
    let dir = TempDir::new(name);
    let program = dir.executable("bin/deck", fs::read("/bin/sh").unwrap());
    let state = dir.path().join("chat");
    fs::create_dir_all(&state).unwrap();
    let provider = serde_json::from_value(serde_json::json!({
        "id":"deck-chat", "label":"Deck chat",
        "command":[program, "-c", CHAT, "chat", state],
        "chat_snapshot": state.join("transcript.json"),
    }))
    .unwrap();
    let mut opts = super::tests::options(&dir, "true");
    opts.exe = program.clone();
    opts.config.providers = vec![provider];
    let (tx, events) = mpsc::channel();
    let svc = Service::new(opts, tx);
    Deck { dir, program, state, svc, events, clock: Instant::now() }
}

fn pid(svc: &Service, id: u64) -> u32 {
    svc.sessions[&id].hosted.as_ref().and_then(|h| h.pid).expect("a running program")
}

fn idle(svc: &mut Service, id: u64) {
    svc.sessions.get_mut(&id).unwrap().tracker.status = Status::Idle;
}

/// Install a new build of the program at its path, as an install does:
/// a new file renamed over the old one.
fn replace(deck: &Deck) {
    let new = deck.dir.executable("bin/deck.new", fs::read("/bin/sh").unwrap());
    fs::rename(new, &deck.program).unwrap();
}

/// Let the service run its upkeep for `seconds`, handling what happens.
fn run(deck: &mut Deck, seconds: u64) {
    for _ in 0..seconds * 2 {
        deck.clock += Duration::from_millis(500);
        deck.svc.housekeeping(deck.clock);
        while let Ok(event) = deck.events.recv_timeout(Duration::from_millis(20)) {
            deck.svc.handle(event);
        }
    }
}

fn file_id(path: &Path) -> (u64, u64) {
    let meta = fs::metadata(path).unwrap();
    (meta.dev(), meta.ino())
}

#[test]
fn a_replaced_deck_program_starts_again_on_the_installed_one_in_the_same_session() {
    let mut deck = deck("program-restart");
    let tool = Tool::Dynamic("deck-chat");
    let id = deck.svc.create_with(tool, None, &StartModel::default(), &Placement::default()).unwrap();
    let ready = format!("ready {}", deck.state.display());
    super::tests::wait_for_screen(&deck.svc, id, &ready);
    idle(&mut deck.svc, id);
    let first = pid(&deck.svc, id);
    run(&mut deck, 30);
    assert_eq!(pid(&deck.svc, id), first, "the program at its path is unchanged: nothing to move");

    replace(&deck);
    let deadline = Instant::now() + Duration::from_secs(20);
    while deck.svc.sessions[&id].hosted.as_ref().and_then(|h| h.pid) == Some(first) {
        assert!(Instant::now() < deadline, "the replaced program was never moved");
        run(&mut deck, 15);
    }
    super::tests::wait_for_screen(&deck.svc, id, &ready);
    let s = &deck.svc.sessions[&id];
    assert!(s.alive && s.closing.is_none());
    assert_eq!(s.tool, tool);
    assert_eq!(deck.svc.sessions.len(), 1, "the same session, not another");
    let again = pid(&deck.svc, id);
    assert_eq!(file_id(Path::new(&format!("/proc/{again}/exe"))), file_id(&deck.program), "the installed build");
    let entry = deck.svc.registry.sessions.iter().find(|e| e.id == id).expect("its registry entry");
    assert_eq!(entry.ended_at, None);
    assert_eq!(deck.svc.registry.sessions.len(), 1);
    assert!(deck.svc.errors.borrow().is_empty(), "moving is no error: {:?}", deck.svc.errors.borrow());
    // Once moved, it runs the installed program and stays.
    idle(&mut deck.svc, id);
    run(&mut deck, 30);
    assert_eq!(pid(&deck.svc, id), again);
}

#[test]
fn a_replaced_deck_program_waits_while_it_works_or_a_message_is_unsettled() {
    let mut deck = deck("program-restart-wait");
    let id =
        deck.svc.create_with(Tool::Dynamic("deck-chat"), None, &StartModel::default(), &Placement::default()).unwrap();
    super::tests::wait_for_screen(&deck.svc, id, "ready");
    let first = pid(&deck.svc, id);
    replace(&deck);

    deck.svc.sessions.get_mut(&id).unwrap().tracker.status = Status::Working;
    run(&mut deck, 30);
    assert_eq!(pid(&deck.svc, id), first, "never while it works");

    idle(&mut deck.svc, id);
    let journal = deck.state.join("pending.json");
    fs::write(&journal, r#"{"text":"an owner message","request":"0b6c"}"#).unwrap();
    run(&mut deck, 30);
    assert_eq!(pid(&deck.svc, id), first, "never while a message's delivery is unsettled");
    fs::write(&journal, "{").unwrap();
    run(&mut deck, 30);
    assert_eq!(pid(&deck.svc, id), first, "an unreadable journal is not a settled one");

    fs::write(&journal, "null").unwrap();
    let sending = crate::extensions::WorkGuard::new(deck.svc.extensions.workers.clone());
    run(&mut deck, 30);
    assert_eq!(pid(&deck.svc, id), first, "never while a message is on its way");
    drop(sending);

    let deadline = Instant::now() + Duration::from_secs(20);
    while deck.svc.sessions[&id].hosted.as_ref().and_then(|h| h.pid) == Some(first) {
        assert!(Instant::now() < deadline, "the settled program was never moved");
        run(&mut deck, 15);
    }
    assert_eq!(fs::read_to_string(&journal).unwrap(), "null", "its journal is untouched");
}

#[test]
fn agents_and_terminals_running_the_deck_program_are_never_restarted() {
    let mut deck = deck("program-restart-others");
    let program = deck.program.to_string_lossy().into_owned();
    deck.svc.opts.config.claude_command = vec![program.clone(), "-c".into(), CHAT.into(), "claude".into()];
    let claude = deck.svc.create(Tool::Claude, None).unwrap();
    let shell = deck
        .svc
        .create_with(
            Tool::Shell,
            None,
            &StartModel::default(),
            &Placement { command: vec![program, "-c".into(), CHAT.into(), "shell".into()], ..Placement::default() },
        )
        .unwrap();
    super::tests::wait_for_screen(&deck.svc, claude, "ready");
    super::tests::wait_for_screen(&deck.svc, shell, "ready");
    let before = [pid(&deck.svc, claude), pid(&deck.svc, shell)];
    replace(&deck);
    for id in [claude, shell] {
        idle(&mut deck.svc, id);
    }
    run(&mut deck, 60);
    assert_eq!([pid(&deck.svc, claude), pid(&deck.svc, shell)], before);
    assert!(deck.svc.sessions.values().all(|s| s.alive && s.closing.is_none()));
}

#[test]
fn a_replaced_deck_program_follows_the_configured_check_and_grace() {
    let mut deck = deck("program-restart-config");
    let id =
        deck.svc.create_with(Tool::Dynamic("deck-chat"), None, &StartModel::default(), &Placement::default()).unwrap();
    let ready = format!("ready {}", deck.state.display());
    super::tests::wait_for_screen(&deck.svc, id, &ready);
    idle(&mut deck.svc, id);

    // Looked at once a minute, and moved at the first look that finds it.
    deck.svc.opts.config.program_restart_check_secs = 60;
    deck.svc.opts.config.program_restart_grace_secs = 0;
    run(&mut deck, 1);
    let first = pid(&deck.svc, id);
    replace(&deck);
    run(&mut deck, 40);
    assert_eq!(pid(&deck.svc, id), first, "not looked at again before the configured check");
    let deadline = Instant::now() + Duration::from_secs(20);
    while deck.svc.sessions[&id].hosted.as_ref().and_then(|h| h.pid) == Some(first) {
        assert!(Instant::now() < deadline, "the replaced program was never moved");
        run(&mut deck, 15);
    }
    super::tests::wait_for_screen(&deck.svc, id, &ready);

    // Looked at every second, and given a minute to move by itself.
    deck.svc.opts.config.program_restart_check_secs = 1;
    deck.svc.opts.config.program_restart_grace_secs = 60;
    idle(&mut deck.svc, id);
    let second = pid(&deck.svc, id);
    replace(&deck);
    run(&mut deck, 40);
    assert_eq!(pid(&deck.svc, id), second, "not moved before the configured grace");
    let deadline = Instant::now() + Duration::from_secs(20);
    while deck.svc.sessions[&id].hosted.as_ref().and_then(|h| h.pid) == Some(second) {
        assert!(Instant::now() < deadline, "the replaced program was never moved after its grace");
        run(&mut deck, 15);
    }
    super::tests::wait_for_screen(&deck.svc, id, &ready);
    assert!(deck.svc.errors.borrow().is_empty(), "moving is no error: {:?}", deck.svc.errors.borrow());
}
