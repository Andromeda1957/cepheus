use super::*;
use crate::testsupport::TempDir;
use serde_json::json;
use std::io::Write;

/// A project whose linked worktrees are `<repo>-agent-<task>`.
fn agent_worktrees() -> crate::config::Conventions {
    crate::config::Conventions { worktree_task_marker: "-agent-".into(), ..Default::default() }
}

fn monitor(dir: &TempDir) -> Monitor {
    monitor_with(dir, agent_worktrees())
}

fn monitor_with(dir: &TempDir, conventions: crate::config::Conventions) -> Monitor {
    let cfg = MonitorConfig {
        quotas: None,
        desktop: true,
        conventions,
        claude_dir: dir.path().join("claude"),
        codex_dir: dir.path().join("codex"),
        antigravity_dir: dir.path().join("agy"),
        grok_home: dir.path().join("grok"),
        opencode_db: dir.path().join("opencode/opencode.db"),
        start_dir: PathBuf::from("/srv/repo"),
        usage_file: dir.path().join("state/claude-usage.json"),
        service_pid: 1,
        codex_notify: false,
    };
    Monitor::new(cfg, Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Slow::default())))
}

fn hosted(id: u64, tool: Tool, spawned_ms: u64) -> HostedRef {
    HostedRef {
        id,
        tool,
        pid: None,
        conversation: None,
        spawned_ms,
        alive: true,
        left: Vec::new(),
        footer: Vec::new(),
        cwd: None,
        account_root: None,
    }
}

fn rollout(dir: &TempDir, id: &str, cwd: &str, created: &str, extra: &[serde_json::Value]) -> PathBuf {
    let mut text =
        json!({"type": "session_meta", "payload": {"id": id, "cwd": cwd, "timestamp": created}}).to_string() + "\n";
    for line in extra {
        text.push_str(&(line.to_string() + "\n"));
    }
    dir.write(&format!("codex/sessions/2026/10/25/rollout-x-{id}.jsonl"), text)
}

#[test]
fn resume_ids_are_thread_uuids() {
    let args = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
    assert_eq!(
        resume_id(&args("codex --yolo resume 019a1b2c-3d4e-5f60-7182-93a4b5c6d7e8")).as_deref(),
        Some("019a1b2c-3d4e-5f60-7182-93a4b5c6d7e8")
    );
    assert_eq!(resume_id(&args("codex --yolo resume")), None);
    assert_eq!(resume_id(&args("codex resume --last")), None);
    assert_eq!(resume_id(&args("codex")), None);
}

#[test]
fn claude_windows_from_snapshot() {
    let now = 1_800_000_000;
    let v = json!({"updated_at": now - 10, "rate_limits": {
        "five_hour": {"used_percentage": 20.5, "resets_at": now + 600},
        "seven_day": {"used_percentage": 70, "resets_at": now - 1}
    }});
    let w = claude_windows(&v, now);
    assert_eq!(w.len(), 1, "an expired window is dropped");
    assert_eq!(
        (w[0].label.as_str(), w[0].used_percent.unwrap(), w[0].resets_at, w[0].updated_at),
        ("5h", 20.5, Some(now + 600), now - 10)
    );
    assert!(claude_windows(&json!({}), now).is_empty());
}

#[test]
fn claude_usage_follows_the_snapshot_file() {
    let dir = TempDir::new("monitor-claude-usage");
    let mut m = monitor(&dir);
    assert!(m.claude_usage().is_empty(), "no snapshot, no reading");
    let now = util::now_secs();
    let snapshot = |used: f64, reset: u64| {
        json!({"updated_at": now, "rate_limits": {"five_hour": {"used_percentage": used, "resets_at": reset}}})
            .to_string()
    };
    let path = dir.write("state/claude-usage.json", snapshot(20.0, now + 600));
    assert_eq!(m.claude_usage()[0].used_percent, Some(20.0));
    util::write_atomic(&path, snapshot(30.0, now + 600).as_bytes()).unwrap();
    assert_eq!(m.claude_usage()[0].used_percent, Some(30.0), "a rewritten snapshot is read again");
    util::write_atomic(&path, snapshot(40.0, now - 1).as_bytes()).unwrap();
    assert!(m.claude_usage().is_empty(), "a window whose reset passed is dropped");
    std::fs::remove_file(&path).unwrap();
    assert!(m.claude_usage().is_empty());
}

#[test]
fn shared_codex_sessions_are_matched_only_when_unambiguous() {
    let dir = TempDir::new("monitor-discover");
    let at = |secs: u64| {
        let t = codex::parse_rfc3339_ms("2026-10-25T10:00:00Z").unwrap() + secs * 1000;
        (t, format!("2026-10-25T10:{:02}:{:02}Z", secs / 60, secs % 60))
    };
    let user_thread = |id: &str, secs: u64| rollout(&dir, id, "/srv/repo", &at(secs).1, &[]);
    rollout(&dir, "old", "/srv/repo", "2026-10-25T09:00:00Z", &[]);
    // A sub-agent another session spawned in the same directory is no one's conversation.
    let spawned = json!({"type": "session_meta", "payload": {"id": "spawned", "cwd": "/srv/repo", "timestamp": at(1).1, "thread_source": "subagent"}});
    dir.write("codex/sessions/2026/10/25/rollout-x-spawned.jsonl", spawned.to_string() + "\n");
    rollout(&dir, "other-dir", "/elsewhere", &at(2).1, &[]);
    user_thread("lone", 5);
    // Two sessions started three seconds apart, each creating its thread.
    user_thread("b-thread", 105);
    user_thread("c-thread", 108);
    // Another Codex starts a thread in the same checkout just after this one.
    user_thread("d-thread", 305);
    user_thread("outside", 306);
    // One thread that either of two sessions could have created.
    user_thread("f-or-g", 510);

    let mut resumed = hosted(9, Tool::Codex, at(0).0);
    resumed.conversation = Some("old".into());
    let refs = vec![
        hosted(1, Tool::Codex, at(0).0),
        hosted(2, Tool::Codex, at(100).0),
        hosted(3, Tool::Codex, at(103).0),
        hosted(4, Tool::Codex, at(300).0),
        hosted(6, Tool::Codex, at(500).0),
        hosted(7, Tool::Codex, at(503).0),
        resumed,
    ];
    let recent = codex::rollouts_in(&codex::day_dirs(&dir.path().join("codex"), 2));

    // Until the match window closes another candidate may still appear.
    let mut m = monitor(&dir);
    m.discover_codex(&refs[..1], &recent, &HashMap::new(), at(10).0);
    assert!(m.discovered.is_empty());

    let mut m = monitor(&dir);
    m.discover_codex(&refs, &recent, &HashMap::new(), at(600).0);
    assert_eq!(m.discovered.get(&1).map(String::as_str), Some("lone"));
    for id in [2, 3, 4, 6, 7, 9] {
        assert!(!m.discovered.contains_key(&id), "session {id} is not guessed: {:?}", m.discovered.get(&id));
    }
}

#[test]
fn embedded_codex_sessions_are_matched_by_the_rollout_they_hold_open() {
    let dir = TempDir::new("monitor-embedded");
    let mut m = monitor(&dir);
    m.cfg.codex_notify = true;
    let created = "2026-10-25T10:00:01Z";
    let held = rollout(&dir, "held", "/srv/repo", created, &[]);
    // A rollout created at the same moment that the session does not hold.
    rollout(&dir, "same-time", "/srv/repo", created, &[]);
    // One it only reads, as its resume picker does (review
    // CR-20261026-d5c080aeb2).
    let read = rollout(&dir, "read", "/srv/repo", created, &[]);
    let helper = json!({"type": "session_meta", "payload": {"id": "helper", "cwd": "/srv/repo", "timestamp": created, "thread_source": "subagent"}});
    let helper = dir.write("codex/sessions/2026/10/25/rollout-x-helper.jsonl", helper.to_string() + "\n");
    // This test process stands in for the hosted Codex, holding its own
    // thread's rollout and a sub-agent's open for appending, as Codex does.
    let append = |p: &PathBuf| fs::OpenOptions::new().append(true).open(p).unwrap();
    let _files = (append(&held), append(&helper), fs::File::open(&read).unwrap());
    let spawned = codex::parse_rfc3339_ms(created).unwrap();
    let mut session = hosted(1, Tool::Codex, spawned);
    session.pid = Some(std::process::id());
    let no_pid = hosted(2, Tool::Codex, spawned);
    let recent = codex::rollouts_in(&codex::day_dirs(&dir.path().join("codex"), 2));
    m.discover_codex(&[session, no_pid], &recent, &HashMap::new(), spawned + 60_000);
    assert_eq!(m.discovered.get(&1).map(String::as_str), Some("held"));
    assert!(!m.discovered.contains_key(&2), "an embedded session is never matched by time");
}

