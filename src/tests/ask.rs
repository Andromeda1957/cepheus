use super::*;
use crate::testsupport::{scaled_timeout, TempDir};

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| w.to_string()).collect()
}

#[test]
fn questions_distinguish_repository_context_web_lookups_and_general_chat() {
    for q in [
        "where is the hotkey parsed",
        "Why does src/lua.rs not build?",
        "what does `config.toml` set",
        "which file starts the dev server",
        "explain this stack trace",
        "what's in the README",
    ] {
        assert_eq!(classify(q, &[]), Route::Code, "{q}");
    }
    for q in [
        "hello",
        "thanks",
        "explain gravity",
        "what is the capital of france",
        "how tall is everest",
        "is .rs a domain",
    ] {
        assert_eq!(classify(q, &[]), Route::General, "{q}");
    }
    for q in ["latest rust release", "weather in Chicago", "search online for train tickets"] {
        assert_eq!(classify(q, &[]), Route::Web, "{q}");
    }
}

fn choose_with(cfg: &AskConfig, input: &str) -> Choice {
    choose(input, cfg)
}

#[test]
fn a_projects_own_words_mark_code_questions_only_when_configured() {
    assert_eq!(classify("what does task_state do", &[]), Route::General, "no project's names are built in");
    assert_eq!(classify("what does task_state do", &strings(&["Task_State"])), Route::Code);
    let cfg = AskConfig { code_words: strings(&["example"]), ..AskConfig::default() };
    assert_eq!(choose("is Example up", &cfg).route, Route::Code);
    assert_eq!(classify("anything", &strings(&[""])), Route::General, "an empty word matches nothing");
}

#[test]
fn a_prefix_chooses_who_answers() {
    let choice = |input: &str| {
        let c = choose(input, &AskConfig::default());
        (c.route, c.tool, c.text)
    };
    assert_eq!(choice("/web where is everest"), (Route::Web, None, "where is everest".into()));
    assert_eq!(choice("/code  what time is it"), (Route::Code, None, "what time is it".into()));
    assert_eq!(choice("/gemini hi"), (Route::General, Some("agy".into()), "hi".into()));
    assert_eq!(choice("/agy hi"), (Route::General, Some("agy".into()), "hi".into()));
    assert_eq!(choice("/agy latest news"), (Route::Web, Some("agy".into()), "latest news".into()));
    assert_eq!(choice("/agy this repo"), (Route::Code, Some("agy".into()), "this repo".into()));
    assert_eq!(choice("/general latest news"), (Route::General, None, "latest news".into()));
    assert_eq!(choice("/claude where is it"), (Route::Code, Some("claude".into()), "where is it".into()));
    assert_eq!(choice("/codex x"), (Route::Code, Some("codex".into()), "x".into()));
    // Not a prefix: the whole question, as typed.
    assert_eq!(choice("  /usr/bin is what? "), (Route::General, None, "/usr/bin is what?".into()));
    assert_eq!(choice("/web"), (Route::Web, None, String::new()));
}

#[test]
fn selected_routes_and_provider_overrides_preserve_the_appropriate_configured_model() {
    let cfg = AskConfig {
        general: AskRoute { tool: "agy".into(), model: "chat-model".into() },
        web: AskRoute { tool: "agy".into(), model: "lookup-model".into() },
        code: AskRoute { tool: "claude".into(), model: "code-model".into() },
        ..AskConfig::default()
    };
    assert_eq!(route_settings(&cfg, &choose_with(&cfg, "capital of peru")), cfg.general);
    assert_eq!(route_settings(&cfg, &choose_with(&cfg, "/web capital of peru")), cfg.web);
    let code = route_settings(&cfg, &choose_with(&cfg, "/code where is main"));
    assert_eq!(code, cfg.code);
    // Another program than the route's takes its own default model,
    // Antigravity uses its quick-question default.
    let codex = route_settings(&cfg, &choose_with(&cfg, "/codex where is main"));
    assert_eq!((codex.tool.as_str(), codex.model.as_str()), ("codex", ""));
    let mut agy_code = cfg.clone();
    agy_code.code = AskRoute { tool: "codex".into(), model: "gpt-x".into() };
    let agy = route_settings(&agy_code, &Choice { route: Route::Code, tool: Some("agy".into()), text: "q".into() });
    assert_eq!(agy, AskRoute::default());
    // The route's own program keeps its configured model.
    let mut custom = cfg;
    custom.general.model = "custom-chat-model".into();
    let same = route_settings(&custom, &choose_with(&custom, "/agy x"));
    assert_eq!(same.model, "custom-chat-model");
}

