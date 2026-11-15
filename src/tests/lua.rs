use std::sync::mpsc::{self, Receiver};

use super::*;
use crate::testsupport::TempDir;

/// An engine whose config folder is `<dir>/config` and whose start folder is
/// `<dir>/start`, with finished jobs arriving on the receiver.
fn engine(dir: &TempDir) -> (Engine, Receiver<JobDone>) {
    let (tx, rx) = mpsc::channel();
    let sink: JobSink = Arc::new(move |done| {
        let _ = tx.send(done);
    });
    std::fs::create_dir_all(dir.path().join("start")).unwrap();
    let places = Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("start") };
    (Engine::new(places, sink).expect("Lua starts"), rx)
}

/// An engine with `init` as its init.lua, isolating generic Lua tests from
/// the bundled task-source dashboard.
fn loaded(name: &str, init: &str) -> (TempDir, Engine, Receiver<JobDone>) {
    let dir = TempDir::new(name);
    dir.write("config/init.lua", init);
    let (mut e, rx) = engine(&dir);
    e.load_with(&BTreeMap::from([("tasks".into(), false)]));
    (dir, e, rx)
}

fn notice(text: &str) -> Request {
    Request::Notify { text: text.into(), error: false }
}

fn plain(text: &str) -> Segment {
    Segment { text: text.into(), style: String::new() }
}

fn seg(text: &str, style: &str) -> Segment {
    Segment { text: text.into(), style: style.into() }
}

#[test]
fn keymaps_and_commands_from_init_lua_run_command_lines_and_functions() {
    let (_dir, mut e, _rx) = loaded(
        "lua-keys",
        r#"
        local deck = require("deck")
        deck.keymap("alt+c", "new codex", { desc = "Codex here" })
        deck.keymap("alt+c", "new claude", { desc = "Claude here" })
        deck.keymap("f9", function() deck.notify("pressed") end)
        deck.command("fleet", function(args)
          deck.new(args[1] or "codex", { host = args[2], cwd = "/srv/x", name = "n" })
        end, { desc = "on every host", complete = { "claude", "codex" } })
        "#,
    );
    assert_eq!(e.take_errors(), Vec::<String>::new());
    let keys = e.keymaps();
    // The same chord again replaces the first.
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].chord, Chord::parse("alt+c").unwrap());
    assert_eq!(keys[0].action, KeyAction::Command("new claude".into()));
    assert_eq!(keys[0].desc, "Claude here");
    let KeyAction::Lua(callback) = keys[1].action else { panic!("f9 runs a function") };
    e.press(callback);
    assert_eq!(e.take_requests(), vec![notice("pressed")]);

    let commands = e.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!((commands[0].name.as_str(), commands[0].desc.as_str()), ("fleet", "on every host"));
    assert_eq!(commands[0].complete, vec!["claude".to_string(), "codex".into()]);
    assert!(e.run_command("fleet", &["claude".into(), "fedora".into()]));
    assert_eq!(
        e.take_requests(),
        vec![Request::Create {
            tool: "claude".into(),
            host: Some("fedora".into()),
            cwd: Some("/srv/x".into()),
            name: Some("n".into()),
            command: Vec::new(),
        }]
    );
    assert!(!e.run_command("nothing", &[]));
}

