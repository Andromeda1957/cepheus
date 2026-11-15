use super::*;
use crate::testsupport::TempDir;
use serde_json::json;
use std::io::Write;

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"type":"response_item","payload":{"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()}})
}

fn result(id: &str, output: Value) -> Value {
    json!({"type":"response_item","timestamp":"2026-11-05T12:00:00Z","payload":{"type":"function_call_output","call_id":id,"output":output}})
}

fn lines(records: &[Value]) -> String {
    records.iter().map(|v| v.to_string() + "\n").collect()
}

fn append(path: &Path, records: &[Value]) {
    fs::OpenOptions::new().append(true).open(path).unwrap().write_all(lines(records).as_bytes()).unwrap();
}

const KILL: &str = "Out of memory: Killed process 4123 (rustc) total-vm:1234kB";

#[test]
fn codex_kill_keeps_exact_command_and_stable_event_identity() {
    let dir = TempDir::new("provider-memory-exec");
    let command = "cd /srv/project && cargo test --locked\n";
    let path = dir.write(
        "rollout.jsonl",
        lines(&[
            call("build-a", "exec_command", json!({"cmd":command,"workdir":"/srv/project/cepheus"})),
            result("build-a", json!({"output":KILL,"exit_code":137})),
        ]),
    );
    let mut scan = Scanner::default();
    let kills = scan.read(&path, Tool::Codex, 1, &[]);
    assert_eq!(
        kills,
        vec![MemoryKill {
            id: "codex:build-a".into(),
            command: command.into(),
            cwd: Some("/srv/project/cepheus".into()),
            at: codex::parse_rfc3339_ms("2026-11-05T12:00:00Z").unwrap() / 1000,
            authoritative: false
        }]
    );
    let offset = scan.offset;
    assert_eq!(scan.read(&path, Tool::Codex, 99, &[]), kills);
    assert_eq!(scan.offset, offset);
    append(&path, &[result("unknown-call", json!({"output":KILL,"exit_code":137}))]);
    assert_eq!(scan.read(&path, Tool::Codex, 99, &[]), kills, "unknown results cannot borrow an earlier call");
}

#[test]
fn codex_write_stdin_correlates_session_not_the_last_command() {
    let dir = TempDir::new("provider-memory-poll");
    let path = dir.write(
        "rollout.jsonl",
        lines(&[
            call("tests", "exec_command", json!({"cmd":"python3 -m pytest tests/"})),
            result("tests", json!({"output":"still running","session_id":44})),
        ]),
    );
    let mut scan = Scanner::default();
    assert!(scan.read(&path, Tool::Codex, 1, &[]).is_empty());
    append(
        &path,
        &[
            call("inspect", "exec_command", json!({"cmd":"git status --short"})),
            result("inspect", json!({"output":"clean","exit_code":0})),
            call("poll", "write_stdin", json!({"session_id":44,"chars":""})),
            result("poll", json!({"output":KILL,"exit_code":137})),
        ],
    );
    let kills = scan.read(&path, Tool::Codex, 2, &[]);
    assert_eq!((kills[0].id.as_str(), kills[0].command.as_str()), ("codex:tests", "python3 -m pytest tests/"));
    assert!(scan.sessions.is_empty());
    // The provider's text envelope is used by installed Codex too.
    let path = dir.write(
        "text.jsonl",
        lines(&[
            call("build", "exec_command", json!({"cmd":"cargo build"})),
            result("build", Value::String("Wall time: 1s\nProcess running with session ID 55\nOutput:\n".into())),
            call("wait", "write_stdin", json!({"session_id":55})),
            result("wait", Value::String(format!("Process exited with code 137\nOutput:\n{KILL}"))),
        ]),
    );
    assert_eq!(scan.read(&path, Tool::Codex, 3, &[])[0].command, "cargo build");
}

