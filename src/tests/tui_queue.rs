use super::*;
use crate::tui::tests::{app_with, press, session};
use crate::work_queue::{ChoreInfo, Info, SourceInfo, Task};
use ratatui::{backend::TestBackend, Terminal};

fn item(id: u64, state: &str, cancelable: bool) -> Info {
    serde_json::from_value(json!({"id":id,"title":"Compile fixture","kind":"job","state":state,"priority":3,"created_at":1,"updated_at":2,"reason":"waiting for capacity","dispatch":null,"dependencies":[],"source":null,"budget":1.25,"cancelable":cancelable,"provider":"codex","model":"fixture-model","placement":"worker"})).unwrap()
}
fn chore() -> ChoreInfo {
    serde_json::from_value(json!({"id":"daily","title":"Daily fixture","owner":"test-owner","owner_live":true,"placement":"local","guarded":false,"provider":"codex","repository":"repo","registered":true,"enabled":false,"effective_enabled":false,"target":"job","schedule":{"cron":"0 8 * * *","timezone":"local"},"next_run":100,"claim":{"day":"2026-11-06","generation":4,"session":"fixture-session","claimed_at":1,"stale_at":28801},"outcome":"fixture complete"})).unwrap()
}
fn app() -> (App, Receiver<ClientMsg>) {
    let (mut app, rx) = app_with(vec![session(11, Tool::Claude, Status::Idle, true)], vec![]);
    app.state.work_queue_capable = true;
    app.state.job_capable = true;
    app.service_version = 14;
    app.state.work_queue.items = vec![item(91, "held", true), item(92, "queued", true), item(93, "failed", false)];
    app.state.work_queue.chores = vec![chore()];
    (app, rx)
}
fn requests(rx: &Receiver<ClientMsg>) -> Vec<(u64, String, Value)> {
    rx.try_iter()
        .filter_map(|m| match m {
            ClientMsg::Fleet { request, action, args } => Some((request, action, args)),
            _ => None,
        })
        .collect()
}
fn draw(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal
        .draw(|f| {
            crate::view::render(f, app);
        })
        .unwrap();
    terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect::<Vec<_>>().join("")
}

#[test]
fn new_routes_and_jobs_require_protocol14_and_capability_with_unique_requests() {
    let (mut app, rx) = app();
    for (version, capable) in [(13, true), (14, false)] {
        app.service_version = version;
        app.state.work_queue_capable = capable;
        for command in [
            "queue list",
            "after queue 91 finished -- printf test",
            "batch absent",
            "chores list",
            "queue source s t -- printf test",
        ] {
            app.run_command(command);
            assert!(requests(&rx).is_empty(), "{command}");
            assert!(app.notice.as_ref().unwrap().0.contains("update needed"));
        }
    }
    app.service_version = 14;
    app.run_command("job status");
    let job = requests(&rx).pop().unwrap();
    assert_eq!(job.1, "job-status");
    app.service_version = 14;
    app.state.work_queue_capable = true;
    app.run_command("queue list");
    let queue = requests(&rx).pop().unwrap();
    assert_eq!(queue.1, "queue-list");
    assert_ne!(job.0, queue.0);
    assert!(queue.0 >= 1 << 63);
    app.handle_server(ServerMsg::FleetResult {
        request: queue.0,
        data: json!({"enabled":false,"items":[item(91,"held",true)]}),
        error: None,
    });
    app.handle_server(ServerMsg::FleetResult { request: job.0, data: json!({"jobs":[]}), error: None });
    assert_eq!(app.state.work_queue.items.len(), 1);
}

#[test]
fn queue_and_after_create_literal_explicit_specs_with_and_dependencies_without_enabling_dispatch() {
    let (mut app, rx) = app();
    app.run_command("queue add --key stable --title 'Build all' --cwd /srv/repo --host worker --provider codex --account test --priority 9 --budget 2.50 --after 91 --after-task markdown task-a ready -- printf '$(never execute)' '*.rs'");
    let request = requests(&rx).pop().unwrap();
    assert_eq!(request.1, "queue-add");
    let spec: Spec = serde_json::from_value(request.2).unwrap();
    assert_eq!(spec.argv, ["printf", "$(never execute)", "*.rs"]);
    assert_eq!(spec.key, "stable");
    assert_eq!(spec.title, "Build all");
    assert_eq!(spec.account.as_deref(), Some("test"));
    assert_eq!(spec.priority, 9);
    assert_eq!(spec.budget, Some(2.5));
    assert_eq!(spec.dependencies.len(), 2);
    assert!(!app.state.work_queue.enabled);
    app.run_command("after task github issue-3 merged --key child --kind session --provider codex --prompt 'Check result' --after 92");
    let request = requests(&rx).pop().unwrap();
    assert_eq!(request.1, "queue-add");
    let spec: Spec = serde_json::from_value(request.2).unwrap();
    assert_eq!(spec.key, "child");
    assert_eq!(spec.dependencies.len(), 2);
    assert_eq!(spec.dependencies[0].condition, "merged");
    assert_eq!(spec.dependencies[1].queue, Some(92));
    assert!(spec.argv.is_empty());
    for command in [
        "queue add --budget NaN -- printf bad",
        "queue add --budget 0 -- printf bad",
        "after queue 91 ready -- printf bad",
    ] {
        app.run_command(command);
        assert!(requests(&rx).is_empty());
    }
}

