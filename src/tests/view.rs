use super::*;
use crate::pairing::Pairing;
use crate::protocol::{ContainerInfo, Tool, Worktree};
use crate::screen::{CursorPos, Run, ScreenFrame, CURSOR_BEAM};
use crate::tui::tests::{app_with, outside, session};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn subagent(name: &str, active: bool) -> crate::protocol::Subagent {
    crate::protocol::Subagent { name: name.into(), kind: None, active, started_at: 1, last_active: 1 }
}

fn draw(app: &mut App, width: u16, height: u16) -> (Vec<String>, Option<(u16, u16, u8)>) {
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut cursor = None;
    term.draw(|f| cursor = render(f, app)).unwrap();
    let buf = term.backend().buffer();
    let rows = (0..height).map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect();
    (rows, cursor)
}

fn screen(rows: &[String]) -> String {
    rows.join("\n")
}

fn plugin_script(app: &mut App, source: &str) -> crate::testsupport::TempDir {
    let dir = crate::testsupport::TempDir::new("sidebar-plugin");
    dir.write("config/init.lua", source);
    app.load_lua(
        crate::lua::Places { config_dir: dir.path().join("config"), start_dir: dir.path().into() },
        std::sync::Arc::new(|_| {}),
    );
    dir
}

#[test]
fn plugin_header_cost_stays_on_the_title_row_without_a_second_sidebar_row() {
    for width in [24, 34, 60] {
        let (mut app, _) = app_with(vec![], vec![]);
        app.cfg.sidebar_width = width;
        let before = draw(&mut app, width * 2 + 1, 20).0;
        let _dir = plugin_script(
            &mut app,
            r#"
            deck.ui.section('api-cost', {title='API equivalent', header=true,
                lines={{{'~$9,118.56', 'ok'}, {' *', 'warn'}}}})
            deck.ui.section('other-plugin', {title='', lines={'another plugin'}})
        "#,
        );
        let rows = draw(&mut app, width * 2 + 1, 20).0;
        let title: String = rows[0].chars().take(width as usize).collect();
        assert!(title.contains("~$9,118.56 *"), "width {width}: {rows:#?}");
        assert!(title.ends_with("~$9,118.56 * "), "cost is next to the divider: {title:?}");
        assert_eq!(rows[1], before[1], "resources keep their original row");
        assert!(!rows.iter().skip(1).any(|r| r.contains("9,118")), "no duplicate cost row: {rows:#?}");
        assert!(screen(&rows).contains("another plugin"), "other plugins keep their sections");
    }
}

/// The title row and the style of the cell that starts `text` on it.
fn title_row(app: &mut App, width: u16, text: &str) -> (String, Option<Style>) {
    let mut term = Terminal::new(TestBackend::new(width * 2 + 1, 20)).unwrap();
    term.draw(|f| {
        render(f, app);
    })
    .unwrap();
    let buf = term.backend().buffer();
    let cells: Vec<String> = (0..width).map(|x| buf[(x, 0)].symbol().to_string()).collect();
    let at = (0..cells.len()).find(|&x| cells[x..].concat().starts_with(text));
    (cells.concat(), at.map(|x| buf[(x as u16, 0)].style()))
}

// Owner, 2026-11-09: "the 15 location moved, didnt ask for it to be moved it
// was fine where it was, just asked for the count to be accurate and for it to
// be in lua". It sat right after the branch and its counts.
#[test]
fn an_inline_header_follows_the_title_text_while_header_true_stays_right_aligned() {
    let (mut app, _) = app_with(vec![], vec![]);
    app.cfg.sidebar_width = 40;
    app.state.start_dir = "/w/project".into();
    app.git.now = Some(BranchStatus { branch: "master".into(), ahead: 3, ..BranchStatus::default() });
    let _dir = plugin_script(
        &mut app,
        r#"
        deck.ui.section('count', {title='Count', header='inline', lines={{{'15', 'strong'}}}})
        deck.ui.section('cost', {title='Cost', header=true, lines={{{'$9', 'ok'}}}})
    "#,
    );
    let (title, style) = title_row(&mut app, 40, "15");
    assert!(title.starts_with(" project master ⇡3 15 "), "directly after the title: {title:?}");
    assert!(title.ends_with(" $9 "), "header = true stays beside the divider: {title:?}");
    let style = style.expect("the count is drawn");
    assert_eq!(style.fg, Some(app.theme.accent), "strong is the accent colour");
    assert!(style.add_modifier.contains(Modifier::BOLD), "and bold");
    let rows = draw(&mut app, 81, 20).0;
    assert!(!rows.iter().skip(1).any(|r| r.contains("Count")), "no sidebar row: {rows:#?}");
}