#[test]
fn a_conversation_the_session_left_is_never_found_for_it_again() {
    // After `/clear` or `/new` Codex keeps the old rollout open beside the
    // new one (owner, 2026-10-26: "/ clear doesnt reset the title and
    // worktree given"). The service has dropped the old conversation; the
    // monitor must not hand it back, though it found it itself before.
    let dir = TempDir::new("monitor-left");
    let old = rollout(&dir, "old", "/srv/repo", "2026-10-25T10:00:00Z", &[]);
    let new = rollout(&dir, "new", "/srv/repo", "2026-10-25T10:30:00Z", &[]);
    // This test process stands in for the hosted Codex holding both.
    let _open = [&old, &new].map(|p| fs::OpenOptions::new().append(true).open(p).unwrap());
    let mut session = hosted(1, Tool::Codex, 0);
    session.pid = Some(std::process::id());
    session.left = vec!["old".into()];
    let conversation = |m: &mut Monitor| {
        *m.hosted.lock().unwrap() = vec![session.clone()];
        m.cycle().facts.remove(&1).unwrap().conversation
    };

    // Embedded: of the two rollouts it holds, the one it has not left.
    let mut m = monitor(&dir);
    m.cfg.codex_notify = true;
    assert_eq!(conversation(&mut m).as_deref(), Some("new"));

    // Attached, its new thread not found yet: no conversation, not the old one.
    let mut m = monitor(&dir);
    m.discovered.insert(1, "old".into());
    assert_eq!(conversation(&mut m), None);
}

fn two_worktrees() -> Vec<WorktreeEntry> {
    vec![
        WorktreeEntry { path: "/r/repo-agent-alpha".into(), branch: Some("agent/alpha".into()) },
        WorktreeEntry { path: "/r/repo-agent-beta".into(), branch: None },
    ]
}

fn ran(command: &str) -> String {
    json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "input": {"command": command}}]}})
        .to_string()
        + "\n"
}

fn jsonl(lines: &[serde_json::Value]) -> String {
    lines.iter().map(|l| l.to_string() + "\n").collect()
}

fn task(wt: Option<Worktree>) -> Option<String> {
    wt.and_then(|w| w.task)
}

#[test]
fn worktree_scan_is_incremental_and_follows_last_mention() {
    let dir = TempDir::new("monitor-scan");
    let mut m = monitor(&dir);
    m.worktrees = two_worktrees();
    let path = dir.write("t.jsonl", ran("cd /r/repo-agent-alpha && git status"));
    let wt = m.scan_worktree("s1".into(), path.clone(), Tool::Claude).unwrap();
    assert_eq!((wt.task.as_deref(), wt.branch.as_deref()), (Some("alpha"), Some("agent/alpha")));
    let append =
        |text: &str| fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(text.as_bytes()).unwrap();
    append(&ran("echo nothing relevant"));
    assert_eq!(task(m.scan_worktree("s1".into(), path.clone(), Tool::Claude)).as_deref(), Some("alpha"));
    // A line caught half written is judged once it is complete.
    let line = ran("cd /r/repo-agent-beta && cargo test");
    let (head, tail) = line.split_at(line.find("beta").unwrap() + 4);
    append(head);
    assert_eq!(task(m.scan_worktree("s1".into(), path.clone(), Tool::Claude)).as_deref(), Some("alpha"));
    append(tail);
    assert_eq!(task(m.scan_worktree("s1".into(), path.clone(), Tool::Claude)).as_deref(), Some("beta"));
    assert_eq!(m.scans["s1"].offset, fs::metadata(&path).unwrap().len());
    // Another transcript for the same key starts over.
    let other = dir.write("u.jsonl", "plain\n");
    assert_eq!(m.scan_worktree("s1".into(), other, Tool::Claude), None);
}

#[test]
fn a_worktree_task_follows_the_configured_marker() {
    let dir = TempDir::new("monitor-scan-marker");
    let path = dir.write("t.jsonl", ran("cd /r/repo-agent-alpha && git status"));
    // The generic default: the whole folder name, whatever plugins are on.
    let mut m = monitor_with(&dir, crate::config::Conventions::default());
    m.worktrees = two_worktrees();
    let wt = m.scan_worktree("s1".into(), path.clone(), Tool::Claude).unwrap();
    assert_eq!(wt.task.as_deref(), Some("repo-agent-alpha"));
    // A folder without the configured marker has no task.
    let marker = crate::config::Conventions { worktree_task_marker: "-task-".into(), ..Default::default() };
    let mut m = monitor_with(&dir, marker);
    m.worktrees = two_worktrees();
    let wt = m.scan_worktree("s1".into(), path, Tool::Claude).unwrap();
    assert_eq!((wt.path.as_str(), wt.task), ("/r/repo-agent-alpha", None));
}

#[test]
fn a_session_is_in_review_while_a_descendant_runs_a_configured_review_command() {
    use crate::extensions::conventions::review;
    use std::process::{Command, Stdio};
    // `read` keeps the child waiting without a process of its own; its last
    // argument is a review tool's path.
    let mut child = Command::new("sh")
        .args(["-c", "read line", "review", "/opt/tools/code-review"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    // Its arguments are readable once it has started `sh`, not just forked.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !crate::procs::cmdline(child.id()).iter().any(|arg| arg == "/opt/tools/code-review") {
        assert!(std::time::Instant::now() < deadline, "the child never started");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let session = u32::MAX - 7;
    let procs = HashMap::from([(
        child.id(),
        ProcInfo { pid: child.id(), ppid: session, comm: "sh".into(), state: 'S', start_ticks: 0 },
    )]);
    let defaults = crate::config::Conventions::default().review_commands;
    assert!(review(session, &procs, &defaults), "the generic default names code-review");
    assert!(review(session, &procs, &["tools/code-review".to_string()]), "an argument's ending");
    assert!(!review(session, &procs, &["code_review.py".to_string()]), "another project's tool");
    assert!(!review(session, &procs, &[]));
    assert!(!review(session, &procs, &[String::new()]), "an empty entry names nothing");
    assert!(!review(child.id(), &procs, &defaults), "only descendants count");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn worktree_scan_ignores_injected_context() {
    let dir = TempDir::new("monitor-scan-context");
    let mut m = monitor(&dir);
    m.worktrees = two_worktrees();
    // A SessionStart hook's task summary names every worktree; it is not work.
    let summary = "Active tasks: /r/repo-agent-alpha /r/repo-agent-beta";
    let claude = dir.write(
        "claude.jsonl",
        jsonl(&[
            json!({"type": "user", "message": {"role": "user", "content": "work in /r/repo-agent-alpha"}}),
            json!({"type": "attachment", "attachment": {"type": "hook_success", "hookEvent": "SessionStart", "content": summary}}),
            json!({"type": "user", "isMeta": true, "message": {"content": "cwd /r/repo-agent-beta"}}),
            json!({"type": "api-request-blob", "body": "/r/repo-agent-beta"}),
        ]),
    );
    assert_eq!(task(m.scan_worktree("c".into(), claude, Tool::Claude)).as_deref(), Some("alpha"));
    let codex = dir.write(
        "codex.jsonl",
        jsonl(&[
            json!({"type": "response_item", "payload": {"type": "custom_tool_call", "input": "cd /r/repo-agent-alpha"}}),
            json!({"type": "world_state", "payload": {"state": {"agents_md": {"text": "see /r/repo-agent-beta"}}}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": summary}]}}),
            json!({"type": "turn_context", "payload": {"cwd": "/r/repo-agent-beta"}}),
        ]),
    );
    assert_eq!(task(m.scan_worktree("x".into(), codex, Tool::Codex)).as_deref(), Some("alpha"));
    let fresh = dir.write(
        "fresh.jsonl",
        jsonl(&[
            json!({"type": "attachment", "attachment": {"type": "hook_success", "hookEvent": "SessionStart", "content": summary}}),
            json!({"type": "user", "message": {"role": "user", "content": "Reply with only the word OK."}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "OK"}]}}),
        ]),
    );
    assert_eq!(
        m.scan_worktree("f".into(), fresh, Tool::Claude),
        None,
        "a session that never worked in a tree claims none"
    );
}

#[test]
fn worktree_scan_ignores_tool_output() {
    // Owner screenshot, 2026-10-31: a Claude session given a new task was
    // labelled with the last worktree of the startup task summary, which it
    // had read back from the file Claude Code saves long hook output to.
    // Codex lists every worktree the same way when it runs
    // `task_state.py list`. What a tool printed is not where the session works.
    let dir = TempDir::new("monitor-scan-output");
    let mut m = monitor(&dir);
    m.worktrees = two_worktrees();
    let listing = "alpha: worktree=/r/repo-agent-alpha\nbeta: worktree=/r/repo-agent-beta\n";
    let append = |path: &PathBuf, lines: &[serde_json::Value]| {
        fs::OpenOptions::new().append(true).open(path).unwrap().write_all(jsonl(lines).as_bytes()).unwrap()
    };

    let read_back = json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t1", "content": listing}
    ]}, "toolUseResult": {"type": "text", "file": {"content": listing}}});
    let claude = dir.write(
        "claude.jsonl",
        jsonl(&[
            json!({"type": "user", "message": {"role": "user", "content": "add a resume key to the deck"}}),
            json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "Read", "input": {"file_path": "/home/u/.claude/tool-results/hook-stdout.txt"}}
            ]}}),
            read_back.clone(),
        ]),
    );
    assert_eq!(m.scan_worktree("c".into(), claude.clone(), Tool::Claude), None, "a listing it read claims no tree");
    let started = json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "name": "Bash", "input": {"command": "cd /r/repo-agent-alpha && git status"}}
    ]}});
    append(&claude, &[started, read_back]);
    assert_eq!(
        task(m.scan_worktree("c".into(), claude, Tool::Claude)).as_deref(),
        Some("alpha"),
        "the tree it works in, not the last one a later listing names"
    );

    let command = |cwd: &str, cmd: &str| {
        json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {
            "type": "CommandExecution", "command": ["/usr/bin/zsh", "-lc", cmd],
            "cwd": format!("file://{cwd}"), "aggregated_output": listing
        }}})
    };
    let codex = rollout(
        &dir,
        "listing",
        "/srv/repo",
        "2026-10-25T10:00:00Z",
        &[
            command("/srv/repo", "python3 tools/work_status.py list"),
            json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "c1", "output": listing}}),
            json!({"type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": "c2",
                "output": [{"type": "input_text", "text": listing}]}}),
            // Older Codex reported a finished command this way.
            json!({"type": "event_msg", "payload": {"type": "exec_command_end", "command": ["ls"], "cwd": "/srv/repo",
                "stdout": listing, "stderr": listing, "aggregated_output": listing, "formatted_output": listing}}),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "McpToolCall",
                "result": {"content": [{"type": "text", "text": listing}]}}}}),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "FileChange",
                "stdout": listing}}}),
            json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "Extension",
                "query": "worktrees", "results": [{"text": listing}]}}}),
        ],
    );
    assert_eq!(m.scan_worktree("x".into(), codex.clone(), Tool::Codex), None, "a listing it ran claims no tree");
    append(&codex, &[command("/r/repo-agent-alpha", "git status"), command("/srv/repo", "git worktree list")]);
    assert_eq!(task(m.scan_worktree("x".into(), codex, Tool::Codex)).as_deref(), Some("alpha"));
}