#[test]
fn ambiguous_or_human_authored_memory_text_never_offers_a_rerun() {
    let dir = TempDir::new("provider-memory-negative");
    let cases = [
        ("cargo test", json!({"output":"Killed\nSIGKILL\nexit 137","exit_code":137})),
        ("cargo test", json!({"output":"fatal error: out of memory","exit_code":1})),
        ("cargo test", json!({"output":KILL,"exit_code":0})),
        ("git status", json!({"output":KILL,"exit_code":137})),
        ("echo 'cargo test'", json!({"output":KILL,"exit_code":137})),
    ];
    for (i, (command, output)) in cases.into_iter().enumerate() {
        let path = dir.write(
            &format!("case-{i}.jsonl"),
            lines(&[
                call("tool", "exec_command", json!({"cmd":command})),
                result("tool", output),
                json!({"type":"event_msg","payload":{"type":"user_message","message":KILL}}),
            ]),
        );
        assert!(Scanner::default().read(&path, Tool::Codex, 1, &[]).is_empty(), "case {i}");
    }
    let path = dir.write(
        "oversized.jsonl",
        lines(&[
            call("tool", "exec_command", json!({"cmd":format!("cargo test # {}","x".repeat(MAX_COMMAND))})),
            result("tool", json!({"output":KILL,"exit_code":137})),
        ]),
    );
    assert!(Scanner::default().read(&path, Tool::Codex, 1, &[]).is_empty());
}

#[test]
fn reused_sessions_and_input_do_not_inherit_a_stale_build() {
    let dir = TempDir::new("provider-memory-stale");
    for (i, replacement) in [
        vec![
            call("other", "exec_command", json!({"cmd":"git status"})),
            result("other", json!({"output":"","session_id":7})),
        ],
        vec![
            call("input", "write_stdin", json!({"session_id":7,"chars":"echo unrelated\n"})),
            result("input", json!({"output":"","session_id":7})),
        ],
    ]
    .into_iter()
    .enumerate()
    {
        let mut records = vec![
            call("old", "exec_command", json!({"cmd":"cargo test"})),
            result("old", json!({"output":"","session_id":7})),
        ];
        records.extend(replacement);
        records.extend([
            call("poll", "write_stdin", json!({"session_id":7})),
            result("poll", json!({"output":KILL,"exit_code":137})),
        ]);
        let path = dir.write(&format!("stale-{i}.jsonl"), lines(&records));
        assert!(Scanner::default().read(&path, Tool::Codex, 1, &[]).is_empty());
    }
}

fn bash(id: &str) -> Value {
    json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":id,"name":"Bash",
        "input":{"command":"cargo test --locked","description":"Run tests","run_in_background":true}}]}})
}

fn notice(id: &str) -> String {
    format!("<task-notification>\n<task-id>job-1</task-id>\n<tool-use-id>{id}</tool-use-id>\n<status>killed</status>\n<summary>Background command \"Run tests\" was stopped because the system is running low on memory</summary>\n</task-notification>")
}

#[test]
fn claude_accepts_typed_notifications_and_results_but_not_user_quotes() {
    let dir = TempDir::new("provider-memory-claude");
    let path=dir.write("claude.jsonl",lines(&[
        bash("bash-1"),
        json!({"type":"user","message":{"content":notice("bash-1")}}),
        json!({"type":"user","message":{"content":[{"type":"text","text":notice("bash-1")} ]}}),
        json!({"type":"attachment","attachment":{"type":"queued_command","commandMode":"prompt","origin":{"kind":"human"},"prompt":notice("bash-1")}}),
    ]));
    let mut scan = Scanner::default();
    assert!(scan.read(&path, Tool::Claude, 1, &[]).is_empty());
    append(
        &path,
        &[json!({"type":"attachment","attachment":{"type":"queued_command","commandMode":"task-notification",
        "origin":{"kind":"task-notification"},"prompt":notice("missing-call")}})],
    );
    assert!(scan.read(&path, Tool::Claude, 2, &[]).is_empty());
    append(
        &path,
        &[json!({"type":"attachment","attachment":{"type":"queued_command","commandMode":"task-notification",
        "origin":{"kind":"task-notification"},"prompt":notice("bash-1")}})],
    );
    let kills = scan.read(&path, Tool::Claude, 3, &[]);
    assert_eq!((kills[0].id.as_str(), kills[0].command.as_str()), ("claude-task:job-1", "cargo test --locked"));
    assert!(!kills[0].authoritative, "only service repository metadata can grant hook authority");
    append(
        &path,
        &[
            bash("bash-2"),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"bash-2","is_error":true,"content":KILL}]}}),
        ],
    );
    assert_eq!(scan.read(&path, Tool::Claude, 4, &[]).len(), 2);
}