#[test]
fn a_script_command_may_not_take_a_built_in_name_or_a_strange_one() {
    let (_dir, mut e, _rx) = loaded("lua-names", "");
    for name in ["new", "wq", "q", "branches", "bad name", "", "x;rm", &"a".repeat(33)] {
        e.eval(&format!("deck.command({name:?}, function() end)"));
        let errors = e.take_errors();
        assert_eq!(errors.len(), 1, "{name:?} was accepted");
    }
    e.eval(r#"deck.command("tasks-2_b", function() end)"#);
    assert_eq!(e.take_errors(), Vec::<String>::new());
    assert_eq!(e.commands().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["tasks-2_b"]);
}

#[test]
fn shipped_agent_updater_runs_only_on_command_with_an_argument_list() {
    let (dir, mut e, _rx) = loaded("agent-update-plugin", include_str!("../../plugins/agent-update.lua"));
    assert!(e.take_errors().is_empty());
    assert!(e.take_requests().is_empty(), "loading never updates or starts a terminal");
    assert!(e.run_command("agent-update", &[]));
    let requests = e.take_requests();
    let [Request::Create { tool, name, command, .. }] = requests.as_slice() else { panic!("{requests:?}") };
    assert_eq!(tool, "shell");
    assert_eq!(name.as_deref(), Some("Agent updates"));
    assert_eq!(
        command,
        &vec![
            std::env::current_exe().unwrap().to_string_lossy().into_owned(),
            "agent-update".into(),
            "--config".into(),
            dir.path().join("config/config.json").to_string_lossy().into_owned()
        ]
    );
    assert!(e.run_command("agent-update", &["check".into(), "local".into()]));
    let requests = e.take_requests();
    let [Request::Create { command, .. }] = requests.as_slice() else { panic!("{requests:?}") };
    assert!(command.ends_with(&["--check".into(), "--local".into()]));
    assert!(e.run_command("agent-update", &["unexpected".into()]));
    assert!(matches!(e.take_requests().as_slice(), [Request::Notify { error: true, .. }]));
}

#[test]
fn shipped_cost_plugin_uses_a_pinned_row_and_remembers_being_disabled() {
    let (dir, mut e, _rx) = loaded("api-cost-plugin", include_str!("../../plugins/api-cost.lua"));
    assert!(e.take_errors().is_empty());
    let sections = e.sections();
    let [section] = sections.as_slice() else { panic!("{sections:?}") };
    assert!(section.pinned && section.sidebar && section.phone && section.header.is_none());
    assert!(section.title.is_empty());
    assert_eq!(section.lines[0].len(), 2);
    assert_eq!(section.lines[0][0].text, " API Cost: ");
    let amount = &section.lines[0][1].text;
    assert!(amount.starts_with('$') && !amount.contains('~'), "{amount}");
    assert!(e.option("sidebar").is_none(), "leave the sidebar order alone");
    assert!(e.run_command("api-cost", &["off".into()]));
    assert!(e.sections().is_empty());
    let (mut again, _rx) = engine(&dir);
    again.load_with(&BTreeMap::from([("tasks".into(), false)]));
    assert!(again.take_errors().is_empty());
    assert!(again.sections().is_empty(), "the next view remembers the setting");
}

#[test]
fn a_section_header_is_right_aligned_inline_or_none_and_nothing_else() {
    let (_dir, mut e, _rx) = loaded(
        "lua-header-place",
        r#"
        deck.ui.section('right', {header = true, lines = {'r'}})
        deck.ui.section('inline', {header = 'inline', lines = {{{'7', 'strong'}}}})
        deck.ui.section('rows', {header = false, lines = {'n'}})
        deck.ui.section('plain', {lines = {'p'}})
        "#,
    );
    assert_eq!(e.take_errors(), Vec::<String>::new());
    let place = |e: &mut Engine, id: &str| e.sections().into_iter().find(|s| s.id == id).map(|s| s.header);
    assert_eq!(place(&mut e, "right"), Some(Some(HeaderPlace::Right)));
    assert_eq!(place(&mut e, "inline"), Some(Some(HeaderPlace::Inline)));
    assert_eq!(place(&mut e, "rows"), Some(None));
    assert_eq!(place(&mut e, "plain"), Some(None));
    assert!(STYLES.contains(&"strong"));
    e.eval("deck.ui.section('odd', {header = 'left', lines = {'x'}})");
    let errors = e.take_errors();
    assert!(errors.iter().any(|m| m.contains("header must be true or \"inline\"")), "{errors:?}");
    assert_eq!(place(&mut e, "odd"), None, "an unknown place adds nothing");
}

#[test]
fn an_endless_loop_is_stopped_and_the_deck_keeps_its_scripts_working() {
    let started = Instant::now();
    let (dir, mut e, _rx) = loaded("lua-loop", "while true do end");
    assert!(started.elapsed() < Duration::from_secs(10), "loading took {:?}", started.elapsed());
    let errors = e.take_errors();
    assert!(errors.len() == 1 && errors[0].contains("ran too long"), "{errors:?}");
    assert_eq!(e.loaded, ["unmerged-branches"], "only the bundled plugin");
    // A keymap's function that never returns is stopped at the call limit.
    e.eval("deck.keymap('f8', function() while true do end end)");
    let KeyAction::Lua(callback) = e.keymaps()[0].action else { panic!() };
    let pressed = Instant::now();
    e.press(callback);
    assert!(pressed.elapsed() < Duration::from_secs(2), "{:?}", pressed.elapsed());
    assert!(e.take_errors()[0].contains("ran too long"));
    assert_eq!(e.eval("=1 + 1"), Some("2".into()));
    drop(dir);
}

/// OPS-S12801: a table's `__index` is Lua as much as a function is, whether
/// the deck reads it from a keymap's, watch's or section's arguments or from
/// what a section or badge function returned. Each is stopped at the same
/// limit, and an argument is read before the deck's own state is borrowed.
#[test]
fn a_tables_index_function_is_stopped_at_the_limit_too() {
    let (_dir, mut e, _rx) = loaded(
        "lua-index-limit",
        r#"
        function slow() return setmetatable({}, { __index = function() for _ = 1, 1e9 do end end }) end
        function slow_lines()
            return setmetatable({ title = "T", phone = true, sidebar = true, pinned = false, header = false },
                { __index = function() for _ = 1, 1e9 do end end })
        end
        function reentrant() return setmetatable({}, { __index = function() deck.notify("read") return 0 end }) end
        "#,
    );
    let stopped = |e: &mut Engine, what: &str| {
        let errors = e.take_errors();
        assert!(errors.iter().any(|m| m.contains("ran too long")), "{what}: {errors:?}");
    };
    assert_eq!(e.take_errors(), Vec::<String>::new());
    e.eval("deck.keymap('f9', 'help', slow())");
    stopped(&mut e, "a keymap's options");
    assert!(e.keymaps().iter().all(|k| k.key != "f9"));
    e.eval("deck.fs.watch({}, function() end, slow())");
    stopped(&mut e, "a watch's options");
    e.eval("deck.fs.watch({}, function() end, reentrant())");
    assert_eq!(e.take_errors(), Vec::<String>::new(), "a watch's options can call the deck");
    assert_eq!(e.take_requests(), vec![notice("read")]);
    e.eval("deck.ui.section('given', slow_lines())");
    stopped(&mut e, "a section's lines");
    e.eval("deck.ui.section('returned', { lines = function() return { slow() } end })");
    assert_eq!(e.take_errors(), Vec::<String>::new());
    assert!(e.sections().iter().all(|s| s.id != "given"));
    stopped(&mut e, "a returned line");
    let sessions = [serde_json::json!({"id": 1})];
    let before = e.badges(&sessions);
    e.eval("deck.ui.badge(function() return slow() end)");
    assert_eq!(e.badges(&sessions), before);
    stopped(&mut e, "a returned badge");
    e.eval("setmetatable(deck.opt, { __index = function() for _ = 1, 1e9 do end return false end })");
    assert_eq!(e.option("badges"), None);
}

#[test]
fn the_call_limit_charges_only_the_scripts_own_processor_time() {
    // A callback that waits on a program longer than the call limit uses
    // almost no processor time; under a busy machine the wall clock alone
    // must never stop it.
    let (dir, mut e, _rx) = loaded(
        "lua-cpu-limit",
        r#"deck.keymap('f8', function()
            deck.run({"sleep", "0.6"})
            local n = 0
            for i = 1, 200000 do n = n + i end
            deck.notify("finished " .. n)
        end)"#,
    );
    assert!(e.take_errors().is_empty());
    let KeyAction::Lua(callback) = e.keymaps()[0].action else { panic!() };
    let pressed = Instant::now();
    e.press(callback);
    assert!(pressed.elapsed() >= Duration::from_millis(500), "{:?}", pressed.elapsed());
    assert!(e.take_errors().is_empty(), "waiting is not running");
    assert!(e
        .take_requests()
        .iter()
        .any(|r| matches!(r, Request::Notify { text, .. } if text.starts_with("finished"))));
    drop(dir);
}

#[test]
fn failing_state_callback_identifies_its_plugin_without_an_extra_notice() {
    let dir = TempDir::new("lua-callback-origin");
    dir.write("config/plugins/broken.lua", "deck.on('state', function() while true do end end)");
    dir.write("config/plugins/healthy.lua", "deck.on('state', function() deck.notify('healthy') end)");
    let (mut e, _) = engine(&dir);
    e.load();
    assert!(e.take_errors().is_empty());
    e.emit_json("state", &serde_json::Value::Null);
    let errors = e.take_errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("broken.lua:1"), "{errors:?}");
    assert_eq!(e.take_requests(), vec![notice("healthy")]);
}

#[test]
fn a_picked_field_is_exactly_that_field_of_the_whole_state() {
    use crate::protocol::{BranchInfo, DeckState, HostInfo, Tool};
    let state = DeckState {
        sessions: vec![crate::tui::tests::session(4, Tool::Codex, Status::Working, true)],
        native_host: Some(HostInfo { name: "omarchy".into(), ..HostInfo::default() }),
        branches: vec![BranchInfo { branch: "topic".into(), ..BranchInfo::default() }],
        start_dir: "/repo".into(),
        ..DeckState::default()
    };
    let serde_json::Value::Object(whole) = serde_json::to_value(&state).unwrap() else { panic!("an object") };
    for (name, value) in &whole {
        assert_eq!(pick(&state, name).as_ref(), Some(value), "{name}");
    }
    // Parts the whole state leaves out when empty are left out when picked.
    for name in ["panels", "scans", "processes", "providers", "not_a_field"] {
        assert!(!whole.contains_key(name), "{name}");
        assert_eq!(pick(&state, name), None, "{name}");
    }
}

