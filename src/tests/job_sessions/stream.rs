use super::*;
use serde_json::{json, Value};

fn lines(records: &[Value]) -> String {
    records.iter().map(|record| format!("{record}\n")).collect()
}
fn assistant(text: &str) -> Item {
    Item::Assistant { text: text.into(), questions: Vec::new(), at: None }
}

#[test]
fn codex_exec_item_updates_keep_one_command_and_its_failed_output() {
    // openai/codex rust-v0.166.0: exec/src/exec_events.rs (not rollout JSON).
    let records = [
        json!({"type":"thread.started","thread_id":"thread-a"}),
        json!({"type":"item.started","item":{"id":"cmd-a","type":"command_execution","command":"cargo test","aggregated_output":"","exit_code":null,"status":"in_progress"}}),
        json!({"type":"item.updated","item":{"id":"cmd-a","type":"command_execution","command":"cargo test","aggregated_output":"checking","exit_code":null,"status":"in_progress"}}),
        json!({"type":"item.completed","item":{"id":"cmd-a","type":"command_execution","command":"cargo test","aggregated_output":"failed\n棋 ♞","exit_code":2,"status":"failed"}}),
        json!({"type":"item.completed","item":{"id":"answer-a","type":"agent_message","text":"Fix the test\nthen retry."}}),
        json!({"type":"turn.completed","usage":{"input_tokens":4,"output_tokens":5}}),
    ];
    let working = items(Some(Tool::Codex), &lines(&records[..3]));
    assert!(matches!(working.get(1), Some(Item::Result { text, error: false, .. }) if text == "checking"));
    let log = lines(&records);
    let parsed = items(Some(Tool::Codex), &log);
    assert_eq!(parsed.iter().filter(|item| matches!(item, Item::Tool { .. })).count(), 1);
    assert!(
        matches!(&parsed[0], Item::Tool { call, name, summary, .. } if call == "cmd-a" && name == "shell" && summary == "cargo test")
    );
    assert!(
        matches!(&parsed[1], Item::Result { call, text, error: true } if call == "cmd-a" && text == "failed\n棋 ♞\n(exit 2)")
    );
    assert_eq!(parsed[2], assistant("Fix the test\nthen retry."));
    assert!(matches!(parsed.last(), Some(Item::Notice { text }) if text == "Completed"));
}

#[test]
fn claude_complete_messages_keep_tool_labels_results_and_multiline_text() {
    // Claude Agent SDK stream-json assistant/user messages share transcript blocks.
    let log = lines(&[
        json!({"type":"assistant","uuid":"uuid-a","message":{"id":"msg-a","role":"assistant","content":[
            {"type":"text","text":"Checking\nthe build"},
            {"type":"tool_use","id":"tool-a","name":"Bash","input":{"command":"cargo test","description":"Run tests"}}
        ]}}),
        json!({"type":"user","uuid":"uuid-b","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-a","content":[{"type":"text","text":"passed\n✓"}],"is_error":false}]}}),
    ]);
    let parsed = items(Some(Tool::Claude), &log);
    assert_eq!(parsed[0], assistant("Checking\nthe build"));
    assert!(
        matches!(&parsed[1], Item::Tool { call, name, summary, label: Some(label), .. } if call == "tool-a" && name == "Bash" && summary == "cargo test" && label == "Run tests")
    );
    assert_eq!(parsed[2], Item::Result { call: "tool-a".into(), text: "passed\n✓".into(), error: false });
}

#[test]
fn claude_partial_blocks_accumulate_and_final_message_replaces_them() {
    // code.claude.com/docs/en/agent-sdk/streaming-output: stream_event envelopes.
    let records = [
        json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg-a"}}}),
        json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}),
        json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello\n"}}}),
        json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"世界"}}}),
    ];
    assert_eq!(items(Some(Tool::Claude), &lines(&records)), vec![assistant("Hello\n世界")]);
    let mut complete = lines(&records);
    complete.push_str(&lines(&[
        json!({"type":"assistant","uuid":"uuid-final","message":{"id":"msg-a","role":"assistant","content":[{"type":"text","text":"Hello\n世界"}]}}),
        json!({"type":"result","subtype":"success","is_error":false,"result":"Hello\n世界"}),
    ]));
    assert_eq!(
        items(Some(Tool::Claude), &complete),
        vec![assistant("Hello\n世界"), Item::Notice { text: "Completed".into() }]
    );
    let long_answer = "棋".repeat(24_010);
    let long_log = lines(&[
        json!({"type":"assistant","uuid":"long","message":{"id":"long","role":"assistant","content":[{"type":"text","text":long_answer}]}}),
        json!({"type":"result","subtype":"success","is_error":false,"result":long_answer}),
    ]);
    let parsed = items(Some(Tool::Claude), &long_log);
    assert_eq!(parsed.iter().filter(|item| matches!(item, Item::Assistant { .. })).count(), 1);
}

#[test]
fn claude_streamed_tool_arguments_become_a_shared_tool_item() {
    let log = lines(&[
        json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg-tool"}}}),
        json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-a","name":"Read","input":{}}}}),
        json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":"}}}),
        json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"notes/棋.md\"}"}}}),
        json!({"type":"stream_event","event":{"type":"content_block_stop","index":0}}),
    ]);
    assert!(
        matches!(&items(Some(Tool::Claude), &log)[0], Item::Tool { call, name, summary, .. } if call == "call-a" && name == "Read" && summary == "notes/棋.md")
    );
}