#[test]
fn incremental_scans_bound_history_and_ignore_partial_and_replaced_records() {
    let dir = TempDir::new("provider-memory-bounded");
    let path = dir.write("rollout.jsonl", lines(&[call("old", "exec_command", json!({"cmd":"cargo test"}))]));
    let mut scan = Scanner::default();
    scan.read(&path, Tool::Codex, 1, &[]);
    let raw = result("old", json!({"output":KILL,"exit_code":137})).to_string();
    fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(raw.as_bytes()).unwrap();
    assert!(scan.read(&path, Tool::Codex, 2, &[]).is_empty(), "partial JSON is not yet an event");
    fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"\n").unwrap();
    assert_eq!(scan.read(&path, Tool::Codex, 3, &[]).len(), 1);
    fs::write(&path, lines(&[result("old", json!({"output":KILL,"exit_code":137}))])).unwrap();
    assert!(scan.read(&path, Tool::Codex, 4, &[]).is_empty(), "truncation clears old command associations");
    let records: Vec<Value> = (0..MAX_KILLS + 10)
        .flat_map(|i| {
            [
                call(&format!("c-{i}"), "exec_command", json!({"cmd":"cargo test"})),
                result(&format!("c-{i}"), json!({"output":KILL,"exit_code":137})),
            ]
        })
        .collect();
    let path = dir.write("many.jsonl", lines(&records));
    assert_eq!(scan.read(&path, Tool::Codex, 5, &[]).len(), MAX_KILLS);
    let large = dir.write("large.jsonl", "x".repeat(INITIAL_TAIL as usize + 10) + "\n");
    append(&large, &[result("missing", json!({"output":KILL,"exit_code":137}))]);
    let mut scan = Scanner::default();
    for _ in 0..6 {
        assert!(scan.read(&large, Tool::Codex, 6, &[]).is_empty());
    }
    assert!(scan.record.len() <= MAX_RECORD);
    assert_eq!(scan.offset, fs::metadata(&large).unwrap().len());
}

#[test]
fn a_hundred_hosted_scanners_keep_their_own_command_associations() {
    let dir = TempDir::new("provider-memory-fleet");
    let mut scans: Vec<Scanner> = (0..100).map(|_| Scanner::default()).collect();
    for (i, scan) in scans.iter_mut().enumerate() {
        let command = format!("cargo test test_{i}");
        let path = dir.write(
            &format!("session-{i}.jsonl"),
            lines(&[
                call("same-call-id", "exec_command", json!({"cmd":command})),
                result("same-call-id", json!({"output":KILL,"exit_code":137})),
            ]),
        );
        assert_eq!(scan.read(&path, Tool::Codex, 1, &[])[0].command, command);
        let offset = scan.offset;
        scan.read(&path, Tool::Codex, 2, &[]);
        assert_eq!(scan.offset, offset);
    }
}

#[test]
fn reruns_decline_authorizing_or_unrecognized_compounds_without_reconstruction() {
    for command in [
        "cargo test --config 'alias.foo=!git push'",
        "make --eval='target:; git push'",
        "pytest && git push",
        "cargo test; rm -rf /srv/project/output",
        "cargo test && git merge topic",
        "cargo test && ./deploy.sh",
        "cargo test && gh workflow run release",
        "cargo test; git reset --hard",
        "cargo test > important.txt",
        "X=$(git push) cargo test",
        "cargo test && echo done",
        "make deploy",
        "python3 -c 'print(1)' -m pytest",
        "sh -c 'cargo test; git push'",
    ] {
        assert!(!safe_rerun(command, &[]), "{command}");
    }
    for command in [
        "cd /srv/project && cargo test --locked",
        "X=1 python3 -m pytest tests/",
        "env CI=1 timeout 60s sh -c 'cargo build --locked'",
        "npm run build",
        "uv run python3 -m unittest tests.test_module",
    ] {
        assert!(safe_rerun(command, &[]), "{command}");
    }
}

