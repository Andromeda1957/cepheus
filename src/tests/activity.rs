use serde_json::json;

use super::*;

fn agent(name: &str, active: bool, last_active: u64) -> Subagent {
    Subagent { name: name.into(), kind: None, active, started_at: 0, last_active }
}

/// A `background_tasks` entry for a sub-agent, as Claude Code 2.1.319 writes it.
fn task(id: &str, status: &str, description: &str) -> Value {
    json!({"id": id, "type": "subagent", "status": status, "description": description, "agent_type": "general-purpose"})
}

/// The `Stop` hook input as Claude Code 2.1.319 writes it.
fn stop(tasks: Vec<Value>) -> Value {
    json!({
        "session_id": "conv-1", "transcript_path": "/p/conv-1.jsonl", "cwd": "/w", "permission_mode": "default",
        "hook_event_name": "Stop", "stop_hook_active": false, "background_tasks": tasks, "session_crons": []
    })
}

/// The `SubagentStop` hook input: the agent whose turn ended, and the
/// session's whole in-flight work.
fn subagent_stop(agent: &str, tasks: Vec<Value>) -> Value {
    json!({
        "session_id": "conv-1", "transcript_path": "/p/conv-1.jsonl", "cwd": "/w", "permission_mode": "default",
        "hook_event_name": "SubagentStop", "stop_hook_active": false, "agent_id": agent,
        "agent_transcript_path": format!("/p/conv-1/subagents/agent-{agent}.jsonl"), "agent_type": "general-purpose",
        "background_tasks": tasks, "session_crons": []
    })
}

fn subagent_start(agent: &str, kind: &str) -> Value {
    json!({
        "session_id": "conv-1", "transcript_path": "/p/conv-1.jsonl", "cwd": "/w",
        "hook_event_name": "SubagentStart", "agent_id": agent, "agent_type": kind
    })
}

fn running(agents: &[ClaudeAgent]) -> Vec<&str> {
    agents.iter().filter(|a| a.agent.active).map(|a| a.id.as_str()).collect()
}

#[test]
fn claudes_sub_agents_are_the_running_ones_its_stop_hook_lists() {
    let mut agents = Vec::new();
    let now = 1_000_000;
    let tasks = vec![
        task("a1", "running", "Inspect the\nparser"),
        task("a2", "running", "Review the tests"),
        task("a3", "completed", "Done already"),
        task("a4", "killed", "Stopped by the owner"),
        // A backgrounded main session is not one of its agents.
        json!({"id": "m1", "type": "subagent", "status": "running", "description": "main", "agent_type": "main-session"}),
        json!({"id": "b1", "type": "shell", "status": "running", "description": "cargo test", "command": "cargo test"}),
        json!({"id": "w1", "type": "workflow", "status": "running", "description": "review", "name": "review"}),
        json!({"id": "d1", "type": "dream", "status": "running", "description": "memory"}),
    ];
    assert!(claude_hook(&mut agents, &stop(tasks), now));
    assert_eq!(running(&agents), ["a1", "a2"]);
    let first = &agents.iter().find(|a| a.id == "a1").unwrap().agent;
    assert_eq!((first.name.as_str(), first.kind.as_deref()), ("Inspect the parser", Some("general-purpose")));
    assert_eq!((first.started_at, first.last_active), (now, now));

    // Nothing else is a report of its sub-agents.
    let notification = json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"});
    assert!(!claude_hook(&mut agents, &notification, now));
    // A Claude too old to list its background work tells nothing.
    assert!(!claude_hook(&mut agents, &json!({"hook_event_name": "Stop"}), now));
    assert_eq!(running(&agents), ["a1", "a2"]);
}

#[test]
fn a_stopped_claude_sub_agent_is_gone_at_the_next_report() {
    // Owner, 2026-11-08: "say like you had to stop agents and relaunch, it
    // doesnt factor that stuff in". A stopped or killed agent never says it
    // finished; Claude no longer lists it.
    let mut agents = Vec::new();
    let now = 1_000_000;
    claude_hook(&mut agents, &stop(vec![task("a1", "running", "one"), task("a2", "running", "two")]), now);
    claude_hook(&mut agents, &stop(vec![task("a2", "running", "two"), task("a1", "killed", "one")]), now + 60);
    assert_eq!(running(&agents), ["a2"]);
    let stopped = &agents.iter().find(|a| a.id == "a1").unwrap().agent;
    assert_eq!(
        (stopped.active, stopped.last_active),
        (false, now + 60),
        "recent, ended when Claude stopped listing it"
    );
    // The relaunch is a new agent of its own.
    claude_hook(&mut agents, &subagent_start("a3", "general-purpose"), now + 70);
    assert_eq!(running(&agents), ["a3", "a2"]);
    claude_hook(&mut agents, &stop(vec![]), now + 80);
    assert!(running(&agents).is_empty());
    assert_eq!(agents.len(), 3, "ended ones stay listed as recent");
}