#[test]
fn the_unmerged_branch_count_follows_the_branch_on_the_title_row() {
    let (mut app, _) = app_with(vec![], vec![]);
    app.cfg.sidebar_width = 40;
    let mut state = app.state.clone();
    state.start_dir = "/w/project".into();
    let branch = |name: &str, ahead: u32, main: bool| crate::protocol::BranchInfo {
        branch: name.into(),
        path: format!("/w/{name}"),
        ahead: Some(ahead),
        main,
        ..crate::protocol::BranchInfo::default()
    };
    state.branches = vec![branch("master", 0, true), branch("agent/a", 3, false), branch("agent/b", 1, false)];
    crate::tui::tests::server(&mut app, crate::protocol::ServerMsg::State(state));
    app.git.now = Some(BranchStatus { branch: "master".into(), ahead: 3, ..BranchStatus::default() });
    let dir = crate::testsupport::TempDir::new("title-unmerged");
    app.load_lua(
        crate::lua::Places { config_dir: dir.path().join("config"), start_dir: dir.path().into() },
        std::sync::Arc::new(|_| {}),
    );
    let (title, style) = title_row(&mut app, 40, "2");
    assert_eq!(title.trim_end(), " project master ⇡3 2", "{title:?}");
    let style = style.expect("the count is drawn");
    assert_eq!(style.fg, Some(app.theme.accent));
    assert!(style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn plugin_visibility_changes_builtin_parts_without_replacing_the_sidebar_order() {
    let (mut app, _) = app_with(vec![], vec![]);
    let _dir = plugin_script(
        &mut app,
        r#"
        deck.opt.sidebar_hidden = {'usage', 'resources', 'dev'}
        deck.ui.section('another', {lines={'independent plugin'}})
        deck.command('show-parts', function() deck.opt.sidebar_hidden = {} end)
    "#,
    );
    let parts = sidebar_parts(&app);
    for id in ["usage", "resources", "dev"] {
        assert!(!parts.iter().any(|p| p == id), "{id} should be hidden: {parts:?}");
    }
    assert!(parts.iter().any(|p| p == "another"));
    app.run_command("show-parts");
    let parts = sidebar_parts(&app);
    for id in ["resources", "dev", "another"] {
        assert!(parts.iter().any(|p| p == id), "{id} should be visible: {parts:?}");
    }
}

#[test]
fn shipped_sidebar_controls_default_on_and_persist_independently_after_reload() {
    let (mut app, _) = app_with(vec![], vec![]);
    let dir = plugin_script(&mut app, "deck.opt.sidebar_hidden = {'custom'}");
    dir.write("config/plugins/sidebar-controls.lua", include_str!("../../plugins/sidebar-controls.lua"));
    app.reload_lua();
    assert_eq!(app.sidebar_hidden, ["custom"]);
    for command in ["resources off", "dev-indicator off"] {
        app.run_command(command);
    }
    for id in ["resources", "dev", "custom"] {
        assert!(app.sidebar_hidden.iter().any(|p| p == id));
    }
    app.reload_lua();
    let parts = sidebar_parts(&app);
    assert!(!parts.iter().any(|p| p == "resources" || p == "dev"));
    app.run_command("dev-indicator on");
    app.reload_lua();
    assert!(sidebar_parts(&app).iter().any(|p| p == "dev"));
    app.run_command("resources invalid");
    assert!(!sidebar_parts(&app).iter().any(|p| p == "resources"));
    app.run_command("resources");
    app.reload_lua();
    assert!(sidebar_parts(&app).iter().any(|p| p == "resources"));
    assert!(app.sidebar_hidden.iter().any(|p| p == "custom"));
}

/// Whether a row of the help sheet has `key`, then space, then the start of
/// `about`.
fn beside(rows: &[String], key: &str, about: &str) -> bool {
    let start: String = about.chars().take(12).collect();
    rows.iter().any(|r| {
        r.match_indices(key).any(|(i, _)| {
            let after = &r[i + key.len()..];
            after.starts_with(' ') && after.trim_start().starts_with(&start)
        })
    })
}

/// Every hotkey keeps a space between its key and description.
fn assert_help_columns_apart(rows: &[String], app: &App) {
    for (key, about) in hotkey_rows(app) {
        assert!(beside(rows, &key, &about), "{key} beside {about:?}: {rows:#?}");
    }
}

#[test]
fn sidebar_shows_usage_and_live_sessions() {
    let mut working = session(1, Tool::Claude, Status::Working, true);
    working.title = "Build the deck".into();
    working.host = Some("fedora".into());
    working.created_at = 10;
    working.worktree =
        Some(Worktree { path: "/r/repo-cepheus".into(), branch: None, task: Some("deck".into()), exists: true });
    working.containers = vec![ContainerInfo { name: "c".into(), state: "running".into(), status: "Up".into() }];
    let now = crate::util::now_secs();
    working.status_since = now;
    working.subagents = vec![subagent("explore", true), subagent("done", false), subagent("review", true)];
    working.activity = Some("Edit view.rs".into());
    let mut needs = session(2, Tool::Codex, Status::NeedsYou, true);
    needs.detail = "permission".into();
    needs.created_at = 20;
    let (mut app, _rx) = app_with(vec![working, needs], vec![outside(77, None)]);
    app.hosts = vec![crate::tui::tests::host("fedora", "Fedora", true)];
    // Usage has its own window; the sidebar has it when the config lists it.
    app.cfg.sidebar.parts = ["title", "resources", "dev", "usage", "sessions"].map(String::from).to_vec();
    app.state.usage.claude = vec![
        UsageWindow { label: "5h".into(), used_percent: Some(42.0), resets_at: Some(now + 3600), updated_at: now },
        UsageWindow { label: "wk".into(), used_percent: Some(90.0), resets_at: None, updated_at: now },
    ];
    app.state.usage.grok =
        vec![UsageWindow { label: "wk".into(), used_percent: None, resets_at: Some(now + 7260), updated_at: now }];
    app.state.usage.gemini =
        [("wk", 0.0, 6 * 86400 + 12 * 3600), ("5h", 25.0, 10800), ("Argon wk", 0.0, 6 * 86400 + 23 * 3600)]
            .map(|(label, used, reset)| UsageWindow {
                label: label.into(),
                used_percent: Some(used),
                resets_at: Some(now + reset + 60),
                updated_at: now,
            })
            .into();
    let (rows, _) = draw(&mut app, 120, 30);
    let text = screen(&rows);
    // Owner, 2026-11-01: the deck's name "is taking up space ... it should be
    // removed as its not needed".
    let header: String = rows[0].chars().take(app.cfg.sidebar_width as usize).collect();
    assert!(!header.contains("cepheus"), "{text}");
    let side: Vec<String> = rows.iter().map(|row| row.chars().take(app.cfg.sidebar_width as usize).collect()).collect();
    let usage = screen(&side);
    // One bar per model (owner, 2026-11-12): Claude's row is its most used
    // window, the week, and the 5-hour window has no row of its own.
    let claude: Vec<&String> = side.iter().filter(|row| row.contains("% left") && row.contains("Claude")).collect();
    assert!(claude.len() == 1 && claude[0].contains("wk") && claude[0].contains("10% left"), "{usage}");
    assert!(!usage.contains("58% left"), "{usage}");
    assert!(!usage.contains("no data yet") && !usage.contains("Codex"), "unused providers stay hidden: {text}");
    assert!(!usage.contains("Grok"), "a reset without a share is not use: {text}");
    assert!(!usage.contains("not reported"), "{text}");
    let gemini = side.iter().position(|row| row.contains("Gemini")).unwrap();
    assert!(
        side[gemini].contains("5h") && side[gemini].contains("75%") && side[gemini].contains("3h"),
        "Gemini's used window is on its own row: {usage}"
    );
    assert!(!usage.contains("100%") && !usage.contains("6d12h"), "unused windows stay hidden: {text}");
    assert!(!usage.contains("Argon"), "unused pools stay hidden even when another pool is used: {text}");
    assert!(text.contains("Build the deck"));
    // Its branch is in F3; the row says what the agent is doing (owner,
    // 2026-11-01: "removing the branches from sessions ... show subagents
    // counts, activity , times etc where the branches were").
    assert!(text.contains("⑂2 Edit view.rs ⬢1"), "{text}");
    assert!(!text.contains("⎇") && !text.contains("deck ⬢"), "{text}");
    assert!(text.contains("needs you: permission"), "{text}");
    assert!(text.contains("1 working · 1 need you"), "{text}");
    let hit_rows: Vec<Sel> = app.row_hits.iter().map(|(_, _, s)| *s).collect();
    assert_eq!(hit_rows, vec![Sel::Session(1), Sel::Session(2)]);
    let y1 = app.row_hits[0].0 as usize;
    assert!(rows[y1].contains("Build the deck"), "row hit points at its row");
    // Each window shows by its own use: Claude's unused week and Codex's
    // unused week stay hidden while their used 5-hour windows show.
    app.state.usage.claude[1].used_percent = Some(0.0);
    app.state.usage.codex = vec![
        UsageWindow { label: "5h".into(), used_percent: Some(70.0), resets_at: None, updated_at: now },
        UsageWindow { label: "wk".into(), used_percent: Some(0.0), resets_at: None, updated_at: now },
    ];
    app.state.usage.grok[0].used_percent = Some(35.0);
    app.state.usage.gemini[2].used_percent = Some(20.0);
    // Fable's own weekly limit is a row of its own, apart from Claude's.
    app.state.usage.claude_models =
        vec![UsageWindow { label: "Fable wk".into(), used_percent: Some(30.0), resets_at: None, updated_at: now }];
    for width in [24, 34, 60] {
        app.cfg.sidebar_width = width;
        let (rows, _) = draw(&mut app, width * 2 + 1, 30);
        let rows: Vec<String> = rows.iter().map(|row| row.chars().take(width as usize).collect()).collect();
        let text = screen(&rows);
        assert!(
            rows.iter().any(|row| row.contains("Grok") && row.contains("65%")),
            "used Grok shows its share: {rows:#?}"
        );
        assert!(text.contains("30% left"), "used Codex window should be visible: {rows:#?}");
        assert!(!text.contains("100%") && !text.contains("10% left"), "unused windows stay hidden: {rows:#?}");
        let claude = rows.iter().position(|row| row.contains("Claude") && row.contains('%')).unwrap();
        let quota = &rows[claude..claude + 6];
        let rows_of = |name: &str, window: &str, left: &str| {
            quota.iter().filter(|row| row.contains(name) && row.contains(window) && row.contains(left)).count()
        };
        assert!(
            rows_of("Claude", "5h", "58%") == 1
                && rows_of("Fable", "wk", "70%") == 1
                && rows_of("Codex", "5h", "30%") == 1
                && rows_of("Grok", "wk", "65%") == 1
                && rows_of("Gemini", "5h", "75%") == 1
                && rows_of("Argon", "wk", "80%") == 1,
            "width {width}: one row per used model: {quota:#?}"
        );
        assert!(rows.get(claude + 6).is_none_or(|row| !row.contains('%')), "width {width}: {rows:#?}");
        let percent: Vec<_> = quota.iter().map(|row| row.chars().position(|c| c == '%').unwrap()).collect();
        assert!(percent.iter().all(|column| *column == percent[0]), "width {width}: {quota:#?}");
    }
    // The config can keep one program's rows (owner, 2026-11-13: "maybe i
    // only want to see usage for 1 model").
    app.cfg.sidebar.usage = vec!["gemini".into()];
    let (rows, _) = draw(&mut app, 121, 30);
    let side: Vec<String> = rows.iter().map(|row| row.chars().take(60).collect()).collect();
    let quota: Vec<&String> = side.iter().filter(|row| row.contains('%')).collect();
    assert!(quota.iter().any(|row| row.contains("Gemini") && row.contains("75%")), "{quota:#?}");
    assert!(!quota.iter().any(|row| row.contains("Claude") || row.contains("Codex")), "{quota:#?}");
}

#[test]
fn selected_session_screen_and_cursor_are_drawn_in_the_pane() {
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    let run = Run { c: 2, w: 1, fg: 0, bg: 0, fl: 0, t: "hello pane".into(), g: false };
    let frame = ScreenFrame {
        cols: 80,
        rows: 24,
        lines: vec![vec![run]],
        cursor: Some(CursorPos { x: 4, y: 0, shape: CURSOR_BEAM }),
        ..ScreenFrame::default()
    };
    app.frame = Some((1, frame));
    let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let mut cursor = None;
    term.draw(|f| cursor = render(f, &mut app)).unwrap();
    let buf = term.backend().buffer();
    let pane_x = app.cfg.sidebar_width + 1;
    let drawn: String = (0..10).map(|i| buf[(pane_x + 2 + i, 0)].symbol().to_string()).collect();
    assert_eq!(drawn, "hello pane");
    assert_eq!(buf[(pane_x - 1, 0)].symbol(), "│", "a separator sits between sidebar and pane");
    assert_eq!(cursor, None, "in Normal mode the deck has the keys, so the session shows no cursor");
    app.mode = Mode::Insert;
    let (rows, cursor) = draw(&mut app, 120, 30);
    assert_eq!(cursor, Some((pane_x + 4, 0, CURSOR_BEAM)));
    assert!(rows[29].contains("INSERT"), "{}", rows[29]);
}

#[test]
fn the_help_sheet_shows_the_keys_the_config_file_sets() {
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    let keys = r#"{"close": "ctrl+q", "move_up": "alt+k", "move_down": "alt+j", "errors": [], "window": "ctrl+g"}"#;
    app.keys = crate::hotkeys::Keymap::from_config(&serde_json::from_str(keys).unwrap()).0;
    app.dialog = Some(Dialog::Help);
    let rows = draw(&mut app, 120, 60).0;
    let text = screen(&rows);
    assert_help_columns_apart(&rows, &app);
    assert!(beside(&rows, "Ctrl-q", "close selected session"), "{text}");
    assert!(beside(&rows, "Alt+k", "move the session up the list"), "{text}");
    assert!(beside(&rows, ":errors", "errors"), "{text}");
    assert!(text.contains("Ctrl-g w") && !text.contains("Ctrl-w"), "{text}");
    assert!(text.contains("While typing, Ctrl-g then any other key go to the program."), "{text}");
    assert!(text.contains("Change them under \"keys\" in "), "the full sheet remains readable: {text}");

    // A key wider than the label column still has a space before its text.
    app.keys = crate::hotkeys::Keymap::from_config(&serde_json::from_str(r#"{"stop": "ctrl+alt+f1"}"#).unwrap()).0;
    let rows = draw(&mut app, 120, 60).0;
    assert_help_columns_apart(&rows, &app);
}

#[test]
fn scrolled_frame_hides_cursor_and_shows_position() {
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    let frame = ScreenFrame {
        cols: 80,
        rows: 24,
        offset: 5,
        history: 100,
        cursor: Some(CursorPos { x: 0, y: 0, shape: 0 }),
        ..ScreenFrame::default()
    };
    app.frame = Some((1, frame));
    let (rows, cursor) = draw(&mut app, 120, 30);
    assert_eq!(cursor, None);
    assert!(rows[0].contains("↑ 5 of 100 lines"), "{}", rows[0]);
}

#[test]
fn f2_lists_errors_newest_first_and_the_footer_counts_the_unseen() {
    // Owner, 2026-10-26: "a f2 in the agent-desk itself for errors say for
    // example if the dev server fails to start".
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    let now = crate::util::now_secs();
    app.state.errors = vec![
        crate::protocol::LogEntry::new(now - 600, "Claude \u{201c}Fix login\u{201d} exited with status 1"),
        crate::protocol::LogEntry::new(
            now - 5,
            "Dev server failed (exit 1 after 2s): Error: That port is already in use.\nWatching for file changes\nError: That port is already in use.",
        ),
    ];
    let footer = |app: &mut App| draw(app, 120, 30).0.last().unwrap().clone();
    assert!(footer(&mut app).contains("F2 2 errors ·"), "{}", footer(&mut app));
    app.dialog = Some(Dialog::Errors { scroll: 0 });
    let render_started_at = crate::util::now_secs();
    let rows = draw(&mut app, 120, 30).0;
    let render_finished_at = crate::util::now_secs();
    let text = screen(&rows);
    assert!(text.contains("Errors") && text.contains("2 errors"), "{text}");
    let dev = rows.iter().position(|r| r.contains("Dev server failed")).expect("the dev server error");
    let exit = rows.iter().position(|r| r.contains("Fix login\u{201d} exited")).expect("the exit");
    assert!(dev < exit, "newest first: {text}");
    // Rendering can cross a second while other tests run. Both displayed
    // ages must still match the same instant within this render's interval.
    assert!(
        (render_started_at..=render_finished_at).any(|at| {
            rows[dev].contains(&format!("{} ago", short_duration(at - (now - 5))))
                && rows[exit].contains(&format!("{} ago", short_duration(at - (now - 600))))
        }),
        "{text}"
    );
    assert!(rows[dev + 1].contains("Watching for file changes"), "every line of its output: {text}");
    // Seen errors leave the badge; a new one brings it back.
    app.errors_seen_at = now - 5;
    app.dialog = None;
    assert!(!footer(&mut app).contains("F2"), "{}", footer(&mut app));
    app.state
        .errors
        .push(crate::protocol::LogEntry::new(now, "Codex \u{201c}Port parser\u{201d} exited with status 2"));
    assert!(footer(&mut app).contains("F2 1 error ·"), "{}", footer(&mut app));
    // Typing into a session F2 is the session's, so the footer only counts.
    app.mode = Mode::Insert;
    let typing = footer(&mut app);
    assert!(typing.contains(" 1 error ·") && !typing.contains("F2"), "{typing}");
    app.mode = Mode::Normal;
    // Cleared, the list is empty.
    app.errors_cleared_at = now;
    app.dialog = Some(Dialog::Errors { scroll: 0 });
    let text = screen(&draw(&mut app, 120, 30).0);
    assert!(text.contains("No errors") && !text.contains("2 errors"), "{text}");
}

#[test]
fn an_error_that_keeps_happening_is_one_f2_entry_with_its_count() {
    // Owner, 2026-11: "non stop errors that i shouldnt be getting".
    let (mut app, _rx) = app_with(vec![], vec![]);
    let now = crate::util::now_secs();
    app.state.errors =
        vec![crate::protocol::LogEntry { repeats: 4, ..crate::protocol::LogEntry::new(now, "Fedora: lost") }];
    for _ in 0..3 {
        app.set_notice("Dev server not started: something already listens on port 5173.", true);
    }
    assert_eq!(app.local_errors.len(), 1);
    assert_eq!(app.unseen_errors(), 2);
    app.dialog = Some(Dialog::Errors { scroll: 0 });
    let text = screen(&draw(&mut app, 120, 30).0);
    assert_eq!(text.matches("Fedora: lost").count(), 1, "{text}");
    let entry = |what: &str, count: &str| text.lines().any(|row| row.contains(what) && row.contains(count));
    assert!(entry("Fedora: lost", "×5") && entry("port 5173.", "×3"), "{text}");
}

#[test]
fn hidden_sidebar_gives_the_pane_the_full_width() {
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    app.sidebar = false;
    let frame = ScreenFrame {
        cols: 120,
        rows: 29,
        lines: vec![vec![Run { c: 0, w: 1, fg: 0, bg: 0, fl: 0, t: "top-left".into(), g: false }]],
        ..ScreenFrame::default()
    };
    app.frame = Some((1, frame));
    let (rows, _) = draw(&mut app, 120, 30);
    assert!(rows[0].starts_with("top-left"));
    assert!(app.row_hits.is_empty());
}

#[test]
fn tiny_window_does_not_panic() {
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    let (rows, cursor) = draw(&mut app, 20, 3);
    assert!(rows[0].starts_with("cepheus"));
    assert_eq!(cursor, None);
    app.dialog = Some(Dialog::Help);
    draw(&mut app, 40, 6);
}

type CellStyle = (String, Option<Color>, Option<Color>);

fn gauge_cells(spans: &[Span<'static>]) -> Vec<CellStyle> {
    spans.iter().flat_map(|s| s.content.chars().map(move |c| (c.to_string(), s.style.fg, s.style.bg))).collect()
}

#[test]
fn gauge_fills_with_what_is_left_in_level_colours() {
    // Owner, 2026-10-26: "it shows 1% when i have 99% usage remianing". The
    // bar is a fuel gauge: full when nothing is used, coloured by how much is.
    let t = owner_theme();
    let fill = |c: Color| ("█".to_string(), Some(c), None);
    let track = ("█".to_string(), Some(t.faint), None);
    assert_eq!(gauge_cells(&gauge(0.0, 4, &t)), vec![fill(t.ok); 4]);
    assert_eq!(gauge_cells(&gauge(100.0, 4, &t)), vec![track.clone(); 4]);
    assert_eq!(gauge_cells(&gauge(50.0, 4, &t)), vec![fill(t.ok), fill(t.ok), track.clone(), track.clone()]);
    // The partial cell is drawn over the track colour so the bar stays continuous.
    assert_eq!(gauge_cells(&gauge(87.5, 2, &t)), vec![("▎".to_string(), Some(t.bad), Some(t.faint)), track.clone()]);
    assert_eq!(gauge_cells(&gauge(150.0, 3, &t)), vec![track.clone(); 3]);
    assert_eq!(gauge_cells(&gauge(-20.0, 3, &t)), vec![fill(t.ok); 3]);
    assert_eq!(gauge_cells(&gauge(70.0, 4, &t))[0].1, Some(t.warn));
}

fn owner_theme() -> Theme {
    let entries: Vec<(String, String)> = [
        ("foreground", "#b9bec4"),
        ("background", "#131416"),
        ("color0", "#131416"),
        ("color1", "#df6565"),
        ("color2", "#82ad4d"),
        ("color3", "#b39362"),
        ("color4", "#3692e7"),
        ("color9", "#ef8585"),
        ("color10", "#a2c575"),
        ("color11", "#d2b385"),
        ("color12", "#65b1f4"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    Theme::from_palette(&crate::config::Palette::from_kitty_entries(&entries))
}

fn busy_app() -> App {
    let now = crate::util::now_secs();
    let mut working = session(1, Tool::Claude, Status::Working, true);
    working.worktree =
        Some(Worktree { path: "/r/repo-cepheus".into(), branch: None, task: Some("deck".into()), exists: true });
    working.containers = vec![ContainerInfo { name: "c".into(), state: "running".into(), status: "Up".into() }];
    working.review = true;
    working.status_since = now;
    let mut needs = session(2, Tool::Codex, Status::NeedsYou, true);
    needs.detail = "permission".into();
    let mut gone = session(4, Tool::Codex, Status::Ended, false);
    gone.worktree =
        Some(Worktree { path: "/r/repo-old".into(), branch: None, task: Some("old".into()), exists: false });
    let sessions = vec![
        working,
        needs,
        session(3, Tool::Claude, Status::Finished, true),
        gone,
        session(5, Tool::Claude, Status::Idle, true),
        session(6, Tool::Codex, Status::Starting, true),
    ];
    let (mut app, _rx) = app_with(sessions, vec![outside(77, None)]);
    app.state.usage.claude = vec![
        UsageWindow { label: "5h".into(), used_percent: Some(42.0), resets_at: Some(now + 3600), updated_at: now },
        UsageWindow { label: "wk".into(), used_percent: Some(90.0), resets_at: Some(now + 86400 * 3), updated_at: now },
    ];
    app.state.usage.codex =
        vec![UsageWindow { label: "wk".into(), used_percent: Some(65.0), resets_at: None, updated_at: now }];
    app.theme = owner_theme();
    app
}

#[test]
fn chrome_uses_only_theme_colours() {
    let mut app = busy_app();
    let t = app.theme.clone();
    let allowed = [
        Color::Reset,
        t.accent,
        t.dim,
        t.faint,
        t.selected_bg,
        t.surface,
        t.on_color,
        t.claude,
        t.codex,
        t.antigravity,
        t.grok,
        t.opencode,
        t.working,
        t.needs,
        t.finished,
        t.task,
        t.review,
        t.ok,
        t.warn,
        t.bad,
    ];
    let conv = crate::protocol::Conversation {
        search_words: Vec::new(),
        tool: Tool::Claude,
        id: "a".into(),
        title: "Port parser".into(),
        created_at: 0,
        updated_at: 0,
        open_in: Some(1),
        host: None,
    };
    let dialogs = || {
        vec![
            None,
            Some(Dialog::New { sel: 0, model: 0, effort: 0 }),
            Some(Dialog::Confirm { text: "Stop the deck service?".into(), action: crate::tui::Action::Shutdown }),
            Some(Dialog::Picker {
                items: vec![conv.clone()],
                sel: 0,
                filter: String::new(),
                loading: false,
                search_due: None,
                search_request: None,
            }),
            Some(Dialog::Help),
            Some(Dialog::Errors { scroll: 0 }),
            Some(Dialog::Broadcast { text: String::new(), targets: vec![(1, true), (2, false)], sel: 1 }),
            Some(Dialog::Broadcast { text: "line one\nline two".into(), targets: vec![(1, true)], sel: 0 }),
        ]
    };
    let selections = [Some(Sel::Session(1)), Some(Sel::Session(4)), Some(Sel::Outside(77)), None];
    for mode in [Mode::Normal, Mode::Insert, Mode::Command] {
        for current in selections {
            for dialog in dialogs() {
                app.mode = mode;
                app.current = current;
                app.dialog = dialog;
                app.notice = None;
                let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
                term.draw(|f| {
                    render(f, &mut app);
                })
                .unwrap();
                let buf = term.backend().buffer();
                for y in 0..30 {
                    for x in 0..120 {
                        let cell = &buf[(x, y)];
                        assert!(
                            allowed.contains(&cell.fg) && allowed.contains(&cell.bg),
                            "{mode:?} {current:?}: cell ({x},{y}) {:?} uses fg {:?} bg {:?}",
                            cell.symbol(),
                            cell.fg,
                            cell.bg
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn keep_tail_keeps_the_end_in_width() {
    assert_eq!(keep_tail("short", 10), "short");
    assert_eq!(keep_tail("abcdefghij", 5), "…ghij");
    assert_eq!(keep_tail("日本語テキスト", 6), "…スト", "wide characters count two cells");
}

#[test]
fn long_card_values_stay_beside_their_labels() {
    let (mut app, _rx) = app_with(vec![], vec![outside(77, Some("conv-x"))]);
    let tree = "/srv/someone/projects/example/app-agent-a-rather-long-task-name-for-a-card";
    app.state.outside[0].worktree = Some(Worktree {
        path: tree.into(),
        branch: None,
        task: Some("a-rather-long-task-name-for-a-card".into()),
        exists: true,
    });
    app.current = Some(Sel::Outside(77));
    let rows = draw(&mut app, 160, 30).0;
    let row = rows.iter().find(|r| r.contains("Worktree")).expect("a worktree row");
    assert!(row.contains(tree), "the path wraps away from its label: {}", screen(&rows));
}

#[test]
fn paths_under_home_are_shown_from_tilde() {
    assert_eq!(home_path("/home/u/projects/x", Some("/home/u")), "~/projects/x");
    assert_eq!(home_path("/home/u", Some("/home/u")), "~");
    assert_eq!(home_path("/home/user2/x", Some("/home/u")), "/home/user2/x", "a sibling home is not home");
    assert_eq!(home_path("/srv/x", Some("/")), "/srv/x");
    assert_eq!(home_path("/home/u/x", None), "/home/u/x");
}

#[test]
fn hints_highlight_marked_keys() {
    let t = owner_theme();
    let spans = hint_line("{Enter} or {Alt+r} resumes · {q} back", &t);
    let parts: Vec<(&str, Option<Color>)> = spans.iter().map(|s| (s.content.as_ref(), s.style.fg)).collect();
    assert_eq!(
        parts,
        vec![
            ("Enter", Some(t.accent)),
            (" or ", Some(t.dim)),
            ("Alt+r", Some(t.accent)),
            (" resumes", Some(t.dim)),
            (" · ", Some(t.faint)),
            ("q", Some(t.accent)),
            (" back", Some(t.dim)),
        ]
    );
}

#[test]
fn the_pairing_screen_draws_the_code_it_opened_dark_on_light() {
    let dir = crate::testsupport::TempDir::new("view-pairing");
    let identity = crate::remote::store::load_or_create_identity(dir.path(), "198.51.100.102").unwrap();
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    let cfg = crate::config::Config {
        remote: true,
        remote_listen: "198.51.100.102:8765".into(),
        ..crate::config::Config::default()
    };
    let pairing = Pairing::open(dir.path(), &cfg, crate::util::now_secs());
    let offer = pairing.offer.as_ref().unwrap();
    let (modules, code) = (offer.modules.clone(), offer.code.clone());
    app.dialog = Some(Dialog::Pair(Box::new(pairing)));

    let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
    term.draw(|f| {
        render(f, &mut app);
    })
    .unwrap();
    let buf = term.backend().buffer();
    let (white, black) = (Color::Rgb(255, 255, 255), Color::Rgb(0, 0, 0));
    let light: Vec<(u16, u16)> =
        (0..30).flat_map(|y| (0..120).map(move |x| (x, y))).filter(|&(x, y)| buf[(x, y)].bg == white).collect();
    let (x0, y0) = (light.iter().map(|c| c.0).min().unwrap(), light.iter().map(|c| c.1).min().unwrap());
    // Read the modules back from the half blocks: the drawn code is the
    // code encoded, the right way up.
    for (row, pair) in modules.chunks(2).enumerate() {
        for (x, &top) in pair[0].iter().enumerate() {
            let cell = &buf[(x0 + x as u16, y0 + row as u16)];
            assert_eq!((cell.fg, cell.bg), (black, white), "cell ({x}, {row})");
            let symbol = cell.symbol();
            let bottom = pair.get(1).is_some_and(|r| r[x]);
            assert_eq!((matches!(symbol, "▀" | "█"), matches!(symbol, "▄" | "█")), (top, bottom), "({x}, {row})");
        }
    }
    assert_eq!(light.len(), modules.len() * modules.len().div_ceil(2), "nothing else is drawn on light");
    let rows: Vec<String> = (0..30).map(|y| (0..120).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect();
    let text = screen(&rows);
    assert!(text.contains(&code) && text.contains("198.51.100.102:8765"), "{text}");
    assert!(text.contains(&identity.fingerprint[..4]), "{text}");
    assert!(text.contains("None yet."), "{text}");

    // Too small a window for the code: the details alone.
    let (rows, _) = draw(&mut app, 80, 24);
    let text = screen(&rows);
    assert!(text.contains(&code) && text.contains("Make the window larger"), "{text}");
    assert!(!text.contains('█'), "{text}");
}

// ---- the 2026-10-30 review of the deck's list and footer ----

#[test]
fn the_list_says_what_is_out_of_view() {
    let mut sessions: Vec<SessionInfo> = (1..=40).map(|i| session(i, Tool::Claude, Status::Idle, true)).collect();
    sessions[38].status = Status::NeedsYou;
    sessions[39].status = Status::Finished;
    let (mut app, _rx) = app_with(sessions, vec![]);
    app.current = Some(Sel::Session(1));
    let rows = draw(&mut app, 120, 30).0;
    let side: Vec<String> = rows.iter().map(|r| r.chars().take(34).collect()).collect();
    let last = side[28].clone();
    assert!(last.contains("↓") && last.contains("more") && last.contains("1 needs you"), "{}", screen(&side));
    assert!(last.contains("1 ✓"), "{last}");
    assert!(!side.iter().any(|r| r.contains('↑')), "nothing above: {}", screen(&side));
    app.current = Some(Sel::Session(40));
    let rows = draw(&mut app, 120, 30).0;
    let side: Vec<String> = rows.iter().map(|r| r.chars().take(34).collect()).collect();
    assert!(side.iter().any(|r| r.contains("↑") && r.contains("more")), "{}", screen(&side));
    assert!(side.iter().any(|r| r.contains("task 40")), "the selection stays in view: {}", screen(&side));
}

#[test]
fn empty_remote_hosts_do_not_advertise_more_sessions() {
    // Two headings, a section spacer, twelve two-line sessions and the footer need 28 rows.
    for height in [28, 29, 30] {
        let sessions = (1..=12).map(|id| session(id, Tool::Codex, Status::Idle, true)).collect();
        let (mut app, _) = app_with(sessions, vec![]);
        app.sidebar_order = Some(vec!["sessions".into()]);
        assert_eq!(sidebar_parts(&app), vec!["cloud".to_string(), "sessions".to_string()]);
        app.hosts = vec![
            crate::tui::tests::host("remote-a", "Remote A", true),
            crate::tui::tests::host("remote-b", "Remote B", false),
        ];
        let rows = draw(&mut app, 120, height).0;
        let side: Vec<String> = rows.iter().map(|r| r.chars().take(34).collect()).collect();
        assert_eq!(app.row_hits.len(), 12, "all sessions fit: {}", screen(&side));
        assert!(app.row_hits.iter().all(|(_, height, _)| *height == 2), "every session retains both rows");
        assert!(!screen(&side).contains("more"), "no sessions remain below: {}", screen(&side));
    }
}

#[test]
fn the_selected_row_keeps_its_two_rows() {
    // Owner, 2026-11-01, of the highlighted row wrapping its whole title:
    // "expands making it very hard to navigate and see where i am".
    let mut long = session(1, Tool::Claude, Status::Working, true);
    long.title = "Rework the sidebar so that long session titles can be read in full".into();
    long.worktree = Some(Worktree {
        path: "/r/project-agent-a-rather-long-worktree-name".into(),
        branch: None,
        task: Some("a-rather-long-worktree-name".into()),
        exists: true,
    });
    let (mut app, _rx) = app_with(vec![long, session(2, Tool::Codex, Status::Idle, true)], vec![]);
    let layout = |app: &mut App| {
        let rows = draw(app, 120, 30).0;
        let side = rows.iter().map(|r| r.chars().take(34).collect::<String>()).collect::<Vec<_>>().join("\n");
        (app.row_hits.clone(), side)
    };
    app.current = Some(Sel::Session(2));
    let (before, _) = layout(&mut app);
    app.current = Some(Sel::Session(1));
    let (after, side) = layout(&mut app);
    assert_eq!(before, after, "highlighting a row moves no row: {side}");
    assert!(after.iter().all(|(_, height, _)| *height == 2), "{after:?}");
    assert!(side.contains("Rework the sidebar") && !side.contains("in full"), "the title is cut: {side}");
}

#[test]
fn the_deck_numbers_sessions_as_its_list_does() {
    // The list numbers rows 1, 2, 3…; the footer, the message dialog and
    // the ended card used the service's ids ("#12") instead.
    let mut app = busy_app();
    app.current = Some(Sel::Session(5));
    app.mode = Mode::Insert;
    app.sidebar = false;
    let footer = draw(&mut app, 160, 30).0.last().unwrap().clone();
    assert!(footer.contains("4 Claude") && !footer.contains('#'), "{footer}");
    app.sidebar = true;
    app.mode = Mode::Normal;
    app.dialog = Some(Dialog::Broadcast { text: String::new(), targets: vec![(1, true), (5, true)], sel: 0 });
    let text = screen(&draw(&mut app, 120, 30).0);
    assert!(text.contains("●  1 ") && text.contains("●  4 ") && !text.contains("●  5 "), "{text}");
    app.dialog = None;
    app.current = Some(Sel::Session(4));
    let text = screen(&draw(&mut app, 120, 30).0);
    assert!(text.contains("Program       Codex") && !text.contains("#4"), "{text}");
}

#[test]
fn a_notice_with_further_lines_reads_as_one_footer_line() {
    // A broadcast result names each host on its own line; the footer once
    // ran them together ("…1 hostOmarchy: 1 delivered").
    let (mut app, _rx) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    app.set_notice("Sent to 1 agent across 1 host; 1 host did not confirm delivery\nOmarchy: 1 delivered\nFedora: host is offline; nothing sent.", true);
    let rows = draw(&mut app, 200, 30).0;
    let footer = rows.last().unwrap();
    assert!(footer.contains("delivery · Omarchy: 1 delivered · Fedora: host is offline"), "{footer}");
}

#[test]
fn the_list_window_keeps_the_selection_and_room_for_what_is_out_of_view() {
    let w = |offset, shown, top, bottom| ListWindow { offset, shown, top, bottom };
    assert_eq!(list_window(10, 20, Some((8, 10))), w(0, 10, false, false), "all of it fits");
    assert_eq!(list_window(50, 20, Some((2, 4))), w(0, 19, false, true));
    assert_eq!(list_window(50, 20, Some((24, 26))), w(8, 18, true, true));
    assert_eq!(list_window(50, 20, Some((48, 50))), w(31, 19, true, false), "the end needs no line below");
    assert_eq!(list_window(50, 20, None), w(0, 19, false, true));
    assert_eq!(list_window(50, 2, Some((40, 42))), w(0, 2, false, false), "too short for the lines");
}

#[test]
fn remote_outside_card_shows_its_host_and_real_pid() {
    let mut o = outside(crate::protocol::host_pid(1, 2973235), None);
    o.host = Some("build-worker".into());
    o.kitty_pid = None;
    o.kitty_window = None;
    o.workspace = None;
    let pid = o.pid;
    let (mut app, _) = app_with(vec![], vec![o]);
    app.current = Some(Sel::Outside(pid));
    let text = screen(&draw(&mut app, 140, 35).0);
    assert!(text.contains("Host") && text.contains("build-worker"), "{text}");
    assert!(text.contains("pid 2973235"), "{text}");
    assert!(!text.contains("7167539") && !text.contains("not in kitty"), "{text}");
    assert!(!text.contains("goes to its window"), "{text}");
}

#[test]
fn script_picker_keeps_status_name_and_detail_colors_when_selected_and_filtered() {
    let dir = crate::testsupport::TempDir::new("colored-picker");
    let (mut app, _rx) = app_with(vec![session(1, Tool::Codex, Status::Idle, true)], vec![]);
    app.load_lua(
        crate::lua::Places { config_dir: dir.path().join("config"), start_dir: dir.path().into() },
        std::sync::Arc::new(|_| {}),
    );
    app.lua_eval(
        r#"deck.ui.pick({title = "Tasks", items = {
        {{"✓ ready   ", "ok"}, {"alpha", "task"}, {"  merge it", "dim"}},
        {{"● active  ", "working"}, {"beta", "task"}, {"  build", "dim"}}
    }, on_choose = function(item, i) chosen_item, chosen_index = item, i end})"#,
    );
    for filter in ["", "beta"] {
        if !filter.is_empty() {
            crate::tui::tests::press(
                &mut app,
                crossterm::event::KeyCode::Char('/'),
                crossterm::event::KeyModifiers::NONE,
            );
        }
        for c in filter.chars() {
            crate::tui::tests::press(
                &mut app,
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            );
        }
        let mut term = Terminal::new(TestBackend::new(140, 30)).unwrap();
        term.draw(|f| {
            render(f, &mut app);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let name = if filter.is_empty() { "alpha" } else { "beta" };
        let (x, y) = (0..30)
            .find_map(|y| {
                let row = (0..140).map(|x| buf[(x, y)].symbol()).collect::<String>();
                row.contains(if filter.is_empty() { "merge it" } else { "build" })
                    .then(|| row.find(name).map(|byte| (row[..byte].chars().count() as u16, y)))
                    .flatten()
            })
            .unwrap();
        assert_eq!(buf[(x, y)].fg, app.theme.task, "task names keep their color even on the selected row");
        assert_eq!(buf[(x - 10, y)].fg, if filter.is_empty() { app.theme.ok } else { app.theme.working });
        let detail_x = x + name.len() as u16 + 2;
        assert_eq!(buf[(detail_x, y)].fg, app.theme.dim);
        assert_eq!(buf[(x, y)].bg, app.theme.selected_bg);
    }
    crate::tui::tests::press(&mut app, crossterm::event::KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
    let lua = app.lua.as_mut().unwrap();
    assert_eq!(lua.eval("=chosen_item"), Some("● active  beta  build".into()));
    assert_eq!(lua.eval("=chosen_index"), Some("2".into()), "filtered choice keeps its original index");
}

#[test]
fn the_new_dialog_with_no_enabled_providers_remains_usable() {
    use crate::tui::tests::press;
    use crossterm::event::{KeyCode, KeyModifiers};
    let (mut app, rx) = app_with(vec![], vec![]);
    for tool in crate::protocol::ALL_TOOLS.into_iter().filter(|tool| tool.is_agent()) {
        app.cfg.plugins.insert(format!("provider-{}", tool.key()), false);
    }
    app.run_command("new");
    let text = screen(&draw(&mut app, 120, 30).0);
    assert!(text.contains("No agent providers are enabled."), "{text}");
    assert!(text.contains("Enable a provider plugin"), "{text}");
    for code in [
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Enter,
        KeyCode::Char('m'),
        KeyCode::Char('e'),
        KeyCode::Char('c'),
        KeyCode::Right,
    ] {
        press(&mut app, code, KeyModifiers::NONE);
        assert!(matches!(app.dialog, Some(Dialog::New { .. })), "{code:?}");
    }
    assert!(rx.try_iter().next().is_none(), "No unavailable provider may start");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.dialog.is_none(), "Escape closes the empty chooser");
}

#[test]
fn cloud_connections_sit_above_local_agents_and_leave_with_the_last_one_off() {
    let mut cloud = session(9, Tool::Dynamic("dot"), Status::Idle, true);
    cloud.title = "Cloud thread".into();
    let (mut app, _) = app_with(vec![session(1, Tool::Claude, Status::Idle, true), cloud.clone()], vec![]);
    app.state.providers.push(crate::providers::Info {
        id: "dot".into(),
        label: "Dot".into(),
        cloud: true,
        ..Default::default()
    });
    app.sidebar_order = Some(vec!["sessions".into()]);
    let text = screen(&draw(&mut app, 120, 35).0);
    assert!(text.find("CLOUD AGENTS").unwrap() < text.find("SESSIONS").unwrap());
    assert!(!text.contains("LOCAL AGENTS"));
    // The live Dot shows once, under its own name, and not also as off.
    assert_eq!(text.matches("◇   Dot").count(), 1);
    assert!(!text.contains("off · :dot on"));
    assert_eq!(app.rows(), [Sel::Session(9), Sel::Session(1)]);
    // `:dot off` closes the only cloud agent: its row and the section go.
    app.state.sessions.retain(|s| s.id != 9);
    let text = screen(&draw(&mut app, 120, 35).0);
    assert!(!text.contains("CLOUD AGENTS") && !text.contains("Dot"), "{text}");
    assert_eq!(app.rows(), [Sel::Session(1)]);
    // `:dot on` brings both back.
    app.state.sessions.push(cloud);
    let text = screen(&draw(&mut app, 120, 35).0);
    assert!(text.find("CLOUD AGENTS").unwrap() < text.find("SESSIONS").unwrap());
    assert_eq!(text.matches("◇   Dot").count(), 1);
    app.state.sessions.retain(|s| s.id != 9);
    // Other hosts add no heading of their own (owner: "titles are duplicated").
    app.hosts = vec![crate::tui::tests::host("fedora", "Fedora", true)];
    let text = screen(&draw(&mut app, 120, 35).0);
    assert_eq!(text.matches("SESSIONS").count(), 1, "{text}");
    assert!(!text.contains("LOCAL AGENTS") && !text.contains("FEDORA"), "{text}");
}

#[test]
fn the_sidebar_leaves_slots_the_queue_and_jobs_to_their_own_windows() {
    // Owner of the sidebar's queue, chores and slots block: "this is noisey
    // and annoying"; of its Jobs block: "random jobs text too".
    let (mut app, _) = app_with(vec![session(1, Tool::Claude, Status::Idle, true)], vec![]);
    assert_eq!(sidebar_parts(&app), ["title", "resources", "dev", "cloud", "sessions"]);
    app.state.work_queue_capable = true;
    app.state.job_capable = true;
    for order in [None, Some(vec!["slots".to_string(), "jobs".into(), "sessions".into()])] {
        app.sidebar_order = order;
        let text = screen(&draw(&mut app, 120, 30).0);
        assert!(!text.contains("Queue") && !text.contains("Jobs") && !text.contains(":job"), "{text}");
    }
}

#[test]
fn the_sidebar_lists_no_ended_sessions_or_agents_outside_the_deck() {
    // Owner, 2026-11-04: "i dont want to see whats ended or agents outside of the deck".
    let sessions = vec![session(1, Tool::Claude, Status::Idle, true), session(2, Tool::Codex, Status::Ended, false)];
    let (mut app, _) = app_with(sessions, vec![outside(77, None)]);
    // An order written when the sidebar had an outside part still draws none.
    app.sidebar_order = Some(vec!["sessions".into(), "outside".into()]);
    let text = screen(&draw(&mut app, 120, 30).0);
    assert!(text.contains("SESSIONS  1 live") && text.contains("task 1"), "{text}");
    assert!(!text.contains("task 2") && !text.contains("outside work"), "{text}");
    assert_eq!(app.rows(), [Sel::Session(1)]);
}

// Owner, 2026-11-06: Dot must look like an agent, with its own name and pet.
#[test]
fn dot_shows_its_own_name_and_pet() {
    let (mut app, _) = app_with(vec![session(9, Tool::Dynamic("dot"), Status::Idle, true)], vec![]);
    app.state.providers.push(crate::providers::Info {
        id: "dot".into(),
        label: "Dot".into(),
        cloud: true,
        ..Default::default()
    });
    app.dot.profile = Some(crate::pet::Profile { name: Some("AndroDot".into()), ..Default::default() });
    app.dot.art = Some(crate::tui::dot_chat::ArtShape::Animated);
    let mut term = Terminal::new(TestBackend::new(120, 35)).unwrap();
    term.draw(|f| {
        render(f, &mut app);
    })
    .unwrap();
    let buf = term.backend().buffer();
    let text: String = buf.content.iter().map(|c| c.symbol()).collect();
    assert!(text.contains("AndroDot"));
    // Ready, the pet is its ready animation: two rows of four kitty
    // placeholder cells in that image's colour.
    let cells: Vec<_> = buf.content.iter().filter(|c| c.symbol().starts_with('\u{10EEEE}')).collect();
    assert_eq!(cells.len(), 8);
    assert!(cells.iter().all(|c| c.fg == ratatui::style::Color::Rgb(0x0d, 0x07, 0x02)));
}

#[test]
fn branch_columns_stay_aligned_whatever_the_commit_age() {
    // Owner screenshot, 2026-11-05: rows whose last commit was ten hours or
    // more ago ("10h44m") pushed the subject column one cell right.
    let (mut app, _) = app_with(vec![], vec![]);
    let now = crate::util::now_secs();
    let branch = |name: &str, path: &str, age: u64, subject: &str| crate::protocol::BranchInfo {
        branch: name.into(),
        path: path.into(),
        committed_at: Some(now - age),
        subject: Some(subject.into()),
        ahead: Some(u32::from(!path.is_empty())),
        behind: Some(if path.is_empty() { 160 } else { 0 }),
        ..Default::default()
    };
    app.state.branches = vec![
        crate::protocol::BranchInfo { main: true, ..branch("master", "/w", 8 * 3600 + 19 * 60, "subject-main") },
        branch("agent/fresh", "/w/fresh", 9 * 60, "subject-fresh"),
        branch("agent/old", "", 10 * 3600 + 44 * 60, "subject-old"),
        branch("agent/older", "", 2 * 86400, "subject-older"),
    ];
    app.dialog = Some(Dialog::Branches { sel: 0, filter: String::new() });
    let rows = draw(&mut app, 160, 40).0;
    let column = |subject: &str| {
        let row = rows.iter().find(|r| r.contains(subject)).unwrap_or_else(|| panic!("{}", screen(&rows)));
        row.find(subject).map(|byte| row[..byte].chars().count())
    };
    let main = column("subject-main");
    for subject in ["subject-fresh", "subject-old", "subject-older"] {
        assert_eq!(column(subject), main, "{subject} starts in the subject column: {}", screen(&rows));
    }
}

#[test]
fn a_long_error_wraps_under_its_own_text() {
    // Owner screenshot, 2026-11-05: the end of a long error ran on from the
    // window's left edge, under the ages.
    let (mut app, _) = app_with(vec![], vec![]);
    let words: Vec<String> = (0..40).map(|i| format!("word{i:02}")).collect();
    app.local_errors.push(crate::protocol::LogEntry::new(crate::util::now_secs(), words.join(" ")));
    app.dialog = Some(Dialog::Errors { scroll: 0 });
    let rows = draw(&mut app, 120, 30).0;
    let first = rows.iter().position(|r| r.contains("word00")).expect("the error");
    let text_at = rows[first].find("word00").map(|byte| rows[first][..byte].chars().count()).unwrap();
    let mut seen = 1;
    for row in &rows[first + 1..] {
        let Some(byte) = row.find("word") else { break };
        assert_eq!(row[..byte].chars().count(), text_at, "continued under the text: {}", screen(&rows));
        seen += 1;
    }
    assert!(seen > 1 && screen(&rows).contains("word39"), "it wrapped and all of it shows: {}", screen(&rows));
}

/// Dot's chat pane drawn alone, `width` x `height`: its rows, its cells and
/// the composer cursor.
fn dot_pane(app: &App, width: u16, height: u16) -> (Vec<String>, ratatui::buffer::Buffer, Option<(u16, u16, u8)>) {
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut cursor = None;
    let theme = app.theme.clone();
    term.draw(|f| cursor = dot_chat::pane(f, f.area(), app, &theme)).unwrap();
    let buf = term.backend().buffer().clone();
    let rows = (0..height).map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect();
    (rows, buf, cursor)
}

const DOT_REPLY: &str = "## Plan\n\nThe **first** step uses `cargo`, see [the guide](https://example.com/guide).\n\n\
    1. Build it\n2. Ship *later*\n\n- a bullet that is long enough to wrap across the narrow pane width\n\n\
    > quoted words that also run long enough to wrap onto a second row\n\n```\nlet x = 1;\n```";

fn dot_conversation(status: Status) -> App {
    use crate::remote::chat::Item;
    let (mut app, _rx) = app_with(vec![session(1, Tool::Dynamic("dot"), status, true)], vec![]);
    app.dot.profile = Some(crate::pet::Profile { name: Some("AndroDot".into()), ..Default::default() });
    app.dot.items = vec![
        Item::User { client_id: None, text: "Summarise the plan\nwith steps".into(), images: vec![], at: None },
        Item::Assistant { text: DOT_REPLY.into(), questions: vec![], at: None },
        Item::Notice { text: "Delivery uncertain; check ChatGPT, then /resolve.".into() },
    ];
    app
}

/// Every word of `words` shows whole on one row: none is split at a row end.
fn assert_whole_words(rows: &[String], words: &str) {
    for word in words.split_whitespace() {
        assert!(
            rows.iter().any(|row| row.split(|c: char| c.is_whitespace() || c == '│').any(|w| w == word)),
            "{word:?} is split: {}",
            screen(rows)
        );
    }
}

// Owner, 2026-11-08, of the Dot pane: "this UI is pretty ass for the dot";
// 2026-11-06: "this interface is ass" (raw Markdown, words split at line ends).
#[test]
fn dot_chat_renders_markdown_and_tells_the_owner_and_dot_apart() {
    let mut app = dot_conversation(Status::Working);
    app.dot.draft = "line one\nline two".into();
    app.mode = Mode::Insert;
    let (rows, buf, cursor) = dot_pane(&app, 48, 40);
    let text = screen(&rows);
    println!("{text}");
    for marker in ["##", "**", "`", "Ready", "ready", "Working", "working", "needs attention", "! "] {
        assert!(!text.contains(marker), "{marker:?} shows: {text}");
    }
    assert!(text.contains("● Plan"), "{text}");
    assert!(text.contains("The first step uses cargo, see the guide"), "{text}");
    assert!(!text.contains("https://example.com/guide"), "a link shows its text alone: {text}");
    assert!(text.contains("1. Build it") && text.contains("2. Ship later"), "{text}");
    assert!(text.contains("• a bullet"), "{text}");
    assert!(text.contains("│ quoted words"), "{text}");
    assert!(text.contains("let x = 1;"), "{text}");
    assert_whole_words(&rows, "a bullet that is long enough to wrap across the narrow pane width");
    assert_whole_words(&rows, "quoted words that also run long enough to wrap onto a second row");
    // A wrapped item continues under its own text, not at the margin.
    let bullet = rows.iter().position(|r| r.contains("• a bullet")).unwrap();
    let column = rows[bullet].chars().position(|c| c == '•').unwrap() + 2;
    let next: Vec<char> = rows[bullet + 1].chars().collect();
    assert!(next[..column].iter().all(|c| *c == ' ') && next[column] != ' ', "{text}");
    // The owner's message sits on a shaded band; Dot's reply does not.
    let theme = &app.theme;
    let mine = rows.iter().position(|r| r.contains("› Summarise the plan")).expect("the owner's message");
    assert!(rows[mine + 1].contains("  with steps"), "{text}");
    let band = |y: usize| (3..45).all(|x| buf[(x, y as u16)].bg == theme.selected_bg);
    assert!(band(mine) && band(mine + 1), "{text}");
    let reply = rows.iter().position(|r| r.contains("● Plan")).unwrap();
    assert!(!band(reply));
    // A failure reads as a failure, in the failure colour.
    let failed = rows.iter().position(|r| r.contains("✗ Delivery uncertain")).expect("the failure");
    let x = rows[failed].chars().position(|c| c == '✗').unwrap() as u16;
    assert_eq!(buf[(x, failed as u16)].fg, theme.bad);
    assert!(text.contains("line one") && text.contains("line two"), "{text}");
    assert!(!text.contains("Message AndroDot"), "a draft replaces the placeholder: {text}");
    assert!(cursor.is_some(), "typing shows the composer cursor");
}

#[test]
fn dot_composer_shows_one_placeholder_and_grows_without_overflowing() {
    let mut app = dot_conversation(Status::Idle);
    let (rows, _, _) = dot_pane(&app, 60, 24);
    let text = screen(&rows);
    assert_eq!(text.matches("Message AndroDot").count(), 1, "{text}");
    assert!(rows[23].contains('╰') && rows[22].contains("› Message AndroDot"), "{text}");
    app.mode = Mode::Insert;
    let words: Vec<String> = (0..120).map(|n| format!("word{n}")).collect();
    app.dot.draft = words.join(" ");
    let (rows, _, cursor) = dot_pane(&app, 60, 24);
    let text = screen(&rows);
    println!("{text}");
    // Six rows of text between the borders, all inside the box.
    let top = rows.iter().position(|r| r.contains('╭')).unwrap();
    assert_eq!(top, 24 - 8, "{text}");
    let left = rows[top].chars().position(|c| c == '╭').unwrap();
    let right = rows[top].chars().position(|c| c == '╮').unwrap();
    for row in &rows[top + 1..23] {
        let chars: Vec<char> = row.chars().collect();
        assert!(chars[left] == '│' && chars[right] == '│', "{text}");
    }
    assert_whole_words(&rows[top + 1..23], &words[100..].join(" "));
    assert!(rows[22].contains("word119"), "the end of the draft stays in view: {text}");
    let (x, y, _) = cursor.unwrap();
    assert_eq!(y, 22);
    assert!((x as usize) < right, "the cursor stays inside the box");
}

#[test]
fn dot_chat_reads_well_narrow_and_wide() {
    let app = dot_conversation(Status::Idle);
    for (width, height) in [(40, 46), (140, 30)] {
        let (rows, _, _) = dot_pane(&app, width, height);
        let text = screen(&rows);
        println!("{width}x{height}\n{text}");
        assert_whole_words(&rows, "a bullet that is long enough to wrap across the narrow pane width");
        let used: Vec<usize> =
            rows.iter().filter(|r| !r.trim().is_empty()).map(|r| r.chars().position(|c| c != ' ').unwrap()).collect();
        // Owner, 2026-11-08: "its not even in the window right, its pushed to
        // the right now". The conversation and composer fill the pane from its
        // left edge, as a local agent's pane does.
        let left = *used.iter().min().unwrap();
        assert!(left <= 1, "{text}");
        let top = rows.iter().position(|r| r.contains('╭')).unwrap();
        assert_eq!(rows[top].chars().position(|c| c == '╭'), Some(1), "{text}");
        assert_eq!(rows[top].chars().position(|c| c == '╮'), Some(width as usize - 2), "{text}");
    }
}

#[test]
fn a_reader_scrolled_up_keeps_their_place_as_dot_replies() {
    use crate::remote::chat::Item;
    let mut app = dot_conversation(Status::Idle);
    for n in 0..30 {
        app.dot.items.push(Item::Assistant { text: format!("reply {n}"), questions: vec![], at: None });
    }
    app.set_pane(Rect::new(0, 0, 60, 20));
    dot_chat::prepare(&mut app);
    assert!(dot_chat::scroll_limit(&app) > 20);
    app.dot.scroll = 12;
    let before = dot_pane(&app, 60, 20).0;
    app.dot.items.push(Item::Assistant { text: "a new reply\nover two lines".into(), questions: vec![], at: None });
    app.dot.generation += 1;
    dot_chat::prepare(&mut app);
    assert_eq!(app.dot.scroll, 12 + 3);
    let after = dot_pane(&app, 60, 20).0;
    assert_eq!(before[1..10], after[1..10], "the same lines stay in view");
    assert!(after[0].contains("↑ 15 /"), "{}", screen(&after));
}

// Owner, 2026-11-07: "the dot still shows working and ready when it doesnt
// work like that, its a always on agent"; 2026-11-07: "its not "omarchy"
// dots live on openai servers".
#[test]
fn dot_shows_no_worker_status_or_host_and_only_real_failures() {
    use crate::remote::chat::Item;
    for status in [Status::Working, Status::Idle, Status::Starting, Status::Finished] {
        let mut app = dot_conversation(status);
        app.state.sessions.push(session(2, Tool::Claude, Status::Idle, true));
        app.state.providers.push(crate::providers::Info {
            id: "dot".into(),
            label: "Dot".into(),
            cloud: true,
            ..Default::default()
        });
        app.hosts = vec![crate::tui::tests::host("fedora", "Fedora", true)];
        app.sidebar_order = Some(vec!["sessions".into()]);
        app.dot.items.retain(|item| !matches!(item, Item::Notice { .. }));
        let rows = draw(&mut app, 120, 30).0;
        let side: Vec<String> = rows.iter().map(|r| r.chars().take(app.cfg.sidebar_width as usize).collect()).collect();
        let name = side.iter().position(|r| r.contains("AndroDot")).expect("Dot's name");
        for word in ["ready", "working", "connecting", "finished", "off", "·", "✓", "Omarchy", "Fedora"] {
            assert!(!side[name].contains(word) && !side[name + 1].contains(word), "{word}: {}", screen(&side));
        }
        assert!(rows.last().unwrap().contains(" 0 working"), "Dot is not counted as working: {}", screen(&rows));
        assert!(rows.last().unwrap().contains(" 0 ✓"), "Dot never finishes: {}", screen(&rows));
        // A real failure shows plainly under the name.
        app.dot.items.push(Item::Notice { text: "Cannot connect to Dot; retrying. Nothing is resent.".into() });
        let rows = draw(&mut app, 120, 30).0;
        let side: Vec<String> = rows.iter().map(|r| r.chars().take(app.cfg.sidebar_width as usize).collect()).collect();
        assert!(side[name + 1].contains("✗ Cannot connect"), "{}", screen(&side));
        // A stale failure from a bridge that is not running does not.
        app.state.sessions.retain(|s| s.id != 1);
        let rows = draw(&mut app, 120, 30).0;
        assert!(!screen(&rows).contains("Cannot connect"), "{}", screen(&rows));
    }
}