#[test]
fn the_question_is_one_argument_after_the_programs_own_read_only_flags() {
    let config = Config { antigravity_command: strings(&["/opt/agy/bin/agy", "--ignored"]), ..Config::default() };
    let hostile = "-rf $(rm -rf ~); `id` && echo pwned";
    let route = AskRoute { tool: "agy".into(), model: "selected-model".into() };
    let agy = command(&config, &route, hostile, None).unwrap();
    let policy = serde_json::json!({
        "agentMode": "",
        "toolPermission": "request-review",
        "permissions": {
            "allow": ["read_url(*)"],
            "deny": ["write_file(*)", "command(*)", "execute_url(*)", "mcp(*)"]
        }
    })
    .to_string();
    assert_eq!(
        agy,
        strings(&[
            "/opt/agy/bin/agy",
            "--model",
            "selected-model",
            "--disable-slash-commands",
            "--settings",
            &policy,
            "--output-format",
            "stream-json",
            "--print-timeout",
            "175s",
            "-p",
            hostile,
        ])
    );
    let id = "4df78217-b2e1-4e1a-a026-e82391c469a2";
    let more = command(&config, &route, "more", Some(id)).unwrap();
    assert_eq!(more[..5], strings(&["/opt/agy/bin/agy", "--model", "selected-model", "--conversation", id]));
    assert_eq!(more[more.len() - 2..], strings(&["-p", "more"]));
    let sonnet = AskRoute { tool: "claude".into(), model: "sonnet".into() };
    let claude = command(&config, &sonnet, " where? ", None).unwrap();
    let streamed = ["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages"];
    let read_only = ["--permission-mode", "plan", "--model", "sonnet"];
    assert_eq!(claude[1..], strings(&[&streamed[..], &read_only[..], &["--", "where?"]].concat()));
    let claude = command(&config, &sonnet, "and?", Some(id)).unwrap();
    assert_eq!(claude[1..], strings(&[&streamed[..], &read_only[..], &["--resume", id, "--", "and?"]].concat()));
    let route = AskRoute { tool: "codex".into(), model: String::new() };
    let codex = command(&config, &route, "-v", None).unwrap();
    assert_eq!(codex[1..], strings(&["exec", "--json", "--sandbox", "read-only", "--skip-git-repo-check", "--", "-v"]));
    // `exec resume` has no --sandbox option; the setting is the same policy.
    let codex = command(&config, &route, "-v", Some(id)).unwrap();
    assert_eq!(
        codex[1..],
        strings(&[
            "exec",
            "resume",
            "--json",
            "-c",
            "sandbox_mode=\"read-only\"",
            "--skip-git-repo-check",
            id,
            "--",
            "-v"
        ])
    );
}

#[test]
fn a_question_is_refused_when_empty_too_long_or_for_a_bad_model_program_or_conversation() {
    let config = Config::default();
    let route = AskRoute::default();
    assert!(command(&config, &route, "   ", None).is_err());
    assert!(command(&config, &route, &"x".repeat(MAX_QUESTION + 1), None).is_err());
    assert!(command(&config, &route, &"x".repeat(MAX_QUESTION), None).is_ok());
    for model in ["--yolo", "a b", "x;id", "$(id)"] {
        let route = AskRoute { tool: "claude".into(), model: model.into() };
        assert!(command(&config, &route, "q", None).is_err(), "{model}");
    }
    assert!(command(&config, &AskRoute { tool: "sh".into(), model: String::new() }, "q", None).is_err());
    // A conversation id goes before the question as an argument of its own.
    for id in ["--yolo", "", "a b", "x;id", &"a".repeat(129)] {
        for tool in ["agy", "claude", "codex"] {
            let route = AskRoute { tool: tool.into(), model: String::new() };
            assert!(command(&config, &route, "q", Some(id)).is_err(), "{tool} {id}");
        }
    }
}

