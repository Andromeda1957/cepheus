use super::*;
use crate::testsupport::TempDir;

fn roots(dir: &TempDir) -> Roots {
    Roots {
        logs: vec![(dir.path().join("claude"), Provider::Claude), (dir.path().join("codex"), Provider::Codex)],
        snapshots: vec![dir.path().join("snapshots")],
    }
}

fn append(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
}

fn assistant(id: &str, session: &str, stamp: &str, usage: &str) -> String {
    format!(
        r#"{{"type":"assistant","sessionId":"{session}","timestamp":"{stamp}","message":{{"id":"{id}","model":"claude-opus-4-5-20251101","usage":{usage}}}}}"#
    ) + "\n"
}

const USAGE: &str = r#"{"input_tokens":1000,"cache_read_input_tokens":2000,"cache_creation_input_tokens":300,"cache_creation":{"ephemeral_1h_input_tokens":100},"output_tokens":500}"#;

fn total(db: &Connection, model: &str) -> f64 {
    db.query_row("SELECT cost FROM totals WHERE model=?1", [model], |row| row.get(0)).unwrap_or(0.0)
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn the_rate_table_keeps_the_python_collectors_version() {
    assert_eq!(rates_version(), "cc865ef90d9553d720273ea13d4896efe4912dec301e88beae4b12eee6e3bf2d");
}

#[test]
fn json_and_numbers_are_written_as_python_wrote_them() {
    let value =
        serde_json::json!({"b": [1, 2.5, 1e-5, 1e16, 123456789.125], "a": "é\u{1F600}\"\n", "c": null, "d": true});
    assert_eq!(
        python_json(&value, ", ", ": "),
        r#"{"a": "\u00e9\ud83d\ude00\"\n", "b": [1, 2.5, 1e-05, 1e+16, 123456789.125], "c": null, "d": true}"#
    );
    assert_eq!(python_json(&serde_json::json!({"x": 1.0, "y": -0.0001}), ",", ":"), r#"{"x":1.0,"y":-0.0001}"#);
    assert_eq!(python_float(1794132000.123), "1794132000.123");
    assert_eq!(python_float(1e15), "1000000000000000.0");
    assert_eq!(timestamp(Some(&"2026-11-08T10:00:00.123456789Z".into())), Num::Float(1794132000.123456));
    assert_eq!(timestamp(Some(&"2026-11-08T05:00:00-05:00".into())), Num::Float(1794132000.0));
    assert_eq!(timestamp(Some(&"yesterday".into())), Num::Int(0));
    assert_eq!(timestamp(None), Num::Int(0));
}

#[test]
fn models_price_at_their_canonical_rates() {
    assert_eq!(canonical("claude-opus-4-5-20251101"), "claude-opus-4-5");
    assert_eq!(canonical("claude-sonnet-4-5-2025-09-29"), "claude-sonnet-4-5");
    assert_eq!(canonical("claude-opus-5-5[1m]"), "claude-opus-5-5");
    assert_eq!(canonical("gpt-5.6"), "gpt-5.6");
    let usage = |v: serde_json::Value| v.as_object().unwrap().clone();
    let cost = price("claude-opus-4-5", &usage(serde_json::json!({"i":1000,"r":2000,"w":200,"h":100,"o":500})));
    assert!(close(cost.unwrap(), 0.02075));
    let fast = price("claude-opus-4-5", &usage(serde_json::json!({"i":1000000,"speed":"fast","geo":"us"})));
    assert!(close(fast.unwrap(), 5.0 * 2.0 * 1.1));
    let long = price("gpt-5.6", &usage(serde_json::json!({"i":1000000,"o":1000000,"prompt":300000})));
    assert!(close(long.unwrap(), 4.0 * 2.0 + 20.0 * 1.5));
    let large = price("claude-haiku-5-5", &usage(serde_json::json!({"i":200000,"s":2})));
    assert!(close(large.unwrap(), 200000.0 * 0.5 / 1e6 + 0.02));
    assert_eq!(price("someone-else", &usage(serde_json::json!({"i":1}))), None);
}

#[test]
fn passes_read_only_appended_complete_lines_and_persist_offsets() {
    let dir = TempDir::new("api-cost-incremental");
    let roots = roots(&dir);
    let state = dir.path().join("state");
    let log = dir.path().join("claude/project/s1.jsonl");
    let first = assistant("msg_1", "s1", "2026-11-08T10:00:00Z", USAGE);
    let second = assistant("msg_2", "s1", "2026-11-08T10:00:01Z", USAGE);
    append(&log, &first);
    append(&log, &second[..20]);
    {
        let db = connect(&state).unwrap();
        assert_eq!(scan(&db, &roots, BUDGET).unwrap(), 1, "a partial last line leaves the file unread");
        assert!(close(total(&db, "claude-opus-4-5-20251101"), 0.02075));
        let offset: i64 = db.query_row("SELECT offset FROM files", [], |row| row.get(0)).unwrap();
        assert_eq!(offset, first.len() as i64);
    }
    // An earlier line changed in place is never read again: only appended
    // bytes are, from the persisted offset, by a fresh connection.
    let mut bytes = fs::read(&log).unwrap();
    let at = memchr::memmem::find(&bytes, b"\"output_tokens\":500").unwrap();
    bytes[at + 16..at + 19].copy_from_slice(b"900");
    fs::write(&log, &bytes).unwrap();
    append(&log, &second[20..]);
    let db = connect(&state).unwrap();
    assert_eq!(scan(&db, &roots, BUDGET).unwrap(), 0);
    assert!(close(total(&db, "claude-opus-4-5-20251101"), 2.0 * 0.02075));
    let entries: i64 = db.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0)).unwrap();
    assert_eq!(entries, 2);
    // Nothing new: no file is a candidate.
    assert_eq!(scan(&db, &roots, BUDGET).unwrap(), 0);
    let request: (String, f64) = db
        .query_row("SELECT session,stamp FROM claude_requests WHERE key='claude:msg_1'", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(request, ("s1".into(), 1794132000.0));
}

#[test]
fn a_replaced_file_is_read_from_its_start() {
    let dir = TempDir::new("api-cost-replaced");
    let roots = roots(&dir);
    let db = connect(&dir.path().join("state")).unwrap();
    let log = dir.path().join("claude/project/s1.jsonl");
    append(&log, &assistant("msg_1", "s1", "2026-11-08T10:00:00Z", USAGE));
    scan(&db, &roots, BUDGET).unwrap();
    // A new file takes its place: another inode, read from the start.
    let replacement = dir.path().join("claude/project/s1.next");
    append(
        &replacement,
        &(assistant("msg_9", "s1", "2026-11-08T10:00:00Z", USAGE)
            + &assistant("msg_8", "s1", "2026-11-08T10:00:00Z", USAGE)),
    );
    fs::rename(&replacement, &log).unwrap();
    scan(&db, &roots, BUDGET).unwrap();
    assert!(close(total(&db, "claude-opus-4-5-20251101"), 3.0 * 0.02075));
}

#[test]
fn streaming_snapshots_keep_the_largest_and_subagents_take_their_parent_session() {
    let dir = TempDir::new("api-cost-snapshots");
    let roots = roots(&dir);
    let db = connect(&dir.path().join("state")).unwrap();
    let log = dir.path().join("claude/project/parent/subagents/agent-1.jsonl");
    let small = r#"{"input_tokens":1000}"#;
    let large = r#"{"input_tokens":3000}"#;
    let line = |usage: &str| {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-11-08T10:00:00Z","message":{{"id":"m","model":"claude-opus-4-5","usage":{usage}}}}}"#
        ) + "\n"
    };
    append(&log, &(line(small) + &line(large) + &line(small)));
    scan(&db, &roots, BUDGET).unwrap();
    assert!(close(total(&db, "claude-opus-4-5"), 3000.0 * 5.0 / 1e6));
    let session: String = db.query_row("SELECT session FROM claude_requests", [], |row| row.get(0)).unwrap();
    assert_eq!(session, "parent");
}