#[test]
fn reruns_read_through_only_the_configured_command_wrappers() {
    let wrapper =
        |program: &str, subcommand: &str| CommandWrapper { program: program.into(), subcommand: subcommand.into() };
    let task_environment = [wrapper("task_environment.py", "run")];
    let wrapped = "python3 agent/task_environment.py run --task fixture -- sh -c 'cd cepheus && cargo test'";
    // The generic default reads through no wrapper.
    assert!(crate::config::Conventions::default().command_wrappers.is_empty());
    assert!(!safe_rerun(wrapped, &[]));
    assert!(safe_rerun(wrapped, &task_environment));
    // The transcript scan offers the wrapped run only with the wrapper.
    let dir = TempDir::new("provider-memory-wrapper");
    let path = dir.write(
        "rollout.jsonl",
        lines(&[
            call("wrapped", "exec_command", json!({"cmd":wrapped})),
            result("wrapped", json!({"output":KILL,"exit_code":137})),
        ]),
    );
    assert!(Scanner::default().read(&path, Tool::Codex, 1, &[]).is_empty());
    assert_eq!(Scanner::default().read(&path, Tool::Codex, 1, &task_environment)[0].command, wrapped);
    for command in [
        // Run directly, by path or from `uv run`, matched by file name.
        "./agent/task_environment.py run -- cargo test",
        "uv run agent/task_environment.py run --task x -- pytest",
        "task_environment.py run -- make check",
    ] {
        assert!(safe_rerun(command, &task_environment), "{command}");
        assert!(!safe_rerun(command, &[]), "{command}");
    }
    for command in [
        "python3 agent/task_environment.py exec -- cargo test",
        "python3 agent/task_environment.py run cargo test",
        "python3 agent/task_environment.py run -- git status",
        "python3 agent/task_environment.py run -- cargo test && git status",
        "python3 agent/other_environment.py run -- cargo test",
    ] {
        assert!(!safe_rerun(command, &task_environment), "{command}");
    }
    // A program given with its folder matches only that word.
    let pathed = [wrapper("tools/sandbox", "run")];
    assert!(safe_rerun("tools/sandbox run -- cargo test", &pathed));
    assert!(!safe_rerun("other/sandbox run -- cargo test", &pathed));
    // Without a subcommand, the program alone and its `--`.
    let bare = [wrapper("isolate", "")];
    assert!(safe_rerun("isolate --memory 4G -- cargo build", &bare));
    assert!(!safe_rerun("isolate --memory 4G cargo build", &bare));
    assert!(!safe_rerun("x run -- cargo test", &[wrapper("", "run")]), "an empty program names nothing");
}

fn monitor_for(dir: &TempDir) -> super::super::Monitor {
    use super::super::{Monitor, MonitorConfig, Slow};
    use std::sync::{Arc, Mutex};
    let cfg = MonitorConfig {
        quotas: None,
        desktop: false,
        conventions: crate::config::Conventions::default(),
        claude_dir: dir.path().join("claude"),
        codex_dir: dir.path().join("codex"),
        antigravity_dir: dir.path().join("agy"),
        grok_home: dir.path().join("grok"),
        opencode_db: dir.path().join("opencode.db"),
        start_dir: PathBuf::from("/srv/project"),
        usage_file: dir.path().join("usage.json"),
        service_pid: 1,
        codex_notify: false,
    };
    Monitor::new(cfg, Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Slow::default())))
}

fn hosted_account(id: u64, tool: Tool, root: Option<PathBuf>) -> super::super::HostedRef {
    super::super::HostedRef {
        id,
        tool,
        pid: None,
        conversation: Some("same-thread".into()),
        spawned_ms: 0,
        alive: true,
        left: Vec::new(),
        footer: Vec::new(),
        cwd: None,
        account_root: root,
    }
}