#[test]
fn a_slash_word_is_a_command_and_only_known_ones_are_accepted() {
    assert_eq!(typed("/clear"), Typed::Clear(""));
    assert_eq!(typed("  /new what now "), Typed::Clear("what now"));
    assert_eq!(typed("/web x"), Typed::Question);
    assert_eq!(typed("/codex"), Typed::Question);
    assert_eq!(typed("/help me"), Typed::Unknown("/help"));
    assert_eq!(typed("/usr/bin is what?"), Typed::Question);
    assert_eq!(typed("what is 1/2"), Typed::Question);
}

#[test]
fn a_follow_up_continues_the_conversation_unless_another_program_model_or_folder_is_chosen() {
    let cfg = AskConfig::default();
    let (start, state) = (Path::new("/srv/repo"), Path::new("/state"));
    let first = plan(&cfg, "capital of peru", None, None, start, state).unwrap();
    assert_eq!((first.route, first.resume.as_deref(), first.automatic), (Route::General, None, true));
    let thread = first.thread("c1".into());
    // No prefix: the same conversation, whatever the question is about.
    let next = plan(&cfg, "where is main.rs defined", None, Some(&thread), start, state).unwrap();
    assert_eq!((next.resume.as_deref(), next.settings.tool.as_str()), (Some("c1"), "agy"));
    assert_eq!((next.route, next.cwd.as_path()), (Route::General, Path::new("/state/ask")));
    // Choosing the same program, model and folder keeps it.
    let web = plan(&cfg, "/web latest news", None, Some(&thread), start, state).unwrap();
    assert_eq!((web.route, web.resume.as_deref(), web.question.as_str()), (Route::Web, Some("c1"), "latest news"));
    // Another program or folder starts a new one.
    let code = plan(&cfg, "/code where is main", None, Some(&thread), start, state).unwrap();
    assert_eq!((code.settings.tool.as_str(), code.resume.as_deref(), code.cwd.as_path()), ("claude", None, start));
    let picked = plan(&cfg, "where is main", Some(Route::Code), Some(&thread), start, state).unwrap();
    assert_eq!((picked.route, picked.resume.as_deref()), (Route::Code, None));
    assert_eq!(code.continued(), None);
    assert_eq!(next.continued(), Some(thread.clone()));
    // Commands are never asked.
    assert!(plan(&cfg, "/clear", None, Some(&thread), start, state).is_err());
    let unknown = plan(&cfg, "/foo bar", None, None, start, state).unwrap_err();
    assert!(unknown.to_string().contains("/foo is not a command"), "{unknown}");
}

fn read(tool: &str, lines: &[&str]) -> Progress {
    let mut said = Progress::default();
    for line in lines {
        said.read(tool, line);
    }
    said
}

/// Antigravity's `step_update` line, as `agy --output-format stream-json`
/// printed it on 2026-11-10.
fn agy_step(index: u32, state: &str, rest: &str) -> String {
    format!(
        r#"{{"event":"step_update","step_update":{{"conversation_id":"4df78217-b2e1-4e1a-a026-e82391c469a2","step_index":{index},"state":"{state}",{rest}}}}}"#
    )
}