#[test]
fn queue_controls_preserve_opaque_routing_and_show_reorder_priority_semantics() {
    let (mut app, rx) = app();
    let opaque: u64 = (7 << 48) + 91;
    app.run_command(&format!("queue cancel {opaque}"));
    let cancel = requests(&rx).pop().unwrap();
    assert_eq!(cancel.1, "queue-cancel");
    assert_eq!(cancel.2, json!({"id":opaque}));
    app.run_command("queue reorder 92 91");
    let reorder = requests(&rx).pop().unwrap();
    assert_eq!(reorder.1, "queue-reorder");
    assert_eq!(reorder.2, json!({"ids":[92,91]}));
    app.handle_server(ServerMsg::FleetResult {
        request: reorder.0,
        data: json!({"enabled":false,"items":[item(92,"queued",true),item(91,"held",true)]}),
        error: None,
    });
    assert_eq!(app.state.work_queue.items.iter().map(|i| i.id).collect::<Vec<_>>(), [92, 91]);
    assert!(app.notice.as_ref().unwrap().0.contains("priority differences reset"));
    app.run_command("queue enable");
    let enable = requests(&rx).pop().unwrap();
    assert_eq!(enable.1, "queue-enable");
    assert_eq!(enable.2, json!({"enabled":true}));
    app.handle_server(ServerMsg::FleetResult {
        request: cancel.0,
        data: json!({}),
        error: Some("stale/inflight item".into()),
    });
    assert!(app.notice.as_ref().unwrap().0.contains("stale/inflight"));
}

#[test]
fn sources_never_execute_metadata_and_rendering_or_navigation_never_submits_actions() {
    let (mut app, rx) = app();
    let source_id = "source' --disable";
    let item_id = "task \" $(never execute)";
    app.run_command(&format!("queue source {} {}", util::shell_quote(source_id), util::shell_quote(item_id)));
    assert!(requests(&rx).is_empty());
    let form = app.command.clone();
    app.run_command(&format!("{form}printf literal"));
    let explicit = requests(&rx).pop().unwrap();
    assert_eq!(explicit.2["source"], source_id);
    assert_eq!(explicit.2["item"], item_id);
    app.state.work_queue.sources = vec![SourceInfo {
        id: "source-a".into(),
        owner: "fixture-owner".into(),
        repository: "repo".into(),
        host: None,
        enabled: true,
        updated_at: Some(1),
        fresh: true,
        error: None,
        omitted: 0,
    }];
    app.state.work_queue.tasks = vec![Task {
        source_id: "source-a".into(),
        id: "task-a".into(),
        title: "Untrusted\u{1b}[31m title".into(),
        state: "ready".into(),
        worktree: Some("/srv/repo".into()),
        branch: Some("feature".into()),
        next_step: Some("$(never run)\u{7}".into()),
        target: None,
    }];
    app.run_command("queue source source-a task-a");
    assert!(requests(&rx).is_empty());
    assert_eq!(app.mode, Mode::Command);
    assert!(app.command.ends_with(" -- "));
    app.run_command("queue source source-a task-a --key explicit -- printf literal");
    let source = requests(&rx).pop().unwrap();
    assert_eq!(source.1, "source-queue");
    assert_eq!(source.2["source"], "source-a");
    assert_eq!(source.2["item"], "task-a");
    assert_eq!(source.2["spec"]["argv"], json!(["printf", "literal"]));
    assert!(source.2.get("host").is_none());
    app.mode = Mode::Normal;
    app.current = Some(Sel::Job(77));
    app.dialog = Some(Dialog::WorkQueue { tab: Tab::Tasks, sel: 0 });
    let before = app.state.work_queue.clone();
    for _ in 0..5 {
        let text = draw(&mut app);
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{7}'));
        assert!(text.contains("metadata is never executed") || text.contains("never executed"));
    }
    assert!(requests(&rx).is_empty());
    assert_eq!(app.state.work_queue, before);
    assert_eq!(app.current, Some(Sel::Job(77)));
    assert_eq!(app.mode, Mode::Normal);
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert!(matches!(app.dialog, Some(Dialog::WorkQueue { tab: Tab::Chores, .. })));
    let text = draw(&mut app);
    assert!(text.contains("generation 4"));
    assert!(text.contains("fixture complete"));
    assert!(requests(&rx).is_empty());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.dialog.is_none());
    assert_eq!(app.current, Some(Sel::Job(77)));
    app.state.work_queue.tasks[0].id = "bad\u{1b}task".into();
    app.dialog = Some(Dialog::WorkQueue { tab: Tab::Tasks, sel: 0 });
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    assert_eq!(app.mode, Mode::Normal);
    assert!(requests(&rx).is_empty());
    assert!(app.notice.as_ref().unwrap().0.contains("control characters"));
    app.state.work_queue.tasks.clear();
    let text = draw(&mut app);
    assert!(text.contains("source-a"));
    assert!(text.contains("fixture-owner"));
    assert!(requests(&rx).is_empty());
}