#[test]
fn claude_session_facts_combine_records() {
    let dir = TempDir::new("monitor-claude");
    let mut m = monitor(&dir);
    m.worktrees = vec![WorktreeEntry { path: "/r/repo-agent-gamma".into(), branch: None }];
    let conv = "11111111-2222-3333-4444-555555555555";
    dir.write(
        &format!("claude/projects/{}/{conv}.jsonl", claude::project_slug("/srv/repo")),
        [
            json!({"type": "user", "message": {"content": "add the deck"}}).to_string(),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "input": {"command": "cd /r/repo-agent-gamma"}}]}}).to_string(),
        ]
        .join("\n")
            + "\n",
    );
    let files = vec![ClaudeProc {
        pid: 4242,
        session_id: Some(conv.into()),
        cwd: Some("/srv/repo".into()),
        name: Some("deck work".into()),
        busy: Some(true),
        interactive: true,
        proc_start: None,
    }];
    let mut slow = Slow::default();
    slow.containers.insert(
        "gamma".into(),
        vec![ContainerInfo { name: "c1".into(), state: "running".into(), status: "Up".into() }],
    );
    let mut h = hosted(5, Tool::Claude, 0);
    h.pid = Some(4242);
    let f = m.session_facts(&h, &HashMap::new(), &files, &slow);
    assert_eq!(f.conversation.as_deref(), Some(conv));
    assert_eq!(f.meta_title.as_deref(), Some("deck work"));
    assert_eq!(f.first_prompt.as_deref(), Some("add the deck"));
    assert_eq!(f.busy, Some(true));
    assert_eq!(f.worktree.as_ref().and_then(|w| w.task.as_deref()), Some("gamma"));
    assert_eq!(f.containers.len(), 1);
    assert!(!f.review);
}