fn agy_text(index: u32, delta: &str) -> String {
    agy_step(index, "ACTIVE", &format!(r#""step_type":"agent_response","text_delta":{}"#, serde_json::json!(delta)))
}

#[test]
fn antigravitys_stream_gives_the_last_reply_its_conversation_and_the_tool_in_use() {
    let id = "4df78217-b2e1-4e1a-a026-e82391c469a2";
    let mut said = Progress::default();
    let init = format!(r#"{{"event":"init","conversation_id":"{id}","init":{{"model":"gemini-3.8-flash-high"}}}}"#);
    assert!(!said.read("agy", &init));
    assert_eq!(said.conversation.as_deref(), Some(id));
    assert!(!said.read("agy", &agy_step(0, "DONE", r#""step_type":"user_input""#)));
    assert!(said.read("agy", &agy_text(1, "Let me look.")));
    let search = r#""step_type":"tool","tool_name":"search_web","tool_info":{"name":"search_web","parameters":{"query":"latest stable Rust version release"}}"#;
    assert!(said.read("agy", &agy_step(2, "ACTIVE", search)));
    assert_eq!(said.activity.as_deref(), Some("search_web · latest stable Rust version release"));
    assert!(said.read("agy", &agy_step(2, "DONE", search)));
    assert_eq!(said.activity, None);
    // A later reply replaces the earlier one, as the result does.
    said.read("agy", &agy_text(3, "Owls are"));
    said.read("agy", &agy_text(3, " birds.\n"));
    assert_eq!(said.text, "Owls are birds.\n");
    let result = format!(
        r#"{{"event":"result","result":{{"conversation_id":"{id}","status":"SUCCESS","response":"Owls are birds of prey.\n","num_turns":1}}}}"#
    );
    said.read("agy", &result);
    assert_eq!((said.answer(), said.error.as_deref()), ("Owls are birds of prey.\n", None));
    let failed = read("agy", &[r#"{"event":"result","result":{"conversation_id":"x","status":"CANCELLED"}}"#]);
    assert_eq!(failed.error.as_deref(), Some("it ended with CANCELLED"));
}

#[test]
fn claudes_stream_gives_the_text_as_written_and_the_message_after_its_tools() {
    let session = r#""session_id":"22fc2e34-175d-4dc7-aa86-30ea686cd9df","parent_tool_use_id":null"#;
    let event = |event: &str| format!(r#"{{"type":"stream_event","event":{event},{session}}}"#);
    let text = |text: &str| {
        event(&format!(r#"{{"type":"content_block_delta","index":1,"delta":{{"type":"text_delta","text":"{text}"}}}}"#))
    };
    let mut said = Progress::default();
    said.read("claude", &event(r#"{"type":"message_start","message":{"id":"x"}}"#));
    let thinking = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Hmm"}}"#;
    assert!(!said.read("claude", &event(thinking)), "thinking is not the answer");
    assert!(said.read("claude", &text("Let me check.")));
    assert_eq!(said.conversation.as_deref(), Some("22fc2e34-175d-4dc7-aa86-30ea686cd9df"));
    let grep = r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","name":"Grep"}}"#;
    assert!(said.read("claude", &event(grep)));
    assert_eq!(said.activity.as_deref(), Some("Grep"));
    said.read("claude", &event(r#"{"type":"message_start","message":{"id":"y"}}"#));
    said.read("claude", &text("Owls"));
    said.read("claude", &text(" can r"));
    assert_eq!((said.text.as_str(), said.activity.as_deref()), ("Owls can r", None));
    said.read(
        "claude",
        &format!(r#"{{"type":"result","subtype":"success","is_error":false,"result":"Owls can rotate.",{session}}}"#),
    );
    assert_eq!((said.answer(), said.error.as_deref()), ("Owls can rotate.", None));
    let failed =
        read("claude", &[r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":""}"#]);
    assert_eq!(failed.error.as_deref(), Some("error_during_execution"));
}

#[test]
fn codexs_stream_gives_its_last_message_its_thread_and_the_command_running() {
    let mut said = Progress::default();
    said.read("codex", r#"{"type":"thread.started","thread_id":"01a1c8e6-a9a8-7691-a055-947b7b22e0ea"}"#);
    said.read("codex", r#"{"type":"turn.started"}"#);
    said.read(
        "codex",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"I’ll list the files."}}"#,
    );
    let ls = r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"/usr/bin/zsh -lc 'ls -la'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#;
    assert!(said.read("codex", ls));
    assert_eq!(said.activity.as_deref(), Some("/usr/bin/zsh -lc 'ls -la'"));
    said.read("codex", &ls.replace("item.started", "item.completed"));
    said.read(
        "codex",
        r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"It holds **4** files."}}"#,
    );
    said.read("codex", r#"{"type":"turn.completed","usage":{"input_tokens":1}}"#);
    assert_eq!((said.answer(), said.activity.as_deref()), ("It holds **4** files.", None));
    assert_eq!(said.conversation.as_deref(), Some("01a1c8e6-a9a8-7691-a055-947b7b22e0ea"));
    let failed = read(
        "codex",
        &["Reading additional input from stdin...", r#"{"type":"turn.failed","error":{"message":"quota"}}"#],
    );
    assert_eq!(
        (failed.error.as_deref(), failed.other.as_deref()),
        (Some("quota"), Some("Reading additional input from stdin..."))
    );
}

#[test]
fn code_questions_run_in_the_start_folder_and_web_ones_in_a_folder_of_their_own() {
    assert_eq!(workdir(Route::Code, Path::new("/srv/repo"), Path::new("/state")), PathBuf::from("/srv/repo"));
    assert_eq!(workdir(Route::General, Path::new("/srv/repo"), Path::new("/state")), PathBuf::from("/state/ask"));
    assert_eq!(workdir(Route::Web, Path::new("/srv/repo"), Path::new("/state")), PathBuf::from("/state/ask"));
}

#[test]
fn an_answer_streams_from_the_programs_json_and_is_bounded() {
    let dir = TempDir::new("ask-answer");
    let cwd = dir.path().join("ask");
    let probe = dir.calm_probe();
    let limit = Duration::from_secs(20);
    let never = AtomicBool::new(false);
    let id = "4df78217-b2e1-4e1a-a026-e82391c469a2";
    let lines = [
        format!(r#"{{"event":"init","conversation_id":"{id}"}}"#),
        agy_text(1, "Li"),
        agy_text(1, "ma."),
        format!(
            r#"{{"event":"result","result":{{"conversation_id":"{id}","status":"SUCCESS","response":"  Lima.  "}}}}"#
        ),
    ];
    let stream = dir.write("agy.jsonl", lines.join("\n") + "\n").display().to_string();
    let mut seen = Vec::new();
    let reply = stream_answer("agy", &strings(&["cat", &stream]), &cwd, limit, &probe, &never, &mut |said| {
        seen.push(said.text.clone())
    })
    .unwrap();
    assert_eq!(reply, Reply { text: "Lima.".into(), conversation: Some(id.into()), stopped: false });
    assert_eq!(seen, ["Li", "Lima."], "the text shows as it is written");
    assert!(cwd.is_dir(), "the web folder is made on first use");
    let answer = |argv: &[&str]| stream_answer("agy", &strings(argv), &cwd, limit, &probe, &never, &mut |_| {});
    let failed = answer(&["sh", "-c", "echo progress; echo 'quota exceeded' >&2; exit 1"]);
    assert!(failed.unwrap_err().to_string().contains("quota exceeded"));
    let refused = r#"{"event":"result","result":{"status":"ERROR","error":{"message":"model unavailable"}}}"#;
    let failed = answer(&["echo", refused]);
    assert!(failed.unwrap_err().to_string().contains("model unavailable"), "a failure it reports is one");
    assert!(answer(&["true"]).unwrap_err().to_string().contains("empty"));
    let long = agy_text(1, &"x".repeat(100_000));
    let long = answer(&["echo", &long]).unwrap().text;
    assert!(long.len() <= MAX_ANSWER + "\n…".len() && long.ends_with("\n…"), "{}", long.len());
    // The stream is not kept as it comes, so the answer being written is
    // bounded however many pieces a program sends.
    // One byte first, so the bound falls inside a two-byte "é".
    let pieces: Vec<String> =
        std::iter::once(agy_text(1, "x")).chain((0..300).map(|_| agy_text(1, &"é".repeat(500)))).collect();
    let said = read("agy", &pieces.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(said.text.len() <= 2 * MAX_ANSWER && said.text.len() > MAX_ANSWER, "{}", said.text.len());
}

#[test]
fn stopping_an_answer_ends_its_program_and_keeps_what_came() {
    let dir = TempDir::new("ask-stop");
    let stream = dir.write("partial.jsonl", agy_text(1, "Owls are") + "\n").display().to_string();
    let stop = AtomicBool::new(false);
    let started = std::time::Instant::now();
    // The program would go on for ten minutes; the first text stops it.
    let argv = strings(&["sh", "-c", "cat \"$1\"; exec sleep 600", "sh", &stream]);
    let reply =
        stream_answer("agy", &argv, dir.path(), Duration::from_secs(900), &dir.calm_probe(), &stop, &mut |_| {
            stop.store(true, Ordering::SeqCst)
        })
        .unwrap();
    // Unstopped, the program runs 600 seconds; the bound stays below that
    // at the highest load factor.
    assert!(started.elapsed() < scaled_timeout(Duration::from_secs(30)), "{:?}", started.elapsed());
    let conversation = Some("4df78217-b2e1-4e1a-a026-e82391c469a2".to_string());
    assert_eq!(reply, Reply { text: "Owls are".into(), conversation, stopped: true });
}

#[test]
fn the_history_keeps_the_latest_questions() {
    let dir = TempDir::new("ask-history");
    assert!(load_history(dir.path()).is_empty());
    let asked: Vec<Asked> = (0..50)
        .map(|i| Asked { question: format!("q{i}"), who: "agy".into(), answer: "a".into(), at: i, ..Asked::default() })
        .collect();
    save_history(dir.path(), &asked);
    let loaded = load_history(dir.path());
    assert_eq!(loaded.len(), HISTORY_LEN);
    assert_eq!((loaded[0].question.as_str(), loaded[HISTORY_LEN - 1].question.as_str()), ("q10", "q49"));
    // A broken file is no history, not an error.
    dir.write("ask-history.json", "{broken");
    assert!(load_history(dir.path()).is_empty());
}

#[test]
fn concurrent_question_history_appends_keep_every_answer_and_bound_the_file() {
    let dir = TempDir::new("ask-concurrent-history");
    std::thread::scope(|scope| {
        for at in 0..8 {
            let path = dir.path();
            scope.spawn(move || {
                append_history(
                    path,
                    &Asked {
                        question: format!("q{at}"),
                        who: "phone".into(),
                        answer: "answer".into(),
                        at,
                        ..Asked::default()
                    },
                )
                .unwrap();
            });
        }
    });
    let history = load_history(dir.path());
    assert_eq!(history.len(), 8);
    assert_eq!(history.iter().map(|a| &a.question).collect::<std::collections::HashSet<_>>().len(), 8);
    for at in 8..50 {
        append_history(
            dir.path(),
            &Asked {
                question: format!("q{at}"),
                who: "desktop".into(),
                answer: "answer".into(),
                at,
                ..Asked::default()
            },
        )
        .unwrap();
    }
    assert_eq!(load_history(dir.path()).len(), HISTORY_LEN);
}

#[test]
fn invalid_or_symlinked_history_is_preserved_on_append_failure() {
    let dir = TempDir::new("ask-invalid-history");
    let asked = Asked { question: "q".into(), who: "phone".into(), answer: "a".into(), at: 1, ..Asked::default() };
    dir.write("ask-history.json", "{broken");
    assert!(append_history(dir.path(), &asked).is_err());
    assert_eq!(std::fs::read_to_string(history_path(dir.path())).unwrap(), "{broken");
    std::fs::remove_file(history_path(dir.path())).unwrap();
    dir.write("outside.json", "[]");
    std::os::unix::fs::symlink(dir.path().join("outside.json"), history_path(dir.path())).unwrap();
    assert!(append_history(dir.path(), &asked).is_err());
    assert_eq!(std::fs::read_to_string(dir.path().join("outside.json")).unwrap(), "[]");
    // Clearing it goes through no link either.
    dir.write("outside.json", "[1]");
    assert!(clear_history(dir.path()).is_err());
    assert_eq!(std::fs::read_to_string(dir.path().join("outside.json")).unwrap(), "[1]");
    std::fs::remove_file(history_path(dir.path())).unwrap();
    append_history(dir.path(), &asked).unwrap();
    clear_history(dir.path()).unwrap();
    assert!(load_history(dir.path()).is_empty());
}