#[test]
fn chore_controls_are_explicit_preserve_claims_and_register_disabled_with_full_spec() {
    let (mut app, rx) = app();
    let original = app.state.work_queue.chores[0].clone();
    app.run_command("chores register daily --owner test-owner --repo repo --cron '0 8 * * *' --timezone UTC --target job --title Daily -- printf '--owner'");
    let register = requests(&rx).pop().unwrap();
    assert_eq!(register.1, "chore-register");
    assert_eq!(register.2["enabled"], false);
    assert_eq!(register.2["schedule"], json!({"cron":"0 8 * * *","timezone":"UTC"}));
    assert_eq!(register.2["spec"]["argv"], json!(["printf", "--owner"]));
    app.run_command("chores schedule daily '30 9 * * 1' UTC");
    let schedule = requests(&rx).pop().unwrap();
    assert_eq!(schedule.1, "chore-update");
    assert_eq!(schedule.2, json!({"id":"daily","schedule":{"cron":"30 9 * * 1","timezone":"UTC"}}));
    app.run_command("chores enable daily");
    app.run_command("chores run daily run-once");
    let controls = requests(&rx);
    assert_eq!(controls[0].1, "chore-enable");
    assert_eq!(controls[1].1, "chore-run");
    assert_eq!(controls[1].2, json!({"id":"daily","key":"run-once"}));
    assert_eq!(app.state.work_queue.chores[0], original, "UI never takes/acks/completes a claim");
}

#[test]
fn batch_is_bounded_parsed_completely_and_sent_as_one_atomic_request() {
    let (mut app, rx) = app();
    let path = std::env::temp_dir().join(generated_key());
    std::fs::write(
        &path,
        "--key one -- printf 'literal space'\n--key two --kind session --provider codex --prompt 'second item'\n",
    )
    .unwrap();
    app.run_command(&format!("batch {}", util::shell_quote(path.to_str().unwrap())));
    let request = requests(&rx).pop().unwrap();
    assert_eq!(request.1, "queue-batch");
    assert_eq!(request.2["items"].as_array().unwrap().len(), 2);
    assert_eq!(request.2["items"][0]["argv"], json!(["printf", "literal space"]));
    std::fs::write(&path, "--key one -- printf valid\n--not-an-option broken\n").unwrap();
    app.run_command(&format!("batch {}", path.display()));
    assert!(requests(&rx).is_empty());
    assert!(app.notice.as_ref().unwrap().0.contains("Line 2"));
    std::fs::write(&path, vec![b'x'; (MAX_BATCH_BYTES + 1) as usize]).unwrap();
    app.run_command(&format!("batch {}", path.display()));
    assert!(requests(&rx).is_empty());
    assert!(app.notice.as_ref().unwrap().0.contains("1 MiB"));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn filtered_status_merges_metadata_and_timeouts_never_replay_mutations() {
    let (mut app, rx) = app();
    app.run_command("queue list 91");
    let request = requests(&rx).pop().unwrap();
    app.handle_server(ServerMsg::FleetResult {
        request: request.0,
        data: json!({"enabled":false,"items":[item(91,"failed",false)]}),
        error: None,
    });
    assert_eq!(app.state.work_queue.items.len(), 3);
    assert!(app.state.work_queue.items.iter().any(|i| i.id == 91 && i.state == "failed"));
    app.run_command("queue cancel 92");
    requests(&rx);
    app.poll_work_queue(Instant::now() + Duration::from_secs(16));
    assert!(requests(&rx).is_empty());
    assert!(app.notice.as_ref().unwrap().0.contains("may already have succeeded"));
}

#[test]
fn dispatched_entries_open_jobs_without_input_and_completion_exposes_new_commands() {
    let (mut app, rx) = app();
    app.state.work_queue.items[0].dispatch =
        Some(crate::work_queue::Dispatch { kind: "job".into(), id: (7 << 48) + 9 });
    app.dialog = Some(Dialog::WorkQueue { tab: Tab::Queue, sel: 0 });
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.current, Some(Sel::Job((7 << 48) + 9)));
    assert_eq!(app.mode, Mode::Normal);
    assert!(!rx.try_iter().any(|m| matches!(m, ClientMsg::Input { .. } | ClientMsg::Attach { id: Some(_), .. })));
    app.command = "que".into();
    app.complete_command(false);
    assert_eq!(app.command, "queue ");
    app.command = "queue reor".into();
    app.completion = None;
    app.complete_command(false);
    assert_eq!(app.command, "queue reorder ");
}