#[test]
fn a_claude_session_takes_its_ai_title_and_agent_from_its_transcript() {
    // Owner, 2026-10-26: "the claude sessions dont get a proper title". With
    // Claude's folder-derived names skipped, every hosted Claude showed its
    // terminal title, which is the agent's name ("implementer").
    let dir = TempDir::new("monitor-claude-title");
    let mut m = monitor(&dir);
    let conv = "22222222-3333-4444-5555-666666666666";
    dir.write(
        &format!("claude/projects/{}/{conv}.jsonl", claude::project_slug("/srv/repo")),
        jsonl(&[
            json!({"type": "agent-setting", "agentSetting": "implementer", "sessionId": conv}),
            json!({"type": "user", "message": {"content": "review the video verifier"}}),
            json!({"type": "ai-title", "aiTitle": "Video verifier review", "sessionId": conv}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Looking."}]}}),
            json!({"type": "ai-title", "aiTitle": "Security review of video verifier cleanup", "sessionId": conv}),
        ]),
    );
    let files = vec![ClaudeProc {
        pid: 4243,
        session_id: Some(conv.into()),
        cwd: Some("/srv/repo".into()),
        name: None,
        busy: None,
        interactive: true,
        proc_start: None,
    }];
    let mut h = hosted(5, Tool::Claude, 0);
    h.pid = Some(4243);
    let f = m.session_facts(&h, &HashMap::new(), &files, &Slow::default());
    assert_eq!(f.ai_title.as_deref(), Some("Security review of video verifier cleanup"), "the latest title");
    assert_eq!(f.agent.as_deref(), Some("implementer"));
    assert_eq!(f.first_prompt.as_deref(), Some("review the video verifier"));
}

#[test]
fn a_grok_session_is_titled_by_grok_and_works_while_it_keeps_the_machine_awake() {
    use std::os::unix::process::CommandExt;
    let dir = TempDir::new("monitor-grok");
    let mut m = monitor(&dir);
    let id = "0b4f8a2e-1c3d-4e5f-8a9b-0c1d2e3f4a5b";
    dir.write(&format!("grok/sessions/%2Fsrv%2Frepo/{id}/summary.json"), r#"{"generated_title": "Port the parser"}"#);
    // This test process stands in for the hosted Grok.
    let me = std::process::id();
    let mut h = hosted(6, Tool::Grok, 0);
    h.conversation = Some(id.into());
    h.pid = Some(me);
    let proc = |pid: u32, ppid: u32, comm: &str| ProcInfo { pid, ppid, comm: comm.into(), state: 'S', start_ticks: 0 };
    let tree = |child: Option<u32>| -> HashMap<u32, ProcInfo> {
        std::iter::once(proc(me, 1, "grok"))
            .chain(child.map(|c| proc(c, me, "systemd-inhibit")))
            .map(|p| (p.pid, p))
            .collect()
    };
    let f = m.session_facts(&h, &tree(None), &[], &Slow::default());
    assert_eq!(f.ai_title.as_deref(), Some("Port the parser"));
    assert_eq!(f.conversation.as_deref(), Some(id));
    assert_eq!(f.busy, Some(false), "between turns Grok runs nothing under itself");
    // In a turn Grok runs `systemd-inhibit` under itself; a shell named so
    // stands in for it. Another program's inhibitor is not Grok's turn.
    let inhibit = |who: &str| {
        let args = ["-c", "sleep 30 & wait", "--what=idle", who, "--why=agent turn in progress", "sleep"];
        let child =
            std::process::Command::new("sh").arg0("systemd-inhibit").args(args).process_group(0).spawn().unwrap();
        // `spawn` returns once the exec has begun; the kernel sets the new
        // argv after that, and until then `/proc` shows none, half of it or
        // this test's (6 of 20 runs read it too soon, 2026-10-26).
        let deadline = Instant::now() + Duration::from_secs(10);
        while procs::cmdline(child.id()).last().map(String::as_str) != Some("sleep") {
            assert!(Instant::now() < deadline, "the stand-in never started");
            thread::sleep(Duration::from_millis(5));
        }
        child
    };
    let mut busy = Vec::new();
    for who in ["--who=grok", "--who=firefox"] {
        let mut child = inhibit(who);
        busy.push(m.session_facts(&h, &tree(Some(child.id())), &[], &Slow::default()).busy);
        // SAFETY: kill has no preconditions; the group is the child's own.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.wait();
    }
    assert_eq!(busy, [Some(true), Some(false)]);
}

#[test]
fn a_claude_title_follows_the_latest_task_the_owner_gave() {
    // Owner, 2026-10-26: "titles dont update on clear or when given a new
    // task". Claude writes no generated title for an interactive session,
    // and its first prompt names only the first task.
    let dir = TempDir::new("monitor-claude-task");
    let mut m = monitor(&dir);
    let conv = "33333333-3333-4444-5555-666666666666";
    let path = dir.write(
        &format!("claude/projects/{}/{conv}.jsonl", claude::project_slug("/srv/repo")),
        jsonl(&[
            json!({"type": "agent-setting", "agentSetting": "implementer", "sessionId": conv}),
            json!({"type": "user", "timestamp": "2026-10-26T14:50:55Z", "message": {"content": "ssd should show space used"}}),
            json!({"type": "user", "timestamp": "2026-10-26T14:52:00Z", "message": {"content": "continue"}}),
            // Typed while Claude worked: queued, with the image it carried.
            json!({"type": "attachment", "timestamp": "2026-10-26T15:21:47Z", "attachment": {
                "type": "queued_command", "commandMode": "prompt", "origin": {"kind": "human"},
                "prompt": [{"type": "text", "text": "[Image #19] and show the elapsed time only"}, {"type": "image"}]}}),
            json!({"type": "attachment", "timestamp": "2026-10-26T15:22:00Z", "attachment": {
                "type": "queued_command", "commandMode": "task-notification", "origin": {"kind": "task-notification"},
                "prompt": "<task-notification> the build finished with these words"}}),
            json!({"type": "user", "isCompactSummary": true, "timestamp": "2026-10-26T15:23:00Z",
                "message": {"content": "This session is being continued from a previous conversation"}}),
            json!({"type": "user", "timestamp": "2026-10-26T15:24:00Z", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "tool output with many words"}]}]}}),
        ]),
    );
    let files = vec![ClaudeProc {
        pid: 4244,
        session_id: Some(conv.into()),
        cwd: Some("/srv/repo".into()),
        name: None,
        busy: None,
        interactive: true,
        proc_start: None,
    }];
    let mut h = hosted(5, Tool::Claude, 0);
    h.pid = Some(4244);
    let f = m.session_facts(&h, &HashMap::new(), &files, &Slow::default());
    assert_eq!(f.latest_prompt.as_deref(), Some("show the elapsed time only"));
    assert_eq!(f.first_prompt.as_deref(), Some("ssd should show space used"));
    // A new task typed once Claude was idle.
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(
        file,
        "{}",
        json!({"type": "user", "timestamp": "2026-10-26T16:00:00Z", "message": {"content": "make the deck restart key"}})
    )
    .unwrap();
    let f = m.session_facts(&h, &HashMap::new(), &files, &Slow::default());
    assert_eq!(f.latest_prompt.as_deref(), Some("make the deck restart key"));
    // Claude records an interrupt as a user message of its own; it is no task
    // (owner screenshot, 2026-10-26: "[Request interrupted by user for tool use]"
    // as a title), and neither is a question.
    for content in [
        json!([{"type": "text", "text": "[Request interrupted by user for tool use]"}]),
        json!("[Request interrupted by user]"),
        json!("status of the tests?"),
    ] {
        writeln!(
            file,
            "{}",
            json!({"type": "user", "timestamp": "2026-10-26T16:05:00Z", "message": {"content": content}})
        )
        .unwrap();
    }
    let f = m.session_facts(&h, &HashMap::new(), &files, &Slow::default());
    assert_eq!(f.latest_prompt.as_deref(), Some("make the deck restart key"));
    // A rescan from further back never brings an older task back.
    m.scans.values_mut().for_each(|s| s.offset = 0);
    let f = m.session_facts(&h, &HashMap::new(), &files, &Slow::default());
    assert_eq!(f.latest_prompt.as_deref(), Some("make the deck restart key"));
}

#[test]
fn the_latest_task_is_found_behind_a_long_turn() {
    // A turn can write more than the scan reads from the end of a transcript.
    let dir = TempDir::new("monitor-task-deep");
    let mut m = monitor(&dir);
    let id = "019a1b2c-3d4e-5f60-7182-93a4b5c6d7e9";
    let path = rollout(
        &dir,
        id,
        "/srv/repo",
        "2026-10-25T10:00:00Z",
        &[json!({"type": "response_item", "timestamp": "2026-10-25T10:00:05Z", "payload": {
            "type": "message", "role": "user", "content": [{"type": "input_text", "text": "port the whole parser"}]}})],
    );
    let filler =
        json!({"type": "response_item", "payload": {"type": "function_call_output", "output": "x".repeat(4096)}});
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    for _ in 0..(SCAN_LIMIT as usize / 4096 + 64) {
        writeln!(file, "{filler}").unwrap();
    }
    m.discovered.insert(6, id.into());
    let f = m.session_facts(&hosted(6, Tool::Codex, 0), &HashMap::new(), &[], &Slow::default());
    assert_eq!(f.latest_prompt.as_deref(), Some("port the whole parser"));
}