#[test]
fn monitor_accounts_isolate_transcripts_names_goals_and_rollout_cache() {
    use super::super::{AccountCodex, Slow};
    use std::collections::{HashMap, HashSet};
    let dir = TempDir::new("monitor-memory-account");
    let mut monitor = monitor_for(&dir);
    for (root, prompt) in [("codex", "Default account prompt"), ("account-b", "Selected account prompt")] {
        dir.write(&format!("{root}/sessions/2026/11/05/rollout-x-same-thread.jsonl"),lines(&[
            json!({"type":"session_meta","payload":{"id":"same-thread","cwd":"/srv/project","timestamp":"2026-11-05T12:00:00Z"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":prompt}}),
        ]));
        dir.write(&format!("{root}/session_index.jsonl"), lines(&[json!({"id":"same-thread","thread_name":prompt})]));
    }
    monitor.refresh_names();
    monitor.active_goals = Some(HashSet::from(["same-thread".into()]));
    let root = dir.path().join("account-b");
    monitor.accounts.insert(
        root.clone(),
        AccountCodex { names: codex::thread_names(&root), goals: Some(HashSet::new()), ..AccountCodex::default() },
    );
    let default = hosted_account(1, Tool::Codex, None);
    let selected = hosted_account(2, Tool::Codex, Some(root));
    let facts = |m: &mut super::super::Monitor, h: &super::super::HostedRef| {
        m.session_facts(h, &HashMap::new(), &[], &Slow::default())
    };
    let a = facts(&mut monitor, &default);
    let b = facts(&mut monitor, &selected);
    assert_eq!(a.first_prompt.as_deref(), Some("Default account prompt"));
    assert_eq!(b.first_prompt.as_deref(), Some("Selected account prompt"));
    assert_eq!(b.meta_title.as_deref(), Some("Selected account prompt"));
    assert_eq!((a.goal_active, b.goal_active), (Some(true), Some(false)));
    let missing = hosted_account(3, Tool::Codex, Some(dir.path().join("missing-account")));
    let missing = facts(&mut monitor, &missing);
    assert_eq!((missing.first_prompt, missing.meta_title, missing.goal_active), (None, None, None));
    let slug = crate::claude::project_slug("/srv/project");
    for (root, prompt) in [("claude", "Default Claude prompt"), ("claude-b", "Selected Claude prompt")] {
        dir.write(
            &format!("{root}/projects/{slug}/same-thread.jsonl"),
            lines(&[json!({"type":"user","message":{"content":prompt}})]),
        );
    }
    let selected = hosted_account(4, Tool::Claude, Some(dir.path().join("claude-b")));
    assert_eq!(facts(&mut monitor, &selected).first_prompt.as_deref(), Some("Selected Claude prompt"));
}

#[test]
fn typed_codex_items_require_their_own_shell_command_and_failed_result() {
    let dir = TempDir::new("provider-memory-items");
    let item = |id: &str, command: &str, output: &str, code: i64| {
        json!({"type":"event_msg","payload":{"type":"item_completed",
        "item":{"type":"CommandExecution","id":id,"command":["bash","-lc",command],"aggregated_output":output,"exit_code":code}}})
    };
    let path = dir.write(
        "items.jsonl",
        lines(&[
            item("killed", "cargo test", KILL, 137),
            item("signal", "cargo test", "SIGKILL", 137),
            item("read", "git status", KILL, 137),
            item("success", "cargo test", KILL, 0),
        ]),
    );
    let kills = Scanner::default().read(&path, Tool::Codex, 1, &[]);
    assert_eq!(kills.len(), 1);
    assert_eq!((kills[0].id.as_str(), kills[0].command.as_str()), ("codex:killed", "cargo test"));
}

#[test]
fn command_output_cannot_replace_process_session_metadata() {
    let dir = TempDir::new("provider-memory-output-header");
    let path = dir.write(
        "rollout.jsonl",
        lines(&[
            call("tests", "exec_command", json!({"cmd":"cargo test"})),
            result(
                "tests",
                Value::String("Process running with session ID 8\nOutput:\nProcess running with session ID 9".into()),
            ),
        ]),
    );
    let mut scan = Scanner::default();
    assert!(scan.read(&path, Tool::Codex, 1, &[]).is_empty());
    append(
        &path,
        &[
            call("wrong", "write_stdin", json!({"session_id":9})),
            result("wrong", json!({"output":KILL,"exit_code":137})),
        ],
    );
    assert!(scan.read(&path, Tool::Codex, 2, &[]).is_empty(), "stdout cannot forge a tool process-session id");
    append(
        &path,
        &[
            call("right", "write_stdin", json!({"session_id":8})),
            result("right", json!({"output":KILL,"exit_code":137})),
        ],
    );
    let kills = scan.read(&path, Tool::Codex, 3, &[]);
    assert_eq!(kills.len(), 1);
    assert_eq!(kills[0].id, "codex:tests");
    let forged = dir.write("forged.jsonl", lines(&[
        call("unwrapped", "exec_command", json!({"cmd":"cargo test"})),
        result("unwrapped", Value::String("Process running with session ID 99".into())),
        call("envelope", "exec_command", json!({"cmd":"cargo test"})),
        result("envelope", Value::String("Process running with session ID 8\nFinal output:\nProcess running with session ID 99\nOutput:\nordinary stdout".into())),
        call("forged", "write_stdin", json!({"session_id":99})),
        result("forged", json!({"output":KILL,"exit_code":137})),
    ]));
    assert!(
        Scanner::default().read(&forged, Tool::Codex, 4, &[]).is_empty(),
        "neither unwrapped stdout nor a later stdout delimiter can forge process metadata"
    );
}

#[test]
fn account_discovery_does_not_claim_the_same_id_in_another_root() {
    use std::collections::HashMap;
    let dir = TempDir::new("monitor-memory-discovery");
    let mut monitor = monitor_for(&dir);
    let mut a = hosted_account(1, Tool::Codex, None);
    let mut b = hosted_account(2, Tool::Codex, Some(dir.path().join("account-b")));
    a.conversation = None;
    b.conversation = None;
    let created = codex::parse_rfc3339_ms("2026-11-05T12:00:00Z").unwrap();
    a.spawned_ms = created;
    b.spawned_ms = created;
    for (root, session) in [(dir.path().join("codex"), a), (dir.path().join("account-b"), b)] {
        let relative = root.strip_prefix(dir.path()).unwrap().to_string_lossy();
        let path=dir.write(&format!("{relative}/sessions/2026/11/05/rollout-x-same-thread.jsonl"),lines(&[
            json!({"type":"session_meta","payload":{"id":"same-thread","cwd":"/srv/project","timestamp":"2026-11-05T12:00:00Z"}}),
        ]));
        monitor.discover_codex_in(
            &root,
            std::slice::from_ref(&session),
            &[(path, created)],
            &HashMap::new(),
            created + 60_000,
        );
        assert_eq!(monitor.discovered.get(&session.id).map(String::as_str), Some("same-thread"));
    }
}

#[test]
fn a_rewritten_transcript_cannot_use_a_pre_rewrite_command() {
    let dir = TempDir::new("provider-memory-rewritten");
    let path = dir.write("rollout.jsonl", lines(&[call("old", "exec_command", json!({"cmd":"cargo test"}))]));
    let mut scan = Scanner::default();
    scan.read(&path, Tool::Codex, 1, &[]);
    let old_length = fs::metadata(&path).unwrap().len() as usize;
    fs::write(
        &path,
        lines(&[
            json!({"type":"event_msg","payload":{"type":"user_message","message":"x".repeat(old_length+100)}}),
            result("old", json!({"output":KILL,"exit_code":137})),
        ]),
    )
    .unwrap();
    assert!(scan.read(&path, Tool::Codex, 2, &[]).is_empty());
}

#[test]
fn an_old_pending_poll_cannot_erase_a_new_process_session_binding() {
    let dir = TempDir::new("memory-stale-poll");
    let path = dir.write(
        "stale.jsonl",
        lines(&[
            call("old", "exec_command", json!({"cmd":"cargo test"})),
            result("old", json!({"session_id":7,"output":"building"})),
            call("poll-old", "write_stdin", json!({"session_id":7})),
            call("new", "exec_command", json!({"cmd":"cargo build"})),
            result("new", json!({"session_id":7,"output":"building"})),
            result("poll-old", json!({"output":KILL,"exit_code":137})),
        ]),
    );
    let mut scanner = Scanner::default();
    assert!(scanner.read(&path, Tool::Codex, 1, &[]).is_empty());
    append(
        &path,
        &[
            call("poll-new", "write_stdin", json!({"session_id":7})),
            result("poll-new", json!({"output":KILL,"exit_code":137})),
        ],
    );
    let kills = scanner.read(&path, Tool::Codex, 2, &[]);
    assert_eq!(kills.len(), 1);
    assert_eq!(kills[0].id, "codex:new");
}

#[test]
fn unread_records_and_malformed_replacements_clear_pending_correlations() {
    let dir = TempDir::new("memory-unread-record");
    for (index,replacement) in ["broken json\n".to_string(),format!("{}\n","x".repeat(600000)),lines(&[json!({"type":"response_item","payload":{"type":"function_call","call_id":"old","name":"exec_command","arguments":"malformed"}})])].into_iter().enumerate() {
        let path=dir.write(&format!("{index}.jsonl"),lines(&[call("old","exec_command",json!({"cmd":"cargo test"}))]));
        let mut scanner=Scanner::default();
        assert!(scanner.read(&path,Tool::Codex,1,&[]).is_empty());
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(replacement.as_bytes()).unwrap();
        append(&path,&[result("old",json!({"output":KILL,"exit_code":137}))]);
        for _ in 0..3 {assert!(scanner.read(&path,Tool::Codex,2,&[]).is_empty(),"case {index}");}
    }
}