#[test]
fn codex_counters_are_deltas_keyed_as_the_python_collector_keyed_them() {
    let dir = TempDir::new("api-cost-codex");
    let roots = roots(&dir);
    let db = connect(&dir.path().join("state")).unwrap();
    let total1 = r#"{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"reasoning_output_tokens":50}"#;
    let total2 = r#"{"input_tokens":3000,"cached_input_tokens":1400,"output_tokens":300,"reasoning_output_tokens":50}"#;
    let count = |stamp: &str, total: &str, last: &str| {
        format!(
            r#"{{"timestamp":"{stamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{total},"last_token_usage":{last}}}}}}}"#
        ) + "\n"
    };
    let text = [
        r#"{"timestamp":"2026-11-08T10:00:00.000Z","type":"session_meta","payload":{"id":"codex-s"}}"#.to_string() + "\n",
        r#"{"timestamp":"2026-11-08T10:00:00.000Z","type":"turn_context","payload":{"model":"gpt-5.6","service_tier":"priority"}}"#
            .to_string()
            + "\n",
        count("2026-11-08T10:00:01.000Z", total1, r#"{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100}"#),
        count("2026-11-08T10:00:02.000Z", total1, r#"{"input_tokens":1000}"#),
        count("2026-11-08T10:00:03.000Z", total2, r#"{"input_tokens":2000,"cached_input_tokens":1000,"output_tokens":200}"#),
    ]
    .concat();
    append(&dir.path().join("codex/2026/11/08/rollout.jsonl"), &text);
    scan(&db, &roots, BUDGET).unwrap();
    assert!(close(total(&db, "gpt-5.6"), 0.00912 + 0.0168));
    let entries: i64 = db.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0)).unwrap();
    assert_eq!(entries, 2, "a repeated cumulative counter adds nothing");
    let key = "codex:dbb7b2949c26a476733cff4ce124a0e1346898cd84c4b18ef3250d3bae8d9ee4";
    let usage: String = db.query_row("SELECT usage FROM entries WHERE key=?1", [key], |row| row.get(0)).unwrap();
    assert_eq!(usage, r#"{"h":0,"i":600,"o":100,"prompt":1000,"r":400,"s":0,"speed":"priority","w":0}"#);
}

#[test]
fn saved_session_totals_add_what_their_receipts_missed_once_indexing_is_done() {
    let dir = TempDir::new("api-cost-checkpoints");
    let roots = roots(&dir);
    let db = connect(&dir.path().join("state")).unwrap();
    let receipt = r#"{"type":"assistant","sessionId":"s2","timestamp":"2026-11-08T11:00:00Z","message":{"id":"m3","model":"claude-opus-4-5","usage":{"input_tokens":1000000}}}"#;
    let saved = r#"{"type":"cost-state","sessionId":"s2","totalCostUSD":7.5,"timestamp":"2026-11-08T11:00:05Z"}"#;
    append(&dir.path().join("claude/p/s2.jsonl"), &format!("{receipt}\n{saved}\n"));
    scan(&db, &roots, BUDGET).unwrap();
    let result = summary(&db).unwrap();
    assert!(close(result["models"]["claude-opus-4-5"].as_f64().unwrap(), 5.0));
    assert!(close(result["models"][SESSION_MODEL].as_f64().unwrap(), 2.5));
    assert!(close(result["total"].as_f64().unwrap(), 7.5));
    // The same total saved again later cannot move its coverage boundary.
    append(&dir.path().join("claude/p/s2.jsonl"), &format!("{}\n", saved.replace("11:00:05", "11:30:00")));
    scan(&db, &roots, BUDGET).unwrap();
    let through: f64 = db.query_row("SELECT through FROM claude_checkpoints", [], |row| row.get(0)).unwrap();
    assert_eq!(through, 1794135605.0);
    // Status-line snapshots are read as Claude records too.
    append(
        &dir.path().join("snapshots/claude-s3.json"),
        "{\"type\":\"cost-state\",\"sessionId\":\"s3\",\"totalCostUSD\":1.25,\"through\":1794135605}\n",
    );
    scan(&db, &roots, BUDGET).unwrap();
    assert!(close(summary(&db).unwrap()["total"].as_f64().unwrap(), 8.75));
}

#[test]
fn hosts_exchange_entries_without_counting_a_copied_log_twice() {
    let dir = TempDir::new("api-cost-peers");
    let peer_state = dir.path().join("peer-state");
    let line = assistant("msg_1", "s1", "2026-11-08T10:00:00Z", USAGE);
    append(&dir.path().join("claude/p/s1.jsonl"), &line);
    // The host indexes and exports what the hub asks for.
    let export = peer_export(&peer_state, &roots(&dir), "", 0).unwrap();
    assert_eq!(export.entries.len(), 1);
    assert_eq!(export.cursor, export.entries[0].3);
    assert!(!export.more);
    let again = peer_export(&peer_state, &roots(&dir), &export.instance, export.cursor).unwrap();
    assert!(again.entries.is_empty() && again.cursor == export.cursor);
    let reset = peer_export(&peer_state, &roots(&dir), "another-database", export.cursor).unwrap();
    assert_eq!(reset.entries.len(), 1, "another database starts from the beginning");

    // The hub has the same log (a handed-over session): one entry, one cost.
    let hub = TempDir::new("api-cost-hub");
    append(&hub.path().join("claude/p/s1.jsonl"), &line);
    let host = HostConfig {
        name: "fedora".into(),
        label: None,
        tag: 1,
        addresses: vec!["127.0.0.1:1".into()],
        ssh: None,
        fingerprint: String::new(),
        role: Default::default(),
    };
    let mut collector = Collector::new(hub.path().join("state"), roots(&hub));
    let asked = std::cell::RefCell::new(Vec::new());
    let result = collector
        .pass(std::slice::from_ref(&host), &mut |_, instance, cursor| {
            asked.borrow_mut().push((instance.to_string(), cursor));
            peer_export(&peer_state, &roots(&dir), instance, cursor)
        })
        .unwrap();
    assert!(close(result["total"].as_f64().unwrap(), 0.02075));
    assert_eq!(result["peers"][0]["status"], "online");
    let saved: Value = serde_json::from_slice(&fs::read(hub.path().join("state/summary.json")).unwrap()).unwrap();
    assert_eq!(saved["total"], result["total"]);
    collector
        .pass(std::slice::from_ref(&host), &mut |_, instance, cursor| {
            asked.borrow_mut().push((instance.to_string(), cursor));
            peer_export(&peer_state, &roots(&dir), instance, cursor)
        })
        .unwrap();
    let asked = asked.into_inner();
    assert_eq!(asked[0], (String::new(), 0));
    assert_eq!(asked[1], (export.instance.clone(), export.cursor), "the hub resumes from its saved cursor");

    // An unreachable host keeps its last entries and shows as offline.
    let result = collector.pass(&[host], &mut |_, _, _| bail!("unreachable")).unwrap();
    assert_eq!(result["peers"][0]["status"], "offline");
    assert!(close(result["total"].as_f64().unwrap(), 0.02075));
}

#[test]
fn a_collector_already_running_leaves_the_saved_summary() {
    let dir = TempDir::new("api-cost-locked");
    let root = dir.path().join("state");
    let mut collector = Collector::new(root.clone(), roots(&dir));
    let first = collector.pass(&[], &mut |_, _, _| unreachable!()).unwrap();
    assert_eq!(first["total"], 0.0);
    let held = Lock::try_take(&root).unwrap().unwrap();
    // flock locks belong to an open file: a second open of the file is refused.
    assert!(Lock::try_take(&root).unwrap().is_none());
    append(&dir.path().join("claude/p/s1.jsonl"), &assistant("msg_1", "s1", "2026-11-08T10:00:00Z", USAGE));
    let busy = collector.pass(&[], &mut |_, _, _| unreachable!()).unwrap();
    assert_eq!(busy["total"], first["total"]);
    drop(held);
    let done = collector.pass(&[], &mut |_, _, _| unreachable!()).unwrap();
    assert!(close(done["total"].as_f64().unwrap(), 0.02075));
}