#[test]
fn a_codex_thread_name_gives_way_to_a_later_task() {
    // Codex names a thread after its first task and never renames it.
    let dir = TempDir::new("monitor-codex-task");
    let mut m = monitor(&dir);
    let id = "019a1b2c-3d4e-5f60-7182-93a4b5c6d7ea";
    let said = |at: &str, text: &str| {
        json!({"type": "response_item", "timestamp": at, "payload": {
            "type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}})
    };
    let path = rollout(
        &dir,
        id,
        "/srv/repo",
        "2026-10-25T10:00:00Z",
        &[
            json!({"type": "response_item", "timestamp": "2026-10-25T10:00:01Z", "payload": {
                "type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "# AGENTS.md instructions for /srv/repo"},
                    {"type": "input_text", "text": "<environment_context> cwd and more </environment_context>"}]}}),
            said("2026-10-25T10:00:05Z", "finish the sample plan"),
            said("2026-10-25T10:30:00Z", "continue"),
        ],
    );
    dir.write(
        "codex/session_index.jsonl",
        json!({"id": id, "thread_name": "Finish Sample plan", "updated_at": "2026-10-25T10:00:09Z"}).to_string() + "\n",
    );
    m.refresh_names();
    m.discovered.insert(6, id.into());
    let h = hosted(6, Tool::Codex, 0);
    let f = m.session_facts(&h, &HashMap::new(), &[], &Slow::default());
    assert_eq!(f.meta_title.as_deref(), Some("Finish Sample plan"), "a reply leaves the name");
    assert_eq!(f.latest_prompt.as_deref(), Some("finish the sample plan"));
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{}", said("2026-10-25T11:00:00Z", "merge everything to master")).unwrap();
    let f = m.session_facts(&h, &HashMap::new(), &[], &Slow::default());
    assert_eq!(f.meta_title, None, "the name was for the task before");
    assert_eq!(f.latest_prompt.as_deref(), Some("merge everything to master"));
}

#[test]
fn codex_session_facts_use_discovered_rollout() {
    let dir = TempDir::new("monitor-codex");
    let mut m = monitor(&dir);
    let id = "019a1b2c-3d4e-5f60-7182-93a4b5c6d7e8";
    rollout(
        &dir,
        id,
        "/srv/repo",
        "2026-10-25T10:00:00Z",
        &[
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": "port the parser"}}),
            json!({"type": "event_msg", "payload": {"type": "task_started"}}),
        ],
    );
    dir.write("codex/session_index.jsonl", json!({"id": id, "thread_name": "Parser port"}).to_string() + "\n");
    m.refresh_names();
    m.discovered.insert(6, id.into());
    let f = m.session_facts(&hosted(6, Tool::Codex, 0), &HashMap::new(), &[], &Slow::default());
    assert_eq!(f.conversation.as_deref(), Some(id));
    assert_eq!(f.meta_title.as_deref(), Some("Parser port"));
    assert_eq!(f.first_prompt.as_deref(), Some("port the parser"));
    assert_eq!(f.busy, Some(true));
}

#[test]
fn codex_usage_reads_newest_rollout_with_limits() {
    let dir = TempDir::new("monitor-usage");
    let mut m = monitor(&dir);
    let now = util::now_secs();
    let with_limits = rollout(
        &dir,
        "u1",
        "/x",
        "2026-10-25T10:00:00Z",
        &[
            json!({"type": "event_msg", "payload": {"type": "token_count", "rate_limits": {"primary": {"used_percent": 33.0, "window_minutes": 10080, "resets_at": now + 1000}}}}),
        ],
    );
    let without = rollout(&dir, "u2", "/x", "2026-10-25T11:00:00Z", &[]);
    let recent = vec![(without, 2_000), (with_limits, 1_000)];
    let w = m.codex_usage(&recent);
    assert_eq!(w.len(), 1);
    assert_eq!((w[0].label.as_str(), w[0].used_percent.unwrap()), ("wk", 33.0));
    assert_eq!(w[0].updated_at, 1, "a sample without its own time is dated by its file");
    // Nothing recent: the last known windows are kept.
    assert_eq!(m.codex_usage(&[]).len(), 1);
}

#[test]
fn codex_usage_takes_the_newest_sample_not_the_newest_file() {
    // Review CR-20261026-3fdeb2041f: one session recorded 20%, another 60%
    // later, then the first wrote a tool result. Its file was the newest, so
    // the deck showed the older 20%.
    let dir = TempDir::new("monitor-usage-fresh");
    let mut m = monitor(&dir);
    let resets = util::now_secs() + 100_000;
    let base = codex::parse_rfc3339_ms("2026-10-25T10:00:00Z").unwrap();
    let sample = |at: &str, used: f64| json!({"timestamp": at, "type": "event_msg", "payload": {"type": "token_count", "rate_limits": {"primary": {"used_percent": used, "window_minutes": 10080, "resets_at": resets}}}});
    let tool_result = json!({"timestamp": "2026-10-25T10:10:00Z", "type": "response_item", "payload": {"type": "function_call_output", "output": "ok"}});
    let early =
        rollout(&dir, "early", "/x", "2026-10-25T09:00:00Z", &[sample("2026-10-25T10:00:00.000Z", 20.0), tool_result]);
    let later = rollout(&dir, "later", "/x", "2026-10-25T09:30:00Z", &[sample("2026-10-25T10:05:00.000Z", 60.0)]);
    let recent = vec![(early.clone(), base + 600_000), (later.clone(), base + 300_100)];
    let used = |w: &[UsageWindow]| w.iter().map(|w| (w.used_percent.unwrap(), w.updated_at)).collect::<Vec<_>>();
    assert_eq!(used(&m.codex_usage(&recent)), vec![(60.0, (base + 300_000) / 1000)]);
    assert_eq!(used(&m.codex_usage(&recent)), vec![(60.0, (base + 300_000) / 1000)], "unchanged files give the same");
    // The first session records a newer sample.
    fs::OpenOptions::new()
        .append(true)
        .open(&early)
        .unwrap()
        .write_all((sample("2026-10-25T10:11:00.000Z", 25.0).to_string() + "\n").as_bytes())
        .unwrap();
    let recent = vec![(early, base + 660_000), (later, base + 300_100)];
    assert_eq!(used(&m.codex_usage(&recent)), vec![(25.0, (base + 660_000) / 1000)]);
}

#[test]
fn embedded_codex_follows_the_thread_it_wrote_to_last() {
    // Verified live 2026-10-25: after `/resume` an embedded Codex keeps its
    // earlier conversation's rollout open beside the resumed one and writes
    // to the resumed one at once, but its notify names the thread only when a
    // turn ends (review CR-20261026-3fdeb2041f).
    let dir = TempDir::new("monitor-follow");
    let mut m = monitor(&dir);
    m.cfg.codex_notify = true;
    let first = rollout(&dir, "first", "/srv/repo", "2026-10-25T10:00:00Z", &[]);
    let resumed = rollout(&dir, "resumed", "/srv/repo", "2026-10-24T09:00:00Z", &[]);
    let helper = json!({"type": "session_meta", "payload": {"id": "helper", "cwd": "/srv/repo", "timestamp": "2026-10-25T10:00:05Z", "thread_source": "subagent"}});
    let helper = dir.write("codex/sessions/2026/10/25/rollout-x-helper.jsonl", helper.to_string() + "\n");
    let elsewhere = rollout(&dir, "elsewhere", "/srv/repo", "2026-10-25T08:00:00Z", &[]);
    let written = |path: &PathBuf, secs_ago: u64| {
        let at = std::time::SystemTime::now() - Duration::from_secs(secs_ago);
        fs::File::options().write(true).open(path).unwrap().set_modified(at).unwrap();
    };
    // This test process stands in for the hosted Codex, which holds its
    // rollouts open for appending. A sub-agent's rollout and one another live
    // session is having are written most recently.
    let _open = [&first, &resumed, &helper, &elsewhere].map(|p| fs::OpenOptions::new().append(true).open(p).unwrap());
    written(&helper, 1);
    written(&elsewhere, 1);
    let codex = |id: u64, conversation: &str, pid: Option<u32>| HostedRef {
        pid,
        conversation: Some(conversation.into()),
        ..hosted(id, Tool::Codex, 0)
    };
    let mut conversation_after = |recorded: &str| {
        *m.hosted.lock().unwrap() = vec![codex(1, recorded, Some(std::process::id())), codex(2, "elsewhere", None)];
        m.cycle().facts.get(&1).and_then(|f| f.conversation.clone())
    };
    written(&resumed, 60);
    written(&first, 30);
    assert_eq!(conversation_after("first").as_deref(), Some("first"), "it is still in its first conversation");
    // `/resume`: Codex appends to the resumed conversation's rollout.
    written(&resumed, 20);
    assert_eq!(conversation_after("first").as_deref(), Some("resumed"));
    // `/resume` back again, once the service has recorded the switch.
    written(&first, 10);
    assert_eq!(conversation_after("resumed").as_deref(), Some("first"));
}

#[test]
fn a_codex_exec_the_session_runs_is_not_its_conversation() {
    let dir = TempDir::new("monitor-nested");
    let mut m = monitor(&dir);
    m.cfg.codex_notify = true;
    let own = rollout(&dir, "own", "/srv/repo", "2026-10-25T10:00:00Z", &[]);
    let nested = rollout(&dir, "nested", "/srv/repo", "2026-10-25T10:01:00Z", &[]);
    let earlier = std::time::SystemTime::now() - Duration::from_secs(60);
    fs::File::options().write(true).open(&own).unwrap().set_modified(earlier).unwrap();
    // This test process stands in for the hosted Codex, holding its own
    // thread open; a child stands in for a `codex exec` it runs as a tool,
    // holding the newer thread that copy writes.
    let _own = fs::OpenOptions::new().append(true).open(&own).unwrap();
    let mut child =
        std::process::Command::new("sh").arg("-c").arg(r#"exec 3>>"$0"; exec sleep 30"#).arg(&nested).spawn().unwrap();
    wait_until_open(child.id(), &[&nested]);
    let me = std::process::id();
    let codex = |pid: u32, ppid: u32| ProcInfo { pid, ppid, comm: "codex".into(), state: 'S', start_ticks: 0 };
    let procs: HashMap<u32, ProcInfo> = [codex(me, 1), codex(child.id(), me)].into_iter().map(|p| (p.pid, p)).collect();
    let session = HostedRef {
        id: 1,
        tool: Tool::Codex,
        pid: Some(me),
        conversation: Some("own".into()),
        spawned_ms: 0,
        alive: true,
        left: Vec::new(),
        footer: Vec::new(),
        cwd: None,
        account_root: None,
    };
    m.follow_codex(&[session], &procs);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(m.followed.get(&1), None);
}

/// Wait until `pid` holds every one of `paths` open, as a child started to
/// hold them does once its shell has opened them.
fn wait_until_open(pid: u32, paths: &[&PathBuf]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let fds: Vec<PathBuf> = fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .flatten()
            .filter_map(|fd| fs::read_link(fd.path()).ok())
            .collect();
        // Descriptor targets are resolved; the paths may sit under an alias.
        if paths.iter().all(|p| fds.contains(p) || fs::canonicalize(p).is_ok_and(|real| fds.contains(&real))) {
            return;
        }
        assert!(Instant::now() < deadline, "the child never opened {paths:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_rollout_held_only_for_reading_is_not_the_sessions_conversation() {
    // Review CR-20261026-d5c080aeb2: a hosted Codex in one conversation ran
    // `tail -f` on an outside conversation's rollout as a tool. That rollout
    // was written last, so the deck followed the session into it and a
    // reopen resumed the wrong conversation.
    let dir = TempDir::new("monitor-readers");
    let mut m = monitor(&dir);
    m.cfg.codex_notify = true;
    let own = rollout(&dir, "own", "/srv/repo", "2026-10-25T10:00:00Z", &[]);
    let watched = rollout(&dir, "watched", "/srv/repo", "2026-10-25T10:01:00Z", &[]);
    let peeked = rollout(&dir, "peeked", "/srv/repo", "2026-10-25T10:02:00Z", &[]);
    let copied = rollout(&dir, "copied", "/srv/repo", "2026-10-25T10:03:00Z", &[]);
    let resumed = rollout(&dir, "resumed", "/srv/repo", "2026-10-24T10:00:00Z", &[]);
    let written = |path: &PathBuf, secs_ago: u64| {
        let at = std::time::SystemTime::now() - Duration::from_secs(secs_ago);
        fs::File::options().write(true).open(path).unwrap().set_modified(at).unwrap();
    };
    written(&own, 60);
    written(&resumed, 90);
    // This test process stands in for the hosted Codex: it appends to its
    // own conversation and only reads another one, as Codex's resume picker
    // does. A child stands in for a tool it runs, reading one rollout as
    // `tail -f` does and appending to another.
    let _own = fs::OpenOptions::new().append(true).open(&own).unwrap();
    let _peeked = fs::File::open(&peeked).unwrap();
    let _resumed = fs::OpenOptions::new().append(true).open(&resumed).unwrap();
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(r#"exec 3<"$0" 4>>"$1"; exec sleep 30"#)
        .arg(&watched)
        .arg(&copied)
        .spawn()
        .unwrap();
    wait_until_open(child.id(), &[&watched, &copied]);
    let me = std::process::id();
    let proc = |pid: u32, ppid: u32, comm: &str| ProcInfo { pid, ppid, comm: comm.into(), state: 'S', start_ticks: 0 };
    let procs: HashMap<u32, ProcInfo> =
        [proc(me, 1, "codex"), proc(child.id(), me, "tail")].into_iter().map(|p| (p.pid, p)).collect();
    let session = HostedRef {
        id: 1,
        tool: Tool::Codex,
        pid: Some(me),
        conversation: Some("own".into()),
        spawned_ms: 0,
        alive: true,
        left: Vec::new(),
        footer: Vec::new(),
        cwd: None,
        account_root: None,
    };
    // The rollout the tool reads, as in the review, is written last.
    written(&peeked, 5);
    written(&copied, 5);
    written(&watched, 3);
    m.follow_codex(std::slice::from_ref(&session), &procs);
    let stayed = m.followed.get(&1).cloned();
    // The session resumes a conversation of its own: Codex appends to it.
    written(&resumed, 1);
    m.follow_codex(&[session], &procs);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(stayed, None, "only rollouts the session's own Codex writes count");
    assert_eq!(m.followed.get(&1).map(String::as_str), Some("resumed"));
}

#[test]
fn a_codex_on_the_shared_server_follows_the_thread_its_status_line_names() {
    // Owner, 2026-10-26: "notifications still fire when agents are not done".
    // Codex #4, reopened when the service restarted, went on in a new thread
    // ("Merge everything to master" on its status line) while the deck read
    // the thread it had resumed, whose last turn had ended hours before.
    let dir = TempDir::new("monitor-footer");
    let mut m = monitor(&dir);
    let event = |kind: &str| json!({"type": "event_msg", "payload": {"type": kind}});
    rollout(&dir, "resumed", "/srv/repo", "2026-10-25T06:21:44Z", &[event("task_started"), event("task_complete")]);
    rollout(&dir, "current", "/srv/repo", "2026-10-25T19:14:41Z", &[event("task_started")]);
    rollout(&dir, "merge-a", "/srv/repo", "2026-10-25T19:20:00Z", &[]);
    rollout(&dir, "merge-b", "/srv/repo", "2026-10-25T19:30:00Z", &[]);
    rollout(&dir, "elsewhere", "/srv/other", "2026-10-25T19:40:00Z", &[]);
    rollout(&dir, "taken", "/srv/repo", "2026-10-25T19:50:00Z", &[]);
    let names = [
        ("resumed", "Finish Sample session1 plan.md"),
        ("current", "Merge everything to master"),
        ("merge-a", "Merge all ready trees"),
        ("merge-b", "Merge all ready trees"),
        ("elsewhere", "Fix the other repo"),
        ("taken", "Review the parser"),
    ];
    let index: String =
        names.iter().map(|(id, name)| json!({"id": id, "thread_name": name}).to_string() + "\n").collect();
    dir.write("codex/session_index.jsonl", index);
    let spawned_ms = crate::codex::parse_rfc3339_ms("2026-10-25T19:14:05Z").unwrap();
    let codex = |id: u64, conversation: &str, footer: &[&str]| HostedRef {
        id,
        tool: Tool::Codex,
        pid: None,
        conversation: Some(conversation.into()),
        spawned_ms,
        alive: true,
        left: Vec::new(),
        footer: footer.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        account_root: None,
    };
    let mut seen = |name: &str| {
        let footer = ["GPT-6.1-Sol xhigh", name, "weekly 64% left"];
        *m.hosted.lock().unwrap() = vec![codex(4, "resumed", &footer), codex(9, "taken", &["Review the parser"])];
        let f = m.cycle().facts.get(&4).cloned().unwrap();
        (f.conversation.unwrap(), f.busy)
    };
    assert_eq!(
        seen("Finish Sample session1 plan.md"),
        ("resumed".into(), Some(false)),
        "it shows the thread it resumed"
    );
    assert_eq!(
        seen("Merge everything to master"),
        ("current".into(), Some(true)),
        "its turn runs in the thread it shows"
    );
    assert_eq!(seen("Merge everything t…"), ("current".into(), Some(true)), "a name cut short in a narrow pane");
    // Two threads carry the name, or another session's does, or another
    // checkout's: no guess.
    for name in ["Merge all ready trees", "Review the parser", "Fix the other repo"] {
        assert_eq!(seen(name).0, "resumed", "{name}");
    }
}

#[test]
fn codex_usage_reads_every_file_that_can_hold_a_newer_sample() {
    // Review CR-20261026-d5c080aeb2: the deck read only the twelve files
    // written last. Twelve sessions appending tool output put the newest
    // sample thirteenth, and an older one was shown.
    let dir = TempDir::new("monitor-usage-many");
    let mut m = monitor(&dir);
    let resets = util::now_secs() + 100_000;
    let base = codex::parse_rfc3339_ms("2026-10-25T10:00:00Z").unwrap();
    let sample = |at: &str, used: f64| json!({"timestamp": at, "type": "event_msg", "payload": {"type": "token_count", "rate_limits": {"primary": {"used_percent": used, "window_minutes": 10080, "resets_at": resets}}}});
    let mut recent = Vec::new();
    for n in 0..12u64 {
        let older = rollout(
            &dir,
            &format!("busy{n}"),
            "/x",
            "2026-10-25T09:00:00Z",
            &[sample("2026-10-25T10:00:00.000Z", 20.0)],
        );
        recent.push((older, base + 600_000 - n * 1_000));
    }
    let newest = rollout(&dir, "quiet", "/x", "2026-10-25T09:00:00Z", &[sample("2026-10-25T10:05:00.000Z", 60.0)]);
    recent.push((newest, base + 300_100));
    let used: Vec<f64> = m.codex_usage(&recent).iter().map(|w| w.used_percent.unwrap()).collect();
    assert_eq!(used, vec![60.0]);
}

#[test]
fn codex_usage_includes_rollouts_codex_holds_open_from_any_day() {
    // Review CR-20261026-d5c080aeb2: rollouts are filed under the day their
    // conversation began, and the deck read only the two newest days. A
    // conversation resumed from last week held the newest sample.
    let dir = TempDir::new("monitor-usage-held");
    let mut m = monitor(&dir);
    let resets = util::now_secs() + 100_000;
    let sample = |at: &str, used: f64| json!({"timestamp": at, "type": "event_msg", "payload": {"type": "token_count", "rate_limits": {"primary": {"used_percent": used, "window_minutes": 10080, "resets_at": resets}}}});
    let today = rollout(&dir, "today", "/x", "2026-10-25T09:00:00Z", &[sample("2026-10-25T10:00:00.000Z", 20.0)]);
    let meta =
        json!({"type": "session_meta", "payload": {"id": "old", "cwd": "/x", "timestamp": "2026-10-18T09:00:00Z"}});
    let text = format!("{meta}\n{}\n", sample("2026-10-25T10:05:00.000Z", 60.0));
    let resumed = dir.write("codex/sessions/2026/10/18/rollout-x-old.jsonl", text);
    let _yesterday = dir.write("codex/sessions/2026/10/24/rollout-x-y.jsonl", "");
    let written = |path: &PathBuf, secs_ago: u64| {
        let at = std::time::SystemTime::now() - Duration::from_secs(secs_ago);
        fs::File::options().write(true).open(path).unwrap().set_modified(at).unwrap();
    };
    written(&today, 30);
    written(&resumed, 10);
    // A process named `codex` holds the resumed conversation open for
    // appending, as the shared Codex server and an embedded Codex do.
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sh", bin.join("codex")).unwrap();
    // `read` keeps the shell, and so its name, until the test kills it.
    let mut codex = std::process::Command::new(bin.join("codex"))
        .arg("-c")
        .arg(r#"exec 3>>"$0"; read -r line"#)
        .arg(&resumed)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_until_open(codex.id(), &[&resumed]);
    let usage = m.cycle().usage.codex;
    let _ = codex.kill();
    let _ = codex.wait();
    assert_eq!(usage.iter().map(|w| w.used_percent.unwrap()).collect::<Vec<_>>(), vec![60.0]);
}

/// Appends `bytes` of turn output that carries no turn event, as a long Codex
/// turn writes between its `task_started` and `task_complete`.
fn append_turn_output(path: &Path, bytes: usize) {
    let chunk = "x".repeat(60 * 1024);
    let line = json!({"type": "response_item", "payload": {"type": "message", "text": chunk}}).to_string() + "\n";
    let mut file = fs::File::options().append(true).open(path).unwrap();
    for _ in 0..bytes.div_ceil(line.len()) {
        file.write_all(line.as_bytes()).unwrap();
    }
}

#[test]
fn a_long_codex_turn_stays_working_until_its_turn_ends() {
    let dir = TempDir::new("monitor-turn");
    let started = json!({"type": "event_msg", "payload": {"type": "task_started"}});
    let path = rollout(&dir, "long", "/srv/repo", "2026-10-26T09:00:00Z", &[started]);
    // More than the recent tail since the turn began: the deeper read finds it.
    append_turn_output(&path, 300 * 1024);
    let mut fresh = monitor(&dir);
    assert_eq!(fresh.codex_turn(&path), Some(true));
    // So much output that no read reaches `task_started`: the state read
    // earlier stays, rather than the screen's passing working line deciding.
    let mut m = monitor(&dir);
    let short = rollout(
        &dir,
        "longer",
        "/srv/repo",
        "2026-10-26T09:00:00Z",
        &[json!({"type": "event_msg", "payload": {"type": "task_started"}})],
    );
    assert_eq!(m.codex_turn(&short), Some(true));
    append_turn_output(&short, 9 << 20);
    assert_eq!(m.codex_turn(&short), Some(true));
    // The turn's end replaces it at once.
    let complete = json!({"type": "event_msg", "payload": {"type": "task_complete"}}).to_string() + "\n";
    fs::File::options().append(true).open(&short).unwrap().write_all(complete.as_bytes()).unwrap();
    assert_eq!(m.codex_turn(&short), Some(false));
    append_turn_output(&short, 300 * 1024);
    assert_eq!(m.codex_turn(&short), Some(false));
}

#[test]
fn review_a_footer_never_reenters_a_codex_conversation_left_by_clear() {
    let dir = TempDir::new("monitor-footer-left");
    let mut m = monitor(&dir);
    let event = |kind: &str| json!({"type": "event_msg", "payload": {"type": kind}});
    rollout(&dir, "old", "/srv/repo", "2026-10-25T10:00:00Z", &[event("task_complete")]);
    rollout(&dir, "new", "/srv/repo", "2026-10-25T10:31:00Z", &[event("task_started")]);
    dir.write(
        "codex/session_index.jsonl",
        [("old", "Earlier task"), ("new", "Current task")]
            .map(|(id, name)| json!({"id": id, "thread_name": name}).to_string() + "\n")
            .concat(),
    );
    m.refresh_names();
    let mut session = hosted(1, Tool::Codex, crate::codex::parse_rfc3339_ms("2026-10-25T10:30:00Z").unwrap());
    session.left = vec!["old".into()];
    session.footer = vec!["Earlier task".into()];
    let facts = |m: &mut Monitor, session: &HostedRef| {
        m.follow_codex(std::slice::from_ref(session), &HashMap::new());
        m.session_facts(session, &HashMap::new(), &[], &Slow::default())
    };
    let cleared = facts(&mut m, &session);
    assert_eq!(cleared.conversation, None, "a stale footer cannot undo /clear");
    assert_eq!(cleared.meta_title, None);
    assert_eq!(cleared.busy, None, "the previous thread's completed turn is no finish report for the new chat");

    session.footer = vec!["Current task".into()];
    let current = facts(&mut m, &session);
    assert_eq!(current.conversation.as_deref(), Some("new"));
    assert_eq!(current.busy, Some(true));

    // Explicitly resuming a conversation remains valid: the service records
    // it and removes it from left when the person resumes it.
    session.conversation = Some("old".into());
    session.left.clear();
    session.footer = vec!["Earlier task".into()];
    let resumed = facts(&mut m, &session);
    assert_eq!(resumed.conversation.as_deref(), Some("old"));
    assert_eq!(resumed.busy, Some(false));
}

#[test]
fn a_working_unlinked_codex_is_identified_by_its_hidden_cursor_footer() {
    let dir = TempDir::new("monitor-working-footer");
    let mut m = monitor(&dir);
    rollout(&dir, "current", "/srv/repo", "2026-10-25T10:00:00Z", &[]);
    dir.write(
        "codex/session_index.jsonl",
        json!({"id": "current", "thread_name": "Add Morphy collection"}).to_string() + "\n",
    );
    let screen: Vec<String> = "• Working (esc to interrupt)\n\n› Ask Codex to do anything\n\n  GPT-6.1-Sol default · Add Morphy collection · weekly 88% left\n  ← for agents · ? for shortcuts".lines().map(str::to_string).collect();
    // An old session's first request falls outside startup-time discovery.
    let mut session = hosted(35, Tool::Codex, 0);
    session.footer = crate::status::codex_footer(&screen, None);
    *m.hosted.lock().unwrap() = vec![session];
    let report = m.cycle();
    assert_eq!(report.facts[&35].conversation.as_deref(), Some("current"));
}

#[test]
fn active_goal_status_belongs_to_the_current_codex_conversation() {
    let dir = TempDir::new("monitor-goal");
    let mut m = monitor(&dir);
    let mut session = hosted(1, Tool::Codex, 0);
    session.conversation = Some("goal-thread".into());
    rollout(
        &dir,
        "goal-thread",
        "/srv/repo",
        "2026-10-25T06:21:44Z",
        &[json!({"type": "event_msg", "payload": {"type": "task_complete"}})],
    );
    m.active_goals = Some(HashSet::from(["goal-thread".into()]));
    let facts = m.session_facts(&session, &HashMap::new(), &[], &Slow::default());
    assert_eq!(facts.busy, Some(false), "an individual turn ended");
    assert_eq!(facts.goal_active, Some(true), "the goal is independently active");
    m.followed.insert(1, "other-thread".into());
    assert_eq!(m.session_facts(&session, &HashMap::new(), &[], &Slow::default()).goal_active, Some(false));
    m.active_goals = None;
    assert_eq!(m.session_facts(&session, &HashMap::new(), &[], &Slow::default()).goal_active, None);
}

#[test]
fn outside_codex_uses_only_its_own_writable_user_rollout() {
    struct Running(Box<dyn portable_pty::Child + Send + Sync>);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = TempDir::new("outside-codex-identity");
    let mut m = monitor(&dir);
    m.cfg.service_pid = u32::MAX;
    let current = rollout(&dir, "current", "/srv/repo", "2026-10-25T10:00:00Z", &[]);
    let old = rollout(&dir, "old", "/srv/repo", "2026-10-24T10:00:00Z", &[]);
    let helper = dir.write(
        "codex/sessions/2026/10/25/rollout-x-helper.jsonl",
        json!({"type":"session_meta","payload":{"id":"helper","thread_source":"subagent"}}).to_string(),
    );
    let pair = portable_pty::native_pty_system().openpty(portable_pty::PtySize::default()).unwrap();
    let mut command = portable_pty::CommandBuilder::new("/bin/sh");
    command.args(["-c", r#"exec 3>>"$1" 4<"$2" 5>>"$3"; read -r line"#, "sh"]);
    command.arg(&current);
    command.arg(&old);
    command.arg(&helper);
    let child = Running(pair.slave.spawn_command(command).unwrap());
    let pid = child.0.process_id().unwrap();
    wait_until_open(pid, &[&current, &old, &helper]);
    let mut info = procs::stat(pid).unwrap();
    info.comm = "codex".into();
    let procs = HashMap::from([(pid, info)]);
    let outside = m.outside(&procs, &[], &HashSet::new(), &Slow::default());
    assert_eq!(outside.len(), 1);
    assert_eq!(outside[0].conversation.as_deref(), Some("current"));
}

#[test]
fn agy_clear_is_reported_from_its_own_conversation_history() {
    let dir = TempDir::new("monitor-agy-clear");
    let records = [
        json!({"conversationId": "old", "workspace": "/srv/repo", "timestamp": 100, "display": "SampleDB enterprise reading"}),
        json!({"conversationId": "old", "workspace": "/srv/repo", "timestamp": 200, "display": "/clear", "type": "slash_command"}),
        json!({"conversationId": "other", "workspace": "/srv/repo", "timestamp": 300, "display": "/clear", "type": "slash_command"}),
        json!({"conversationId": "old", "workspace": "/elsewhere", "timestamp": 400, "display": "/clear", "type": "slash_command"}),
        json!({"conversationId": "old", "workspace": "/srv/repo", "timestamp": 500, "display": "/clear"}),
    ];
    dir.write("agy/history.jsonl", records.iter().map(|v| format!("{v}\n")).collect::<String>());
    let mut m = monitor(&dir);
    let mut session = hosted(1, Tool::Antigravity, 50);
    session.conversation = Some("old".into());
    let facts = m.session_facts(&session, &HashMap::new(), &[], &Slow::default());
    assert_eq!(facts.cleared_ms, Some(200), "the clear belongs to this conversation and workspace");
    session.spawned_ms = 250;
    assert_eq!(
        m.session_facts(&session, &HashMap::new(), &[], &Slow::default()).cleared_ms,
        None,
        "resuming an old conversation must not replay a historical clear"
    );
}

#[test]
fn agy_clear_never_rediscovers_a_left_conversation_from_history_or_open_database() {
    let dir = TempDir::new("monitor-agy-left");
    let db = dir.write("agy/conversations/old.db", "");
    let _open = fs::File::open(db).unwrap();
    dir.write(
        "agy/history.jsonl",
        format!(
            "{}\n",
            json!({
                "conversationId": "old", "workspace": "/srv/repo", "timestamp": 200,
                "display": "/clear", "type": "slash_command"
            })
        ),
    );
    let mut m = monitor(&dir);
    let mut session = hosted(1, Tool::Antigravity, 200);
    session.left.push("old".into());
    m.discover_antigravity(&[session.clone()], &HashMap::new());
    assert!(!m.discovered.contains_key(&1), "the clear command in history cannot restore its old chat");
    session.pid = Some(std::process::id());
    m.discover_antigravity(&[session.clone()], &HashMap::new());
    assert!(!m.discovered.contains_key(&1), "a lingering database descriptor cannot restore its old chat");
    dir.write(
        "agy/history.jsonl",
        format!(
            "{}\n",
            json!({
                "conversationId": "new", "workspace": "/srv/repo", "timestamp": 300, "display": "Fix the parser"
            })
        ),
    );
    session.pid = None;
    m.agy_prompts.0 = None;
    m.discover_antigravity(&[session], &HashMap::new());
    assert_eq!(m.discovered.get(&1).map(String::as_str), Some("new"), "the next prompt names the new chat");
}

#[test]
fn outside_shared_codex_is_identified_by_its_own_live_screen() {
    let dir = TempDir::new("outside-shared-screen");
    let current = "11111111-aaaa-4bbb-8ccc-123456789abc";
    rollout(&dir, current, "/srv/repo", "2026-10-25T10:00:00Z", &[]);
    dir.write("codex/session_index.jsonl", json!({"id": current, "thread_name": "Port the parser"}).to_string() + "\n");
    let screen = "• Working (esc to interrupt)\n\n› Ask Codex to do anything\n\n  GPT-6.1-Sol default · Port the parser · weekly 88% left\n  ← for agents · ? for shortcuts";
    // A name is not an identity, even when only one saved thread uses it.
    assert_eq!(codex_screen_conversation(&dir.path().join("codex"), "/srv/repo", screen), None);
    assert_eq!(codex_screen_conversation(&dir.path().join("codex"), "/other", screen), None);
    assert_eq!(
        codex_screen_conversation(&dir.path().join("codex"), "/srv/repo", "Assistant said: Port the parser"),
        None
    );
    // Another saved thread with the same name makes the display ambiguous.
    let other = "22222222-aaaa-4bbb-8ccc-123456789abc";
    rollout(&dir, other, "/srv/repo", "2026-10-25T11:00:00Z", &[]);
    let mut index = fs::OpenOptions::new().append(true).open(dir.path().join("codex/session_index.jsonl")).unwrap();
    writeln!(index, "{}", json!({"id":other,"thread_name":"Port the parser"})).unwrap();
    assert_eq!(codex_screen_conversation(&dir.path().join("codex"), "/srv/repo", screen), None);
    // Displaying the exact session ID resolves repeated names automatically.
    let exact = screen.replace("weekly 88% left", current);
    assert_eq!(codex_screen_conversation(&dir.path().join("codex"), "/srv/repo", &exact).as_deref(), Some(current));
    assert_eq!(codex_screen_conversation(&dir.path().join("codex"), "/other", &exact), None);
    let unknown = exact.replace(current, "33333333-aaaa-4bbb-8ccc-123456789abc");
    assert_eq!(codex_screen_conversation(&dir.path().join("codex"), "/srv/repo", &unknown), None);
    let ambiguous = exact.replace("Port the parser", other);
    assert_eq!(codex_screen_conversation(&dir.path().join("codex"), "/srv/repo", &ambiguous), None);
}

#[test]
fn a_programs_own_record_of_its_completed_turns_is_read_again_only_once_it_changed() {
    let dir = TempDir::new("monitor-recorded-turn");
    let mut m = monitor(&dir);
    let id = "01a157f0-ee7a-7cd3-8d82-aad3603dd763";
    let path = dir.path().join(format!("grok/sessions/%2Fsrv%2Frepo/{id}/events.jsonl"));
    assert_eq!(m.recorded_turn(Tool::Grok, id), None, "no record yet");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let ended = |ts: &str| format!("{{\"ts\":\"{ts}\",\"type\":\"turn_ended\",\"outcome\":\"completed\"}}\n");
    let stamp = |secs: u64| {
        let at = std::time::UNIX_EPOCH + Duration::from_secs(secs);
        fs::File::options().write(true).open(&path).unwrap().set_modified(at).unwrap();
    };
    fs::write(&path, ended("2026-11-01T10:05:04.000Z")).unwrap();
    stamp(1000);
    let first = m.recorded_turn(Tool::Grok, id).unwrap();
    assert_eq!(
        (first.id.as_str(), first.at_ms, first.superseded),
        ("2026-11-01T10:05:04.000Z", 1_793_527_504_000, false)
    );
    // Unchanged, it is not read again.
    fs::write(&path, ended("2026-11-01T10:09:00.000Z")).unwrap();
    stamp(1000);
    assert_eq!(m.recorded_turn(Tool::Grok, id), Some(first.clone()));
    // A long next turn's phases push its start and the last end out of the
    // tail: the last end found stays.
    let phases = "{\"ts\":\"x\",\"type\":\"phase_changed\",\"phase\":\"streaming_text\"}\n".repeat(2000);
    let record = ended("2026-11-01T10:05:04.000Z") + "{\"ts\":\"y\",\"type\":\"turn_started\"}\n" + &phases;
    fs::write(&path, &record).unwrap();
    stamp(1001);
    assert_eq!(m.recorded_turn(Tool::Grok, id), Some(first));
    // Its own end replaces it.
    fs::write(&path, record + &ended("2026-11-01T10:20:00.000Z")).unwrap();
    stamp(1002);
    assert_eq!(m.recorded_turn(Tool::Grok, id).unwrap().id, "2026-11-01T10:20:00.000Z");

    // Codex's rollout: the same read that says whether a turn runs.
    let complete = json!({"timestamp": "2026-11-08T07:34:05.017Z", "type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t9"}});
    let path = rollout(&dir, "ended", "/srv/repo", "2026-11-08T07:30:00Z", &[complete]);
    assert_eq!(m.codex_turn(&path), Some(false));
    assert_eq!(m.codex_turn_end(&path).map(|end| end.id), Some("t9".to_string()));
    append_turn_output(&path, 300 * 1024);
    m.codex_turn(&path);
    assert_eq!(m.codex_turn_end(&path).map(|end| end.id), Some("t9".to_string()), "kept past the tail");
}