/// A state source that records each field made into JSON.
struct Counted {
    fields: serde_json::Map<String, serde_json::Value>,
    made: Rc<RefCell<Vec<String>>>,
}

impl StateSource for Counted {
    fn field(&self, name: &str) -> Option<serde_json::Value> {
        self.made.borrow_mut().push(name.to_string());
        self.fields.get(name).cloned()
    }
    fn whole(&self) -> serde_json::Map<String, serde_json::Value> {
        self.made.borrow_mut().push("(whole)".into());
        self.fields.clone()
    }
}

#[test]
fn a_state_handler_makes_only_the_fields_it_asks_for_and_only_after_they_change() {
    // The bundled unmerged-branches count reads its own fields on each change.
    let (dir, mut e, _rx) = loaded(
        "lua-lazy-state",
        r#"deck.opt["unmerged-branches"] = false
        deck.on('state', function()
            local s = deck.state({'sessions'})
            deck.notify('sessions ' .. #s.sessions)
        end)"#,
    );
    let made = Rc::new(RefCell::new(Vec::new()));
    let source = |sessions: usize| -> Rc<dyn StateSource> {
        let fields =
            serde_json::json!({"sessions": vec![serde_json::json!({"id": 1}); sessions], "branches": ["many"]});
        Rc::new(Counted { fields: fields.as_object().unwrap().clone(), made: made.clone() })
    };
    e.set_state_source(source(1), None);
    e.emit_json("state", &serde_json::Value::Null);
    // Only the branch list changed: what the handler read is kept.
    e.set_state_source(source(1), Some(&["branches"][..]));
    e.emit_json("state", &serde_json::Value::Null);
    assert_eq!(*made.borrow(), ["sessions"], "a field not asked for is never made; an unchanged one once");
    e.set_state_source(source(2), Some(&["sessions"][..]));
    e.emit_json("state", &serde_json::Value::Null);
    assert_eq!(*made.borrow(), ["sessions", "sessions"]);
    assert!(e.take_errors().is_empty());
    assert_eq!(e.take_requests(), vec![notice("sessions 1"), notice("sessions 1"), notice("sessions 2")]);
    drop(dir);
}

#[test]
fn a_failing_plugin_leaves_the_others_loaded() {
    let dir = TempDir::new("lua-plugins");
    dir.write("config/init.lua", "deck.opt.mine = true");
    dir.write("config/plugins/a.lua", "error('boom')");
    dir.write("config/plugins/b.lua", "deck.command('fine', function() end)");
    dir.write("config/plugins/notes.txt", "deck.command('never', function() end)");
    let (mut e, _rx) = engine(&dir);
    e.load();
    assert_eq!(e.loaded, ["init.lua", "b.lua", "unmerged-branches", "tasks"]);
    let errors = e.take_errors();
    assert!(errors.len() == 1 && errors[0].starts_with("Lua a.lua: ") && errors[0].contains("boom"), "{errors:?}");
    assert_eq!(e.commands().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["fine", "tasks"]);
}

#[test]
fn sections_draw_theme_styled_lines_and_function_lines_each_time() {
    let (_dir, mut e, _rx) = loaded(
        "lua-sections",
        r##"
        deck.ui.section("load", { title = "LOAD", lines = { "plain", { { " up", "ok" }, { " 3", "#ff0000" } } } })
        local n = 0
        deck.ui.section("count", { lines = function() n = n + 1 return { "call " .. n } end, phone = false, pinned = true })
        deck.ui.section("long", { lines = { string.rep("x", 1000), "a\nb" } })
        "##,
    );
    assert_eq!(e.take_errors(), Vec::<String>::new());
    let first = e.sections();
    assert_eq!(first[0].title, "LOAD");
    // A style that is not a theme name draws plain.
    assert_eq!(first[0].lines, vec![vec![plain("plain")], vec![seg(" up", "ok"), plain(" 3")]]);
    assert!(first[0].phone && !first[0].pinned);
    assert_eq!(
        first[0].panel(),
        Panel { id: "load".into(), title: "LOAD".into(), lines: vec!["plain".into(), " up 3".into()] }
    );
    assert_eq!((first[1].title.as_str(), first[1].phone, first[1].pinned), ("COUNT", false, true));
    assert_eq!(first[1].lines, vec![vec![plain("call 1")]]);
    assert_eq!(e.sections()[1].lines, vec![vec![plain("call 2")]]);
    // A line is one line, and no wider than the limit.
    let long = &first[2].lines;
    assert!(long[0][0].text.chars().count() <= MAX_LINE, "{}", long[0][0].text.len());
    assert!(!long[1][0].text.contains('\n'));

    e.eval(r#"deck.ui.remove("load")"#);
    assert_eq!(e.sections().iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["count", "long"]);
}

#[test]
fn sections_and_their_lines_are_bounded() {
    let (_dir, mut e, _rx) = loaded(
        "lua-bounds",
        r#"
        local lines = {}
        for i = 1, 100 do lines[i] = "line " .. i end
        deck.ui.section("many", { lines = lines })
        for i = 2, 16 do deck.ui.section("s" .. i, { lines = {} }) end
        "#,
    );
    assert_eq!(e.take_errors(), Vec::<String>::new());
    assert_eq!(e.sections()[0].lines.len(), MAX_PANEL_LINES);
    e.eval(r#"deck.ui.section("one-too-many", { lines = {} })"#);
    assert!(e.take_errors()[0].contains("at most 16 sections"));
    // Replacing one that exists is not another.
    e.eval(r#"deck.ui.section("s2", { title = "TWO", lines = {} })"#);
    assert_eq!(e.take_errors(), Vec::<String>::new());
    assert_eq!(e.sections().len(), 16);
}

#[test]
fn badges_come_from_every_badge_function_for_each_session() {
    let (_dir, mut e, _rx) = loaded(
        "lua-badges",
        r#"
        deck.ui.badge(function(s) if s.status == "finished" then return { " done", "finished" } end end)
        deck.ui.badge(function(s) if s.id == 1 then return " one" end end)
        "#,
    );
    let sessions =
        [serde_json::json!({"id": 1, "status": "finished"}), serde_json::json!({"id": 2, "status": "working"})];
    let badges = e.badges(&sessions);
    assert_eq!(badges.len(), 1);
    assert_eq!(badges[&1], vec![seg(" done", "finished"), plain(" one")]);
}

#[test]
fn programs_run_from_a_list_of_words_never_a_shell_line() {
    let (_dir, mut e, _rx) = loaded("lua-run", "");
    assert_eq!(
        e.eval(r#"=deck.run({"printf", "%s", "a b; echo injected"}).stdout"#),
        Some("a b; echo injected".into())
    );
    assert_eq!(e.eval(r#"=deck.run({"sh", "-c", "exit 3"}).code"#), Some("3".into()));
    assert_eq!(e.take_errors(), Vec::<String>::new());
    for code in [r#"deck.run("echo hi")"#, r#"deck.run({})"#, r#"deck.job("echo hi", {}, function() end)"#] {
        assert_eq!(e.eval(code), None);
        let errors = e.take_errors();
        assert!(errors.len() == 1 && errors[0].contains("list of words"), "{code}: {errors:?}");
    }
    // A program that runs past its time limit is stopped.
    let started = Instant::now();
    assert_eq!(e.eval(r#"=deck.run({"sleep", "5"}, { timeout = 0.3 }).timed_out"#), Some("true".into()));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn the_standard_library_cannot_take_the_terminal_or_run_a_shell_line() {
    let (_dir, mut e, _rx) = loaded("lua-stdlib", "");
    for code in ["os.execute('true')", "io.popen('true')", "io.read()", "io.write('x')", "os.exit(1)", "io.lines()"] {
        assert_eq!(e.eval(code), None);
        let errors = e.take_errors();
        let refused = |e: &String| e.contains("not available") || e.contains("needs a file");
        assert!(errors.len() == 1 && refused(&errors[0]), "{code}: {errors:?}");
    }
    assert_eq!(e.eval("=io.stdout"), Some("nil".into()));
    // `print` shows on the notice line.
    e.eval("print('hello', 1, nil)");
    assert_eq!(e.take_requests(), vec![notice("hello 1 nil")]);
    // Files are still the script's to read.
    let dir = TempDir::new("lua-stdlib-file");
    let file = dir.write("notes.txt", "one\ntwo\n");
    assert_eq!(
        e.eval(&format!("=select(2, io.open({:?}):read('a'):gsub('\\n', ''))", file.display().to_string())),
        Some("2".into())
    );
    assert_eq!(e.eval("=package.cpath"), Some("".into()));
}

#[test]
fn a_job_runs_in_the_background_and_calls_back_with_what_it_printed() {
    let (_dir, mut e, rx) = loaded("lua-job", "");
    e.eval(r#"deck.job({"printf", "%s", "done"}, {}, function(r) result = r.stdout .. ":" .. tostring(r.ok) end)"#);
    assert_eq!(e.take_errors(), Vec::<String>::new());
    let done = rx.recv_timeout(Duration::from_secs(20)).expect("the job finishes");
    assert_eq!((done.code, done.stdout.as_str()), (Some(0), "done"));
    e.job_done(done.clone());
    assert_eq!(e.eval("=result"), Some("done:true".into()));
    // Its function runs once.
    e.eval("result = nil");
    e.job_done(done);
    assert_eq!(e.eval("=result"), Some("nil".into()));
}

#[test]
fn at_most_eight_jobs_run_at_once() {
    let (_dir, mut e, rx) = loaded("lua-jobs", "");
    e.eval(r#"for i = 1, 8 do deck.job({"true"}, {}, function() end) end"#);
    assert_eq!(e.take_errors(), Vec::<String>::new());
    e.eval(r#"deck.job({"true"}, {}, function() end)"#);
    assert!(e.take_errors()[0].contains("8 jobs are running already"));
    // As they finish there is room again.
    let done = rx.recv_timeout(Duration::from_secs(20)).unwrap();
    e.job_done(done);
    e.eval(r#"deck.job({"true"}, {}, function() end)"#);
    assert_eq!(e.take_errors(), Vec::<String>::new());
}

#[test]
fn timers_run_when_due_once_or_every_time() {
    let (_dir, mut e, _rx) = loaded("lua-timers", "");
    assert_eq!(e.next_due(), None);
    e.eval("deck.after(0, function() fired = (fired or 0) + 1 end)");
    assert!(e.next_due().is_some());
    assert!(e.tick(Instant::now() + Duration::from_millis(10)));
    assert!(!e.tick(Instant::now() + Duration::from_secs(5)));
    assert_eq!(e.eval("=fired"), Some("1".into()));

    // A repeat is never sooner than a second.
    e.eval("every = deck.every(0.01, function() ticks = (ticks or 0) + 1 end)");
    let now = Instant::now();
    assert!(e.next_due().unwrap() >= now + Duration::from_millis(900));
    assert!(!e.tick(now + Duration::from_millis(100)));
    assert!(e.tick(now + Duration::from_secs(2)));
    assert!(e.tick(now + Duration::from_secs(4)));
    assert_eq!(e.eval("=ticks"), Some("2".into()));
    e.eval("deck.cancel(every)");
    assert_eq!(e.next_due(), None);
}

#[test]
fn colon_lua_runs_code_and_shows_a_value_after_an_equals_sign() {
    let (_dir, mut e, _rx) = loaded("lua-eval", "");
    assert_eq!(e.eval("=1 + 1"), Some("2".into()));
    assert_eq!(e.eval("x = 5"), None);
    assert_eq!(e.eval("=x"), Some("5".into()));
    assert_eq!(e.eval("={a = 1}"), Some(r#"{"a":1}"#.into()));
    assert_eq!(e.take_errors(), Vec::<String>::new());
    assert_eq!(e.eval("=)"), None);
    assert!(e.take_errors()[0].starts_with("Lua :lua: "));
}

#[test]
fn options_from_init_lua_and_from_colon_set() {
    let (_dir, mut e, _rx) = loaded("lua-opt", r#"deck.opt.sidebar = { "title", "sessions" }"#);
    assert_eq!(e.option("sidebar"), Some(serde_json::json!(["title", "sessions"])));
    e.set_option("depth", "3").unwrap();
    e.set_option("label", "fedora box").unwrap();
    assert_eq!(e.option("depth"), Some(serde_json::json!(3)));
    assert_eq!(e.option("label"), Some(serde_json::json!("fedora box")));
    assert_eq!(e.option("missing"), None);
    assert_eq!(e.options().keys().map(String::as_str).collect::<Vec<_>>(), ["depth", "label", "sidebar"]);
    assert_eq!(e.eval("=deck.opt.depth + 1"), Some("4".into()));
}

#[test]
fn events_reach_their_handlers_and_ready_follows_loading() {
    let (_dir, mut e, _rx) = loaded(
        "lua-events",
        r#"
        deck.on("ready", function() deck.notify("ready") end)
        deck.on("finished", function(s) deck.notify(s.title .. " finished") end)
        "#,
    );
    assert_eq!(e.take_requests(), vec![notice("ready")]);
    e.emit_json("finished", &serde_json::json!({"title": "tests"}));
    e.emit_json("needs_you", &serde_json::json!({"title": "nobody listens"}));
    assert_eq!(e.take_requests(), vec![notice("tests finished")]);
    assert_eq!(e.take_errors(), Vec::<String>::new());
}

#[test]
fn state_is_a_read_only_copy() {
    let (_dir, mut e, _rx) = loaded("lua-state", "");
    e.set_state(serde_json::json!({"sessions": [{"id": 4, "title": "t"}], "branches": ["large unused snapshot"]}));
    assert_eq!(e.eval("=deck.state().sessions[1].id"), Some("4".into()));
    assert_eq!(e.eval("=deck.state({'sessions'}).branches == nil"), Some("true".into()));
    e.eval("deck.state({'sessions'}).sessions[1].id = 99");
    assert_eq!(e.eval("=deck.state({'sessions'}).sessions[1].id"), Some("4".into()));
    e.eval("deck.state().sessions[1].id = 9");
    assert_eq!(e.eval("=deck.state().sessions[1].id"), Some("4".into()));
}

#[test]
fn require_finds_the_config_lua_folder_and_the_deck_itself() {
    let dir = TempDir::new("lua-require");
    dir.write("config/lua/helper.lua", "return { greet = function() return 'hi' end }");
    dir.write("config/init.lua", "local h = require('helper') assert(require('deck') == deck) deck.notify(h.greet())");
    let (mut e, _rx) = engine(&dir);
    e.load();
    assert_eq!(e.take_errors(), Vec::<String>::new());
    assert_eq!(e.take_requests(), vec![notice("hi")]);
}

#[test]
fn a_script_cannot_take_more_than_its_memory() {
    let (_dir, mut e, _rx) = loaded("lua-memory", "");
    assert_eq!(e.eval("big = string.rep('x', 100 * 1024 * 1024)"), None);
    assert!(e.take_errors()[0].contains("memory"));
    assert_eq!(e.eval("=#string.rep('x', 1024)"), Some("1024".into()));
}

#[test]
fn requests_for_sessions_terminals_and_pickers() {
    let (_dir, mut e, _rx) = loaded(
        "lua-requests",
        r#"
        deck.term({ host = "fedora", cwd = "/w", name = "logs", cmd = { "tail", "-f", "x.log" } })
        deck.send(3, "go on\r")
        deck.select(3)
        deck.cmd("dev on")
        deck.ui.pick({ title = "Pick one", items = { "a", { { "b", "ok" } } }, on_choose = function(item, i) chosen = item .. i end })
        "#,
    );
    assert_eq!(e.take_errors(), Vec::<String>::new());
    let requests = e.take_requests();
    assert_eq!(
        requests[..4],
        [
            Request::Create {
                tool: "shell".into(),
                host: Some("fedora".into()),
                cwd: Some("/w".into()),
                name: Some("logs".into()),
                command: vec!["tail".into(), "-f".into(), "x.log".into()],
            },
            Request::Send { id: 3, text: "go on\r".into() },
            Request::Select { id: 3 },
            Request::Command("dev on".into()),
        ]
    );
    let Request::Pick { pick, callback } = &requests[4] else { panic!("{:?}", requests[4]) };
    let items: Vec<_> = pick.items.iter().map(|item| item.text.clone()).collect();
    assert_eq!((pick.title.as_str(), items), ("Pick one", vec![vec![plain("a")], vec![seg("b", "ok")]]));
    e.choose(*callback, Some((1, "b")), None);
    assert_eq!(e.eval("=chosen"), Some("b2".into()));
    // A terminal's command is a list of words too.
    e.eval(r#"deck.term({ cmd = "tail -f x.log" })"#);
    assert!(e.take_errors()[0].contains("list of words"));
    e.eval(r#"deck.resume("codex", "saved-chat", {host="fedora"})"#);
    assert_eq!(
        e.take_requests(),
        vec![Request::Resume { tool: "codex".into(), conversation: "saved-chat".into(), host: Some("fedora".into()) }]
    );
    e.eval(r#"deck.resume("shell", "saved-chat")"#);
    assert!(e.take_errors()[0].contains("agent program"));
    e.eval(r#"deck.resume("codex", "--exec")"#);
    assert!(e.take_errors()[0].contains("conversation id"));
    assert!(e.take_requests().is_empty());
}

#[test]
fn a_json_null_reaches_scripts_as_nil() {
    // A terminal has no worktree, so the deck sends `"worktree": null`. That
    // reached scripts as a userdata, causing a badge to fail on every draw
    // with "attempt to index a userdata value (field 'worktree')".
    let (_dir, mut e, _rx) = loaded(
        "lua-null",
        r#"
        deck.ui.badge(function(s) return s.worktree and s.worktree.task or nil end)
        deck.on("finished", function(s) seen = tostring(s.worktree) end)
        "#,
    );
    e.set_state(serde_json::json!({"current": null, "sessions": []}));
    assert_eq!(e.eval("=tostring(deck.state().current)"), Some("nil".into()));
    let sessions =
        [serde_json::json!({"id": 3, "worktree": null}), serde_json::json!({"id": 4, "worktree": {"task": "alpha"}})];
    let badges = e.badges(&sessions);
    assert_eq!(e.take_errors(), Vec::<String>::new());
    assert_eq!(badges.len(), 1, "{badges:?}");
    assert_eq!(badges[&4], vec![plain("alpha")]);
    e.emit_json("finished", &serde_json::json!({"worktree": null}));
    assert_eq!(e.eval("=seen"), Some("nil".into()));
    assert_eq!(e.eval(r#"=tostring(deck.json.decode('{"a": null}').a)"#), Some("nil".into()));
    assert_eq!(e.take_errors(), Vec::<String>::new());
}

#[test]
fn the_example_init_lua_loads_cleanly() {
    let dir = TempDir::new("lua-example");
    dir.write("config/init.lua", EXAMPLE_INIT);
    let (mut e, _rx) = engine(&dir);
    e.load();
    assert_eq!(e.take_errors(), Vec::<String>::new());
    assert_eq!(e.loaded, ["init.lua", "unmerged-branches", "tasks"]);
    assert!(e.commands().iter().any(|c| c.name == "fleet"));
    assert!(e.commands().iter().any(|c| c.name == "tasks"));
    assert!(e.sections().iter().any(|s| s.id == "load"));

    dir.write("start/TASKS.md", "- [ ] Review parser diagnostics\n- [x] Add project overview\n");
    let source = "task-sources:demo:markdown:tasks";
    let tasks = crate::task_source::markdown_tasks(&dir.path().join("start"), source, "TASKS.md").unwrap();
    let data = serde_json::json!({
        "sources":[{"id":source,"owner":"task-sources","repository":"demo","fresh":true,"updated_at":1}],
        "tasks":tasks,
    });
    e.set_state(serde_json::json!({"work_queue_capable":true,"work_queue":data,"sessions":[]}));
    e.emit_json("state", &serde_json::Value::Null);
    assert!(e.run_command("tasks", &["queue".into(), "all".into()]));
    let requests = e.take_requests();
    let [Request::Fleet { action, callback, .. }] = requests.as_slice() else { panic!("{requests:?}") };
    assert_eq!(action, "source-list");
    e.service_result(*callback, &data, None);
    let requests = e.take_requests();
    let [Request::Pick { pick, callback }] = requests.as_slice() else { panic!("{requests:?}") };
    assert_eq!(pick.title, "Tasks");
    assert_eq!(pick.tabs[pick.tab].label, "All");
    assert_eq!(pick.tabs.iter().find(|t| t.label == "Ready").unwrap().count, "1");
    assert_eq!(pick.tabs.iter().find(|t| t.label == "Stopped").unwrap().count, "1");
    assert_eq!(pick.tabs.iter().find(|t| t.label == "Merged").unwrap().count, "0");
    let selected = pick.items.iter().position(|item| item.plain().contains("Review parser diagnostics")).unwrap();
    assert!(pick.items.iter().any(|item| item.plain().contains("Add project overview")));
    e.choose(*callback, Some((selected, &pick.items[selected].plain())), None);
    let requests = e.take_requests();
    let [Request::Command(line)] = requests.as_slice() else { panic!("{requests:?}") };
    assert_eq!(
        crate::actions::words(line).unwrap(),
        ["queue", "source", source, &tasks[0].id],
        "the picker hands opaque source/task identities to the native command"
    );
    let (mut app, rx) = crate::tui::tests::app_with(Vec::new(), Vec::new());
    app.state.work_queue_capable = true;
    let _ = rx.try_iter().collect::<Vec<_>>();
    app.run_command(line);
    assert_eq!(app.mode, crate::tui::Mode::Command);
    assert_eq!(
        crate::actions::words(&app.command).unwrap(),
        ["queue", "source", source, &tasks[0].id, "--title", "New task job", "--"],
        "the App opens the explicit-spec draft without executing task text"
    );
    assert!(rx.try_recv().is_err(), "opening the spec must not submit or execute work");
    assert!(e.take_errors().is_empty());
}

/// What the dynamic loader of a host asks its C library for when it starts
/// this program: the `GLIBC_x.y` versions its file names, and the symbols
/// it takes from shared libraries. This test program is built from the same
/// code and C libraries as the deck itself.
struct LoaderNeeds {
    glibc: Vec<(u32, u32)>,
    imports: Vec<String>,
}

fn loader_needs() -> LoaderNeeds {
    let bytes = std::fs::read("/proc/self/exe").expect("reading this program");
    assert_eq!(&bytes[..6], b"\x7fELF\x02\x01", "a little-endian 64-bit ELF file");
    let u16_at = |o: usize| u16::from_le_bytes(bytes[o..o + 2].try_into().unwrap()) as usize;
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap()) as usize;
    let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap()) as usize;
    let text = |o: usize| {
        let end = bytes[o..].iter().position(|&b| b == 0).unwrap();
        String::from_utf8_lossy(&bytes[o..o + end]).into_owned()
    };
    const SHT_DYNSYM: usize = 11;
    const SHT_GNU_VERNEED: usize = 0x6fff_fffe;
    // Each section header's type, file offset, size, linked string table and info.
    let (shoff, shentsize, shnum) = (u64_at(0x28), u16_at(0x3a), u16_at(0x3c));
    let sections: Vec<[usize; 5]> = (0..shnum)
        .map(|i| shoff + i * shentsize)
        .map(|h| [u32_at(h + 4), u64_at(h + 0x18), u64_at(h + 0x20), u32_at(h + 0x28), u32_at(h + 0x2c)])
        .collect();
    let mut needs = LoaderNeeds { glibc: Vec::new(), imports: Vec::new() };
    for &[kind, offset, size, link, info] in &sections {
        let strings = sections[link][1];
        if kind == SHT_GNU_VERNEED {
            let mut need = offset;
            for _ in 0..info {
                let mut aux = need + u32_at(need + 8);
                for _ in 0..u16_at(need + 2) {
                    let name = text(strings + u32_at(aux + 8));
                    let mut parts = name.strip_prefix("GLIBC_").unwrap_or_default().split('.');
                    if let (Some(Ok(major)), Some(Ok(minor))) =
                        (parts.next().map(str::parse), parts.next().map(str::parse))
                    {
                        needs.glibc.push((major, minor));
                    }
                    aux += u32_at(aux + 12);
                }
                need += u32_at(need + 12);
            }
        } else if kind == SHT_DYNSYM {
            // Entry 0 is the null symbol; an import has no section of its own.
            for sym in (offset..offset + size).step_by(24).skip(1) {
                if u16_at(sym + 6) == 0 {
                    needs.imports.push(text(strings + u32_at(sym)));
                }
            }
        }
    }
    assert!(!needs.glibc.is_empty() && !needs.imports.is_empty(), "found the loader's tables");
    needs
}

#[test]
fn lua_hyperbolic_math_takes_nothing_from_glibc() {
    // glibc 2.44 gave `cosh` and `sinh` new symbol versions, so taking them
    // from glibc at all ties the deck to the C library of the machine that
    // built it. Lua's deprecated `math.cosh` and `math.sinh` are their only
    // users.
    let imports = loader_needs().imports;
    for name in ["cosh", "sinh"] {
        assert!(!imports.iter().any(|i| i == name), "the deck takes {name} from glibc");
    }
}

#[test]
fn lua_hyperbolic_math_still_answers_correctly() {
    let lua = Lua::new();
    let close = |got: f64, want: f64| (got - want).abs() <= want.abs() * 4.0 * f64::EPSILON;
    for (x, cosh, sinh) in [
        (1.0, 1.5430806348152437, 1.1752011936438014),
        (-0.5, 1.1276259652063807, -0.5210953054937474),
        (3.0, 10.067661995777765, 10.017874927409903),
        (-20.0, 242582597.70489514, -242582597.70489514),
        // Past `exp`'s own overflow, still finite.
        (710.0, 1.1169973830808555e308, 1.1169973830808555e308),
    ] {
        let (c, s): (f64, f64) = lua.load(format!("return math.cosh({x:?}), math.sinh({x:?})")).eval().unwrap();
        assert!(close(c, cosh), "math.cosh({x}) = {c}, not {cosh}");
        assert!(close(s, sinh), "math.sinh({x}) = {s}, not {sinh}");
    }
    let (tiny, neg_zero, cosh_zero): (f64, f64, f64) =
        lua.load("return math.sinh(1e-10), math.sinh(-0.0), math.cosh(0)").eval().unwrap();
    assert_eq!(tiny, 1e-10);
    assert!(neg_zero == 0.0 && neg_zero.is_sign_negative());
    assert_eq!(cosh_zero, 1.0);
    let (big, neg_big, nan): (f64, f64, f64) =
        lua.load("return math.cosh(1000), math.sinh(-1000), math.cosh(0/0)").eval().unwrap();
    assert_eq!((big, neg_big), (f64::INFINITY, f64::NEG_INFINITY));
    assert!(nan.is_nan());
}

#[test]
fn service_commands_are_bounded_and_do_not_load_view_timers() {
    let dir = TempDir::new("lua-service-lifecycle");
    dir.write("config/init.lua", "error('a service must not load view scripts')");
    dir.write(
        "config/service/sample.lua",
        r#"
        deck.service.command("sample", "echo", function(args) return {message=args.message} end,
            {desc="Echo literal arguments",args={"message"}})
        deck.on("ready",function() deck.service.annotate("sample",42,{badge="ready"}) end)
    "#,
    );
    let (tx, _rx) = mpsc::channel();
    let mut engine = Engine::new_service(
        Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("start") },
        Arc::new(move |done| {
            let _ = tx.send(done);
        }),
    )
    .unwrap();
    engine.load_service();
    let errors = engine.take_errors();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(engine.service_commands().len(), 1);
    let literal = serde_json::json!({"message":"$HOME `echo no` \"quoted\"\nline"});
    assert_eq!(engine.service_command("sample", "echo", &literal).unwrap(), literal);
    assert!(engine.service_command("sample", "missing", &serde_json::json!({})).is_err());
    assert!(engine.service_command("sample", "echo", &serde_json::json!({"message":"x".repeat(17000)})).is_err());
    assert!(matches!(engine.take_requests().as_slice(), [Request::Annotate { id: 42, .. }]));
}

#[test]
fn a_view_cannot_register_service_capabilities() {
    let (dir, mut view, _rx) =
        loaded("lua-view-context", "deck.service.command('sample','echo',function(args) return args end)");
    assert!(!view.take_errors().is_empty());
    assert!(view.service_commands().is_empty());
    let (mut fresh, _rx) = engine(&dir);
    fresh.eval("deck.service.process('sample','child',{cmd={'sleep','1'}})");
    assert!(!fresh.take_errors().is_empty());
    assert!(fresh.take_requests().is_empty());
}

#[test]
fn auto_ordering_handles_empty_cli_state_type_groups_and_persistent_toggle() {
    use crate::protocol::{DeckState, Tool};
    let dir = TempDir::new("auto-ordering-command");
    dir.write("config/plugins/auto-ordering.lua", include_str!("../../plugins/auto-ordering.lua"));
    let (mut empty, _rx) = engine(&dir);
    empty.load();
    assert!(empty.take_errors().is_empty(), "lua check has no session snapshot yet");
    for first in [Tool::Codex, Tool::Claude] {
        let second = if first == Tool::Codex { Tool::Claude } else { Tool::Codex };
        let make = |id, tool, status| crate::tui::tests::session(id, tool, status, true);
        let mut clear = make(1, first, Status::Idle);
        clear.conversation = None;
        let (mut app, rx) = crate::tui::tests::app_with(Vec::new(), Vec::new());
        app.state = DeckState {
            start_dir: dir.path().join("start").to_string_lossy().into_owned(),
            sessions: vec![
                clear,
                make(2, second, Status::Working),
                make(3, first, Status::Finished),
                make(4, first, Status::Working),
                make(5, second, Status::Idle),
                crate::tui::tests::session(90, first, Status::Finished, false),
            ],
            ..DeckState::default()
        };
        app.current = Some(crate::tui::Sel::Session(1));
        app.use_order_file(dir.path().join("order.json"));
        app.load_lua(
            Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("start") },
            Arc::new(|_| {}),
        );
        let ids = |app: &crate::tui::App| app.ordered_sessions().iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(ids(&app), [4, 3, 1, 2, 5]);
        assert_eq!(app.lua_state()["current"], 1, "movement preserves selection");
        assert!(app.run_user_command("auto-ordering", "off"));
        let mut next = app.state.clone();
        next.sessions.iter_mut().find(|s| s.id == 3).unwrap().status = Status::Working;
        next.sessions.iter_mut().find(|s| s.id == 4).unwrap().status = Status::Idle;
        app.state = next;
        app.lua_state_changed(false);
        assert_eq!(ids(&app), [4, 3, 1, 2, 5], "off stops automatic moves");
        app.reload_lua();
        assert_eq!(ids(&app), [4, 3, 1, 2, 5], "off survives reload");
        assert!(app.run_user_command("auto-ordering", "on"));
        assert_eq!(ids(&app), [3, 4, 1, 2, 5]);
        assert_eq!(app.lua_state()["current"], 1);
        assert!(app.run_user_command("auto-ordering", "off"));
        app.lua_eval("deck.reorder({2,5,3,4,1})");
        assert!(app.run_user_command("auto-ordering", "on"));
        assert_eq!(ids(&app), [2, 5, 3, 4, 1], "a manual type-group move takes effect immediately");
        // Current coordinators persist the shared order through protocol messages.
        assert!(!dir.path().join("order.json").exists());
        let orders: Vec<_> = rx
            .try_iter()
            .filter_map(|msg| match msg {
                crate::protocol::ClientMsg::SetOrder { ids } => Some(ids),
                _ => None,
            })
            .collect();
        assert_eq!(orders.last().unwrap(), &[2, 5, 3, 4, 1]);
        assert!(orders.iter().all(|ids| !ids.contains(&90)), "ended sessions never enter SetOrder");
        assert!(app.state.sessions.iter().any(|s| s.id == 90), "ended history remains available");
    }
}

/// `:auto-ordering` puts "/clear agents ... at the bottom" (owner, 2026-11-05),
/// so the scripts hear of a clear that leaves the status as it was.
#[test]
fn changes_tell_scripts_when_a_conversation_is_cleared_for_ordering() {
    use crate::protocol::Tool;
    let with = |conversation: Option<&str>, last_active| {
        let mut s = crate::tui::tests::session(1, Tool::Codex, Status::Idle, true);
        s.conversation = conversation.map(str::to_string);
        s.last_active = last_active;
        s
    };
    let events = |before: &SessionInfo, after: &SessionInfo| -> Vec<&'static str> {
        changes(&seen(std::slice::from_ref(before)), std::slice::from_ref(after))
            .into_iter()
            .map(|change| change.event)
            .collect()
    };
    let used = with(Some("old"), 1_700_000_000);
    assert_eq!(events(&used, &with(None, 0)), ["cleared"], "Codex names no conversation until its first turn ends");
    assert_eq!(events(&used, &with(Some("new"), 0)), ["cleared"], "Claude names the empty conversation at once");
    assert!(events(&used, &with(Some("other"), 1_700_000_050)).is_empty(), "resuming a used conversation");
    assert!(events(&with(None, 0), &with(Some("first"), 0)).is_empty(), "a new session's first conversation");
    assert!(events(&used, &with(Some("old"), 0)).is_empty(), "the same conversation");
    let mut ended = with(None, 0);
    (ended.alive, ended.status) = (false, Status::Ended);
    assert_eq!(events(&used, &ended), ["session_end", "status"], "an ended session is not cleared");
}

#[test]
fn repository_plugins_use_catalog_sources_and_modules_without_home_fallback() {
    let dir = TempDir::new("repo-plugin-loading");
    dir.write("config/plugins/example.lua", "error('stale installed UI plugin')");
    dir.write("config/service/example.lua", "error('stale installed service plugin')");
    dir.write("config/lua/helper.lua", "error('stale installed helper')");
    dir.write(
        "sources/plugins/catalog.json",
        r#"{"version":1,"plugins":{"example":{"kind":"optional","files":[
        {"source":"plugins/example.lua","target":"plugins/example.lua"},
        {"source":"plugins/service.lua","target":"service/example.lua"}]}}}"#,
    );
    dir.write("sources/plugins/bundled-lua.txt", include_str!("../../plugins/bundled-lua.txt"));
    dir.write("sources/lua/unmerged_branches.lua", "return false");
    dir.write("sources/lua/tasks.lua", "return false");
    dir.write("sources/lua/helper.lua", "return 42");
    dir.write(
        "sources/plugins/example.lua",
        "deck.command('example', function() deck.notify(tostring(require('helper'))) end)",
    );
    dir.write(
        "sources/plugins/service.lua",
        "deck.service.command('example','value', function() return {value=require('helper')} end)",
    );
    let (mut ui, _rx) = engine(&dir);
    ui.use_source_dir(Some(dir.path().join("sources")));
    ui.load();
    assert!(ui.take_errors().is_empty());
    assert!(ui.run_command("example", &[]));
    assert_eq!(ui.take_requests(), [notice("42")]);
    assert_eq!(ui.eval("=deck.config_dir"), Some(dir.path().join("config").to_string_lossy().into_owned()));
    assert_eq!(ui.eval("=deck.plugin_dir"), Some(dir.path().join("sources").to_string_lossy().into_owned()));
    let mut service = Engine::new_service(
        Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("start") },
        Arc::new(|_| {}),
    )
    .unwrap();
    service.use_source_dir(Some(dir.path().join("sources")));
    service.load_service();
    assert!(service.take_errors().is_empty());
    assert_eq!(
        service.service_command("example", "value", &serde_json::json!({})).unwrap(),
        serde_json::json!({"value":42})
    );
    let (mut missing, _rx) = engine(&dir);
    missing.use_source_dir(Some(dir.path().join("missing")));
    missing.load();
    assert!(!missing.take_errors().is_empty());
    assert!(!missing.run_command("example", &[]));
}

#[test]
fn dot_management_runs_the_current_binary_in_both_plugin_modes() {
    let dir = TempDir::new("dot-source-mode");
    let (mut e, _) = engine(&dir);
    e.eval(include_str!("../../plugins/dot.lua"));
    assert!(e.run_command("dot", &["probe".into()]));
    assert!(e.take_errors().is_empty());
    let requests = e.take_requests();
    let [Request::Create { command, .. }] = requests.as_slice() else { panic!("Dot opens a terminal") };
    assert_eq!(
        command[0],
        std::env::current_exe().unwrap().to_string_lossy(),
        "management never executes an unvalidated provider command"
    );
    assert_eq!(&command[1..3], &["dot", "probe"]);
    e.use_source_dir(Some(dir.path().join("config")));
    e.eval(include_str!("../../plugins/dot.lua"));
    assert!(e.run_command("dot", &["probe".into()]));
    assert!(e.take_errors().is_empty());
    let requests = e.take_requests();
    let [Request::Create { command, .. }] = requests.as_slice() else { panic!("Dot opens a terminal") };
    assert_eq!(command[0], std::env::current_exe().unwrap().to_string_lossy());
    assert_eq!(&command[1..3], &["dot", "probe"]);
    assert_eq!(e.eval("=deck.plugin_source_dir"), Some(dir.path().join("config").to_string_lossy().into_owned()));
    e.use_source_dir(None);
    assert_eq!(e.eval("=deck.plugin_source_dir == nil"), Some("true".into()));
}

#[test]
fn repository_catalog_rejects_source_paths_outside_its_root() {
    let dir = TempDir::new("repo-plugin-boundaries");
    dir.write("outside.lua", "deck.command('escape', function() end)");
    for source in ["../outside.lua", "plugins/linked.lua"] {
        dir.write(
            "sources/plugins/catalog.json",
            format!(
                r#"{{"version":1,"plugins":{{"example":{{"kind":"optional","files":[
            {{"source":{source:?},"target":"plugins/example.lua"}}]}}}}}}"#
            ),
        );
        if source.ends_with("linked.lua") {
            std::os::unix::fs::symlink(dir.path().join("outside.lua"), dir.path().join("sources/plugins/linked.lua"))
                .unwrap();
        }
        assert!(sources::files(&dir.path().join("sources"), false).is_err());
    }
}

#[test]
fn fleet_facilities_use_typed_requests_and_the_existing_callback_shape() {
    let (_dir, mut engine, _rx) = loaded(
        "fleet-lua",
        r#"deck.slots.admission({},function(result) assert(result.ok); print(result.data.admitted) end); deck.scheduler.enqueue({provider='codex',placement='auto'})"#,
    );
    let requests = engine.take_requests();
    let callback = match &requests[0] {
        Request::Fleet { action, args, callback } => {
            assert_eq!(action, "admission");
            assert!(args.is_object());
            *callback
        }
        other => panic!("{other:?}"),
    };
    assert!(matches!(&requests[1],Request::Fleet{action,args,..} if action=="enqueue" && args["placement"]=="auto"));
    engine.service_result(callback, &serde_json::json!({"admitted":true}), None);
    assert_eq!(engine.take_requests(), vec![notice("true")]);
}

#[test]
fn a_running_deck_plugs_a_plugin_in_or_out_as_the_config_file_turns_it_on_or_off() {
    let dir = TempDir::new("lua-plugin-config");
    dir.write("config/plugins/hello.lua", r#"require("deck").command("hello", function() end)"#);
    let path = dir.write("config/config.json", r#"{"plugins":{}}"#);
    let (mut app, _rx) = crate::tui::tests::app_with(Vec::new(), Vec::new());
    app.state.start_dir = dir.path().join("start").to_string_lossy().into_owned();
    app.follow_config(path.clone());
    app.load_lua(
        Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("start") },
        Arc::new(|_| {}),
    );
    assert!(app.run_user_command("hello", ""));
    std::fs::write(&path, r#"{"plugins":{"hello":false}}"#).unwrap();
    app.check_config();
    assert!(!app.run_user_command("hello", ""), "turned off in the file, unloaded with no restart");
    std::fs::write(&path, r#"{"plugins":{"hello":false"#).unwrap();
    app.check_config();
    assert!(!app.run_user_command("hello", ""), "a file that does not parse keeps the running settings");
    std::fs::write(&path, r#"{"plugins":{"hello":true}}"#).unwrap();
    app.check_config();
    assert!(app.run_user_command("hello", ""), "turned on again, loaded again");
}

#[path = "task_sources_lua.rs"]
mod task_sources;

#[path = "unmerged_branches_lua.rs"]
mod unmerged_branches;

#[path = "agent_settings_lua.rs"]
mod agent_settings;
