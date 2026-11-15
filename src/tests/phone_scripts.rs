use super::*;
use crate::testsupport::TempDir;

fn scripts(init: &str) -> (TempDir, Scripts) {
    let dir = TempDir::new("phone-scripts");
    dir.write("config/init.lua", init);
    std::fs::create_dir_all(dir.path().join("repo")).unwrap();
    let cfg = Config::default();
    let places = Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("repo") };
    let state = DeckState { start_dir: dir.path().join("repo").to_string_lossy().into_owned(), ..DeckState::default() };
    let sink: JobSink = std::sync::Arc::new(|_| {});
    let scripts = Scripts::new(cfg, places, state, Vec::new(), sink, PROTOCOL_VERSION);
    (dir, scripts)
}

fn messages(value: &Value) -> String {
    value["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn phone_lists_installed_script_commands_and_options_and_runs_them() {
    let (_dir, mut scripts) = scripts(
        r#"
        local d = require("deck")
        d.opt.phone_test = "initial"
        d.command("greet", function(args) d.notify("hello " .. args[1]) end,
          { desc = "Say hello", complete = { "world", "deck" } })
    "#,
    );
    let listing = scripts.perform(&json!({"operation":"list"})).unwrap();
    assert_eq!(listing["commands"][0]["name"], "greet");
    assert_eq!(listing["commands"][0]["desc"], "Say hello");
    assert_eq!(listing["options"]["phone_test"], "initial");
    assert!(listing["loaded"].as_array().unwrap().iter().any(|s| s == "init.lua"));
    let result = scripts.perform(&json!({"operation":"command","name":"greet","args":["world with spaces"]})).unwrap();
    assert!(messages(&result).contains("hello world with spaces"), "{result}");
    let changed = scripts.perform(&json!({"operation":"set","name":"phone_test","value":"changed"})).unwrap();
    assert_eq!(changed["options"]["phone_test"], "changed");
}

#[test]
fn phone_evaluates_lua_and_recovers_after_a_guarded_failure() {
    let (_dir, mut scripts) = scripts("");
    let value = scripts.perform(&json!({"operation":"eval","code":"=6 * 7"})).unwrap();
    assert!(messages(&value).contains("42"), "{value}");
    let bad = scripts.perform(&json!({"operation":"eval","code":"os.execute('touch unavailable')"})).unwrap();
    assert!(bad["messages"].as_array().unwrap().iter().any(|m| m["error"] == true), "{bad}");
    let started = Instant::now();
    let bounded = scripts.perform(&json!({"operation":"eval","code":"while true do end"})).unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(messages(&bounded).contains("ran too long"), "{bounded}");
    let value = scripts.perform(&json!({"operation":"eval","code":"=7 * 8"})).unwrap();
    assert!(messages(&value).contains("56"), "{value}");
}

#[test]
fn phone_command_starts_a_session_through_the_shared_dispatcher() {
    let (_dir, mut scripts) = scripts(
        r#"
        local d = require("deck")
        d.command("work", function(args) d.new("claude", {cwd=args[1],name="Phone work"}) end)
    "#,
    );
    let result = scripts.perform(&json!({"operation":"command","name":"work","args":["/srv/work tree"]})).unwrap();
    assert!(!messages(&result).contains("not a"));
    let creates: Vec<_> = scripts
        .output
        .try_iter()
        .filter_map(|msg| match msg {
            ClientMsg::Create { tool, cwd, name, .. } => Some((tool, cwd, name)),
            _ => None,
        })
        .collect();
    assert_eq!(
        creates,
        vec![(crate::protocol::Tool::Claude, Some("/srv/work tree".into()), Some("Phone work".into()))]
    );
}

#[test]
fn phone_picker_rejects_stale_tokens_and_changed_or_missing_items() {
    let (_dir, mut scripts) = scripts(
        r#"
        local d = require("deck")
        d.command("pick", function()
          d.ui.pick({title="Choose", items={"first","second"}, on_choose=function(item,index) d.notify(item .. ":" .. index) end})
        end)
    "#,
    );
    let first = scripts.perform(&json!({"operation":"command","name":"pick"})).unwrap();
    let token = first["picker"]["callback"].as_u64().unwrap();
    assert_eq!(first["picker"]["items"], json!(["first", "second"]));
    assert!(scripts.perform(&json!({"operation":"choose","callback":token+999,"index":0})).is_err());
    assert!(scripts.perform(&json!({"operation":"choose","callback":token,"index":99})).is_err());
    assert!(scripts.perform(&json!({"operation":"choose","callback":token,"index":1,"item":"wrong"})).is_err());
    let chosen = scripts.perform(&json!({"operation":"choose","callback":token,"index":1,"item":"second"})).unwrap();
    assert!(messages(&chosen).contains("second:2"), "{chosen}");
    assert!(chosen["picker"].is_null());
    assert!(scripts.perform(&json!({"operation":"choose","callback":token,"index":0})).is_err());
}

#[test]
fn phone_panel_actions_refresh_navigate_and_cancel_without_a_keyboard() {
    let (_dir, mut scripts) = scripts(
        r#"
        local d = require("deck")
        local page, render = "overview"
        render = function()
          local items = {{page, heading=true}, {"snapshot", inert=true}}
          if page == "overview" then items[3], items[4] = "Jobs", "Health"
          elseif page == "jobs" then items[3] = "nightly" end
          local keys = {{key="r", label="Refresh"}}
          if page ~= "overview" then table.insert(keys, 1, {key="backspace", label="Back"}) end
          d.ui.pick({title=page, items=items, filter=false, keys=keys,
            on_choose=function(item, index, key)
              if key == "r" then
                if page == "overview" then assert(item == "Jobs" and index == 3)
                elseif page == "jobs" then assert(item == "nightly" and index == 3)
                else assert(item == nil and index == nil) end
                d.notify("refresh:" .. page)
              elseif key == "backspace" then page = page == "job" and "jobs" or "overview"
              elseif page == "overview" then page = index == 3 and "jobs" or "health"
              elseif page == "jobs" then page = "job" end
              render()
            end,
            on_close=function() d.notify("closed:" .. page) end})
        end
        d.command("panel", render)
    "#,
    );
    let choose = |scripts: &mut Scripts, shown: &Value, item: &str| {
        let index = shown["picker"]["items"].as_array().unwrap().iter().position(|value| value == item).unwrap();
        scripts
            .perform(&json!({"operation":"choose", "callback":shown["picker"]["callback"], "index":index, "item":item}))
            .unwrap()
    };
    let overview = scripts.perform(&json!({"operation":"command","name":"panel"})).unwrap();
    assert_eq!(overview["picker"]["items"], json!(["overview", "snapshot", "Jobs", "Health", "Refresh"]));
    assert!(scripts
        .perform(&json!({"operation":"choose","callback":overview["picker"]["callback"],"index":4,"item":"Changed"}))
        .is_err());
    let refreshed = choose(&mut scripts, &overview, "Refresh");
    assert!(messages(&refreshed).contains("refresh:overview"));
    assert_ne!(overview["picker"]["callback"], refreshed["picker"]["callback"]);
    assert!(scripts
        .perform(&json!({"operation":"choose","callback":overview["picker"]["callback"],"index":4,"item":"Refresh"}))
        .is_err());
    let jobs = choose(&mut scripts, &refreshed, "Jobs");
    let jobs = choose(&mut scripts, &jobs, "Refresh");
    assert!(messages(&jobs).contains("refresh:jobs"));
    let detail = choose(&mut scripts, &jobs, "nightly");
    assert_eq!(detail["picker"]["items"], json!(["job", "snapshot", "Back", "Refresh"]));
    let detail = choose(&mut scripts, &detail, "Refresh");
    assert!(messages(&detail).contains("refresh:job"));
    let jobs = choose(&mut scripts, &detail, "Back");
    assert_eq!(jobs["picker"]["title"], "jobs");
    let overview = choose(&mut scripts, &jobs, "Back");
    let health = choose(&mut scripts, &overview, "Health");
    let health = choose(&mut scripts, &health, "Refresh");
    assert!(messages(&health).contains("refresh:health"));
    let overview = choose(&mut scripts, &health, "Back");
    assert_eq!(overview["picker"]["title"], "overview");
    let cancelled = scripts
        .perform(&json!({"operation":"choose","callback":overview["picker"]["callback"],"cancel":true}))
        .unwrap();
    assert!(cancelled["picker"].is_null());
    assert!(messages(&cancelled).contains("closed:overview"));
    assert!(scripts.perform(&json!({"operation":"list"})).unwrap()["picker"].is_null());
}

#[test]
fn phone_turns_a_panels_pages_without_a_keyboard() {
    let (_dir, mut scripts) = scripts(
        r#"
        local d = require("deck")
        local render
        render = function(page)
          d.ui.pick({title="Operations", items={{page, inert=true}}, tabs={"Overview", {"Jobs", 2}},
            tab=page == "jobs" and 2 or 1, keys={{key="r", label="Refresh"}},
            on_tab=function(label, index) d.notify("tab:" .. label .. ":" .. index); render(label:lower()) end,
            on_choose=function() end})
        end
        d.command("panel", function() render("overview") end)
    "#,
    );
    let overview = scripts.perform(&json!({"operation":"command","name":"panel"})).unwrap();
    assert_eq!(overview["picker"]["items"], json!(["overview", "Refresh", "Overview", "Jobs"]));
    let token = &overview["picker"]["callback"];
    assert!(scripts.perform(&json!({"operation":"choose","callback":token,"index":3,"item":"Health"})).is_err());
    let jobs = scripts.perform(&json!({"operation":"choose","callback":token,"index":3,"item":"Jobs"})).unwrap();
    assert!(messages(&jobs).contains("tab:Jobs:2"), "{jobs}");
    assert_eq!(jobs["picker"]["items"][0], "jobs");
}

#[test]
fn reload_does_not_reuse_a_phone_picker_token() {
    let (_dir, mut scripts) = scripts(
        r#"
        local d = require("deck")
        d.command("pick", function() d.ui.pick({title="Choose", items={"a"}, on_choose=function(item) d.notify(item) end}) end)
    "#,
    );
    let first = scripts.perform(&json!({"operation":"command","name":"pick"})).unwrap();
    let token = first["picker"]["callback"].as_u64().unwrap();
    scripts.perform(&json!({"operation":"reload"})).unwrap();
    let second = scripts.perform(&json!({"operation":"command","name":"pick"})).unwrap();
    assert_ne!(token, second["picker"]["callback"].as_u64().unwrap());
    assert!(scripts.perform(&json!({"operation":"choose","callback":token,"index":0})).is_err());
    let next = second["picker"]["callback"].as_u64().unwrap();
    let cancelled = scripts.perform(&json!({"operation":"choose","callback":next,"cancel":true})).unwrap();
    assert!(cancelled["picker"].is_null());
}

#[test]
fn malformed_or_unbounded_phone_script_operations_are_rejected() {
    let (_dir, mut scripts) = scripts("");
    for args in [
        json!({"operation":"arbitrary"}),
        json!({"operation":false}),
        json!({"operation":"list","id":-1}),
        json!({"operation":"eval","code":""}),
        json!({"operation":"eval","code":"x".repeat(MAX_CODE+1)}),
        json!({"operation":"command","name":"missing"}),
        json!({"operation":"command","name":"whatever","args":true}),
        json!({"operation":"set","name":"x","value":"\u{0}"}),
        json!({"operation":"list","id":123456}),
    ] {
        assert!(scripts.perform(&args).is_err(), "{args}");
    }
}

#[test]
fn phone_script_navigation_is_explicit_and_consumed_by_the_next_operation() {
    let (_dir, mut scripts) = scripts(
        r#"
        local d=require("deck")
        d.command("show", function() d.select(42) end)
    "#,
    );
    scripts.app.state.sessions.push(crate::tui::tests::session(
        42,
        crate::protocol::Tool::Claude,
        crate::protocol::Status::Idle,
        true,
    ));
    scripts.app.current = Some(Sel::Session(42));
    assert!(scripts.perform(&json!({"operation":"list"})).unwrap()["selected"].is_null());
    let shown = scripts.perform(&json!({"operation":"command","name":"show"})).unwrap();
    assert_eq!(shown["selected"], 42);
    assert_eq!(shown["selected_tool"], "claude");
    assert!(scripts.perform(&json!({"operation":"list"})).unwrap()["selected"].is_null());
}

#[test]
fn phone_options_accept_typed_json_and_empty_text() {
    let (_dir, mut scripts) = scripts("");
    for value in [
        json!(false),
        json!(42),
        json!({"a":[1,true]}),
        json!("false"),
        json!("null"),
        json!("42"),
        json!("{\"x\":1}"),
        json!(""),
    ] {
        let changed = scripts.perform(&json!({"operation":"set","name":"phone_value","value":value})).unwrap();
        assert_eq!(changed["options"]["phone_value"], value);
    }
}

#[test]
fn a_script_created_shell_waits_for_its_state_before_phone_navigation() {
    let (_dir, mut scripts) = scripts(
        r#"
        local d=require("deck")
        d.command("open-shell", function() d.term({name="Script shell"}) end)
    "#,
    );
    let (mut stream, _peer) = UnixStream::pair().unwrap();
    let (events, input) = mpsc::channel();
    let producer = thread::spawn(move || {
        events.send(Event::Server(Box::new(ServerMsg::Created { id: 42, request: None }))).unwrap();
        thread::sleep(Duration::from_millis(30));
        let state = DeckState {
            sessions: vec![crate::tui::tests::session(
                42,
                crate::protocol::Tool::Shell,
                crate::protocol::Status::Idle,
                true,
            )],
            ..DeckState::default()
        };
        let _ = events.send(Event::Server(Box::new(ServerMsg::State(state))));
    });
    let result = perform_and_flush(
        &mut scripts,
        &mut stream,
        &json!({"operation":"command","name":"open-shell"}),
        &input,
        &mut Default::default(),
    )
    .unwrap();
    producer.join().unwrap();
    assert_eq!(result["selected"], 42);
    assert_eq!(result["selected_tool"], "shell");
}

#[test]
fn a_timed_out_script_creation_cannot_satisfy_a_later_command() {
    let (_dir, mut scripts) = scripts(
        r#"
        local d=require("deck")
        d.command("open-shell", function() d.term({}) end)
    "#,
    );
    let (mut stream, _peer) = UnixStream::pair().unwrap();
    let (events, input) = mpsc::channel();
    let operation = json!({"operation":"command","name":"open-shell"});
    assert!(perform_and_flush(&mut scripts, &mut stream, &operation, &input, &mut Default::default()).is_err());
    events.send(Event::Server(Box::new(ServerMsg::Created { id: 77, request: None }))).unwrap();
    let state = DeckState {
        sessions: vec![crate::tui::tests::session(
            77,
            crate::protocol::Tool::Shell,
            crate::protocol::Status::Idle,
            true,
        )],
        ..DeckState::default()
    };
    events.send(Event::Server(Box::new(ServerMsg::State(state)))).unwrap();
    let second = perform_and_flush(&mut scripts, &mut stream, &operation, &input, &mut Default::default());
    assert!(second.is_err(), "The earlier creation answered a later command: {second:?}");
}