#[test]
fn a_claude_sub_agent_quiet_for_over_half_an_hour_counts_while_claude_lists_it() {
    // Owner, 2026-11-08: Claude said "Waiting for 17 background agents to
    // finish" while the row showed 14: agents waiting on a long build write
    // nothing for a long while.
    let mut agents = Vec::new();
    let now = 1_000_000;
    let all: Vec<Value> = (0..17).map(|i| task(&format!("a{i}"), "running", "build")).collect();
    claude_hook(&mut agents, &stop(all.clone()), now);
    claude_hook(&mut agents, &stop(all), now + 3 * 3600);
    assert_eq!(running(&agents).len(), 17);
    assert!(agents.iter().all(|a| a.agent.started_at == now), "started when the deck first heard of it");
}

#[test]
fn a_claude_sub_agent_whose_turn_ended_with_work_running_still_counts() {
    // Its SubagentStop comes as its turn ends; while its own background work
    // runs, Claude keeps it running and lists it, with what it started.
    let mut agents = Vec::new();
    let now = 1_000_000;
    claude_hook(&mut agents, &stop(vec![task("a1", "running", "Broadcast to running agents")]), now);
    let nested = task("n1", "running", "Ask one agent");
    let shell =
        json!({"id": "b1", "type": "shell", "status": "running", "description": "wait", "command": "sleep 600"});
    let report = subagent_stop("a1", vec![task("a1", "running", "Broadcast to running agents"), nested, shell]);
    claude_hook(&mut agents, &report, now + 5);
    assert_eq!(running(&agents), ["n1", "a1"], "the agents Claude's own count includes");
    // Done: no longer listed.
    claude_hook(&mut agents, &stop(vec![]), now + 900);
    assert!(running(&agents).is_empty());
}

#[test]
fn claudes_subagent_start_adds_the_agent_at_once() {
    let mut agents = Vec::new();
    let now = 1_000_000;
    assert!(claude_hook(&mut agents, &subagent_start("a1", "Explore"), now));
    assert_eq!(running(&agents), ["a1"]);
    assert_eq!(agents[0].agent.name, "Explore", "named by its type until a report names its task");
    // Started again (a resumed agent) is still one.
    assert!(claude_hook(&mut agents, &subagent_start("a1", "Explore"), now + 1));
    assert_eq!(agents.len(), 1);
    assert!(!claude_hook(&mut agents, &subagent_start("", "Explore"), now));
    claude_hook(&mut agents, &stop(vec![task("a1", "running", "Map the hooks")]), now + 2);
    let a = &agents[0].agent;
    assert_eq!((a.name.as_str(), a.started_at), ("Map the hooks", now));
    // One run in the foreground is never listed: its SubagentStop drops it.
    claude_hook(&mut agents, &subagent_start("f1", "Plan"), now + 3);
    assert_eq!(running(&agents), ["f1", "a1"]);
    claude_hook(&mut agents, &subagent_stop("f1", vec![task("a1", "running", "Map the hooks")]), now + 4);
    assert_eq!(running(&agents), ["a1"]);
}

#[test]
fn every_running_claude_sub_agent_is_listed_and_old_ended_ones_go() {
    let mut agents = Vec::new();
    let now = 1_000_000;
    let all: Vec<Value> = (0..40).map(|i| task(&format!("a{i}"), "running", "work")).collect();
    claude_hook(&mut agents, &stop(all), now);
    assert_eq!(running(&agents).len(), 40, "the row counts every one");
    claude_hook(&mut agents, &stop(vec![task("a0", "running", "work")]), now + 10);
    assert_eq!((running(&agents), agents.len()), (vec!["a0"], MAX_LISTED));
    claude_hook(&mut agents, &stop(vec![task("a0", "running", "work")]), now + 10 + FINISHED_KEEP_SECS);
    assert_eq!(agents.len(), 1);
    assert_eq!(claude_listed(&agents), vec![agents[0].agent.clone()]);
}

#[test]
fn keep_recent_lists_running_first_then_newest_at_most_the_limit() {
    let now = 100_000;
    let mut agents = vec![agent("old done", false, now - FINISHED_KEEP_SECS - 1), agent("done", false, now - 10)];
    agents.push(agent("running older", true, now - 50));
    agents.push(agent("running newer", true, now - 5));
    let kept: Vec<String> = keep_recent(agents, now).into_iter().map(|a| a.name).collect();
    assert_eq!(kept, ["running newer", "running older", "done"]);
    let many: Vec<Subagent> = (0..40).map(|i| agent(&format!("a{i}"), i % 2 == 0, now - i)).collect();
    let kept = keep_recent(many, now);
    assert_eq!(kept.len(), MAX_LISTED);
    assert!(kept[..20].iter().all(|a| a.active));
}