#[test]
fn bare_chores_aliases_open_the_read_only_list() {
    let (mut app, rx) = app();
    for command in ["chores", "chore"] {
        app.run_command(command);
        assert!(matches!(app.dialog, Some(Dialog::WorkQueue { tab: Tab::Chores, .. })), "{command} should open chores");
        assert_eq!(requests(&rx).pop().unwrap().1, "chore-list");
    }
}

#[test]
fn live_state_and_replies_preserve_selected_identity_and_never_cancel_a_replacement() {
    let (mut app, rx) = app();
    app.dialog = Some(Dialog::WorkQueue { tab: Tab::Queue, sel: 1 });
    let mut state = app.state.clone();
    state.work_queue.items.insert(0, item(90, "queued", true));
    app.handle_server(ServerMsg::State(state));
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(requests(&rx).is_empty(), "Ctrl-C is not the explicit cancel key");
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert_eq!(requests(&rx).pop().unwrap().2, json!({"id":92}), "a new earlier row cannot redirect cancellation");
    app.run_command("queue list");
    let read = requests(&rx).pop().unwrap();
    app.handle_server(ServerMsg::FleetResult {
        request: read.0,
        data: json!({"enabled":false,"items":[item(92,"queued",true),item(91,"held",true),item(90,"queued",true)]}),
        error: None,
    });
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert_eq!(requests(&rx).pop().unwrap().2, json!({"id":90}), "reordered replies preserve selected identity");
    let mut state = app.state.clone();
    state.work_queue.items.retain(|i| i.id != 90);
    app.handle_server(ServerMsg::State(state));
    for _ in 0..2 {
        press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(requests(&rx).is_empty(), "selected missing entry must refuse controls");
    }
}

#[test]
fn non_regular_batch_input_is_rejected_without_waiting_for_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    let path = std::env::temp_dir().join(generated_key());
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    let (tx, rx) = std::sync::mpsc::channel();
    let input = path.clone();
    std::thread::spawn(move || {
        let _ = tx.send(batch(input.to_str().unwrap(), "/srv/repo"));
    });
    let result = rx.recv_timeout(Duration::from_secs(2));
    std::fs::remove_file(path).unwrap();
    assert!(result.is_ok(), "opening batch input must not wait for a FIFO writer");
    assert!(result.unwrap().unwrap_err().contains("regular file"));
}

#[test]
fn an_actual_older_welcome_and_state_cannot_enable_queue_actions_or_open_an_executable_form() {
    let (mut app, rx) = app();
    app.run_command("queue enable");
    let pending = requests(&rx).pop().unwrap();
    app.handle_server(ServerMsg::Welcome { version: 13, pid: 1, start_dir: "/srv/repo".into() });
    app.handle_server(ServerMsg::State(DeckState {
        start_dir: "/srv/repo".into(),
        job_capable: true,
        ..DeckState::default()
    }));
    app.handle_server(ServerMsg::FleetResult { request: pending.0, data: json!({"enabled":true}), error: None });
    assert!(!app.state.work_queue.enabled, "a late reply from the old connection cannot activate queue state");
    for command in [
        "queue enable",
        "queue source source task",
        "chores run daily once",
        "after queue 91 finished -- printf bad",
        "batch never-read",
    ] {
        app.run_command(command);
        assert!(requests(&rx).is_empty());
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.state.work_queue.enabled);
        assert!(app.notice.as_ref().unwrap().0.contains("update needed"));
    }
    app.run_command("job status");
    assert!(requests(&rx).is_empty(), "Jobs also require protocol 14");
    assert!(app.notice.as_ref().unwrap().0.contains("update needed"));
}