#[test]
fn accumulated_snapshots_wait_for_incomplete_json_and_preserve_diagnostics() {
    let record = json!({"type":"item.completed","item":{"id":"answer","type":"agent_message","text":"✓\n棋"}});
    let prefix = format!("warning: reconnecting\n{record}\n");
    let partial = format!("{prefix}{{\"type\":\"item.completed\",\"item\":{{\"text\":\"not ready");
    let expected = vec![Item::Notice { text: "warning: reconnecting".into() }, assistant("✓\n棋")];
    assert_eq!(items(Some(Tool::Codex), &partial), expected);
    assert_eq!(items(Some(Tool::Codex), &prefix), expected);
    assert_eq!(items(Some(Tool::Codex), prefix.trim_end()), expected);
    assert!(items(None, "{\"type\":\"item.completed\",\"item\":{").is_empty());
    let malformed_block = lines(&[
        json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"bad"}}}),
        json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":true}}),
        json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ignored"}}}),
        json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}}),
        json!({"type":"stream_event","event":{"type":"content_block_stop","index":0}}),
    ]);
    assert!(items(Some(Tool::Claude), &malformed_block).is_empty());
    let malformed = format!("{prefix}{{not json}}\n{{\"type\":\"unknown\",\"text\":\"ignore me\"}}\n");
    let parsed = items(Some(Tool::Codex), &malformed);
    assert_eq!(&parsed[..2], &expected);
    assert!(matches!(parsed.last(), Some(Item::Notice { text }) if text == "{not json}"));
}

#[test]
fn provider_selection_does_not_confuse_exec_events_with_other_json() {
    let record = json!({"type":"item.completed","item":{"id":"answer","type":"agent_message","text":"done"}});
    let text = lines(&[record]);
    assert_eq!(items(None, &text), vec![assistant("done")]);
    assert!(items(Some(Tool::Claude), &text).is_empty());
    assert_eq!(items(Some(Tool::Grok), &text), vec![Item::Notice { text: text.trim_end().into() }]);
    assert_eq!(items(None, "{\"arbitrary\":true}\n"), vec![Item::Notice { text: "{\"arbitrary\":true}".into() }]);
    let unrelated = lines(&[json!({"type":"result","value":42}), json!({"type":"error","code":12})]);
    assert_eq!(items(None, &unrelated), vec![Item::Notice { text: unrelated.trim_end().into() }]);
}

#[test]
fn terminal_errors_and_result_only_success_remain_visible() {
    let codex = lines(&[json!({"type":"turn.failed","error":{"message":"quota exhausted"}})]);
    assert_eq!(items(Some(Tool::Codex), &codex), vec![Item::Notice { text: "Failed: quota exhausted".into() }]);
    let claude = lines(&[
        json!({"type":"result","subtype":"error_during_execution","is_error":true,"errors":["permission denied","retry later"]}),
    ]);
    assert_eq!(
        items(Some(Tool::Claude), &claude),
        vec![Item::Notice { text: "Failed: permission denied\nretry later".into() }]
    );
    let result = lines(&[json!({"type":"result","subtype":"success","is_error":false,"result":"A short answer"})]);
    assert_eq!(
        items(Some(Tool::Claude), &result),
        vec![assistant("A short answer"), Item::Notice { text: "Completed".into() }]
    );
}

#[test]
fn repeated_snapshots_deduplicate_ids_but_keep_distinct_identical_answers() {
    let a = json!({"type":"item.completed","item":{"id":"a","type":"agent_message","text":"done"}});
    let b = json!({"type":"item.completed","item":{"id":"b","type":"agent_message","text":"done"}});
    assert_eq!(items(Some(Tool::Codex), &lines(&[a.clone(), a, b])), vec![assistant("done"), assistant("done")]);
}

#[test]
fn codex_tool_statuses_and_file_changes_keep_available_evidence() {
    let log = lines(&[
        json!({"type":"item.completed","item":{"id":"declined","type":"command_execution","command":"untrusted-command","aggregated_output":"Declined by policy","exit_code":null,"status":"declined"}}),
        json!({"type":"item.completed","item":{"id":"files","type":"file_change","changes":[{"path":"notes/棋.md","kind":"update"}],"status":"completed"}}),
        json!({"type":"item.completed","item":{"id":"mcp","type":"mcp_tool_call","server":"docs","tool":"search","arguments":{"query":"棋"},"result":null,"error":{"message":"Unavailable"},"status":"failed"}}),
    ]);
    let parsed = items(Some(Tool::Codex), &log);
    assert!(
        matches!(&parsed[1], Item::Result { call, text, error: true } if call == "declined" && text == "Declined by policy")
    );
    assert!(
        matches!(&parsed[2], Item::Tool { call, name, summary, diff: None, .. } if call == "files" && name == "edit" && summary == "notes/棋.md")
    );
    assert!(
        matches!(&parsed[3], Item::Tool { call, name, input, .. } if call == "mcp" && name == "mcp:docs.search" && input.contains("棋"))
    );
    assert_eq!(parsed[4], Item::Result { call: "mcp".into(), text: "Unavailable".into(), error: true });
}