#[test]
fn a_tool_call_reads_as_what_it_works_on() {
    assert_eq!(
        describe_tool("Bash", &json!({"command": "cargo test", "description": "Run the tests"})),
        "Bash: Run the tests"
    );
    assert_eq!(describe_tool("Bash", &json!({"command": "cargo\n test"})), "Bash: cargo test");
    assert_eq!(describe_tool("Edit", &json!({"file_path": "/home/x/src/lua.rs"})), "Edit lua.rs");
    assert_eq!(describe_tool("NotebookEdit", &json!({"notebook_path": "/a/b.ipynb"})), "NotebookEdit b.ipynb");
    assert_eq!(describe_tool("Grep", &json!({"pattern": "fn run"})), "Grep \u{201c}fn run\u{201d}");
    assert_eq!(describe_tool("WebFetch", &json!({"url": "https://docs.rs/mlua/latest?x=1"})), "Fetch docs.rs");
    assert_eq!(describe_tool("Agent", &json!({"description": "Review the diff"})), "Agent: Review the diff");
    assert_eq!(describe_tool("mcp__playwright__browser_click", &json!({})), "playwright: browser_click");
    assert_eq!(describe_tool("Read", &json!({})), "Read");
    let long = describe_tool("Bash", &json!({"description": "x".repeat(500)}));
    assert!(long.chars().count() <= ACTIVITY_WIDTH, "{}", long.len());
}

#[test]
fn claude_activity_is_its_last_tool_call() {
    let tail = [
        r#"_use","name":"Grep"}]},"type":"tool_use"}"#.to_string(),
        json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/r/a.rs"}}]}}).to_string(),
        json!({"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}).to_string(),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}).to_string(),
        // The line being written as the tail was read.
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Write""#.to_string(),
    ]
    .join("\n");
    assert_eq!(claude_activity(tail.as_bytes()), Some("Read a.rs".into()));
    let later = format!(
        "{tail}\n{}",
        json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Read","input":{"file_path":"/r/b.rs"}},
            {"type":"tool_use","name":"Bash","input":{"description":"Build it"}}]}})
    );
    assert_eq!(claude_activity(later.as_bytes()), Some("Bash: Build it".into()));
    assert_eq!(claude_activity(b"{}\n"), None);
}

#[test]
fn codex_activity_is_its_last_command_edit_or_tool() {
    let command = json!({"type":"event_msg","payload":{"type":"item_completed","item":{
        "type":"CommandExecution","command":["zsh","-lc","ls"],"parsed_cmd":[{"cmd":"rg -n fn"}]}}});
    let call = json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command",
        "arguments":"{\"cmd\":[\"zsh\",\"-lc\",\"cargo test\\n --quiet\"]}"}});
    let edit = json!({"type":"event_msg","payload":{"type":"item_completed","item":{
        "type":"FileChange","changes":{"/r/src/a.rs":{},"/r/src/b.rs":{},"/r/src/c.rs":{}}}}});
    let tool = json!({"type":"event_msg","payload":{"type":"item_started","item":{
        "type":"McpToolCall","server":"rea","tool":"open_binary"}}});
    let lines = |v: &[&Value]| v.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n");
    assert_eq!(codex_activity(lines(&[&command]).as_bytes()), Some("Ran: rg -n fn".into()));
    assert_eq!(codex_activity(lines(&[&command, &call]).as_bytes()), Some("Ran: cargo test --quiet".into()));
    assert_eq!(codex_activity(lines(&[&call, &edit]).as_bytes()), Some("Edit a.rs and 2 more".into()));
    assert_eq!(codex_activity(lines(&[&edit, &tool]).as_bytes()), Some("rea: open_binary".into()));
    assert_eq!(codex_activity(b"{\"payload\":{\"type\":\"token_count\"}}"), None);
}

#[test]
fn a_codex_sub_agent_is_named_by_its_nickname_and_task() {
    let now = 10_000;
    let a = codex_subagent(Some("Ada"), Some("/root/review_web_ci"), 5_000_000, now - 10, None, now);
    assert_eq!((a.name.as_str(), a.active, a.started_at), ("Ada: review web ci", true, 5_000));
    assert!(!codex_subagent(Some("Ada"), None, 0, now - CODEX_IDLE_LIMIT_SECS - 1, None, now).active);
    // A known turn state wins over how recently it wrote.
    assert!(!codex_subagent(None, Some("/root/x"), 0, now, Some(false), now).active);
    assert!(codex_subagent(None, None, 0, 0, Some(true), now).active);
    assert_eq!(codex_subagent(None, None, 0, 0, None, now).name, "sub-agent");
    assert_eq!(codex_subagent(None, Some("/root/fix_it"), 0, 0, None, now).name, "fix it");
}
