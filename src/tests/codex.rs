use super::*;
use crate::testsupport::TempDir;
use serde_json::json;

fn jsonl(lines: &[Value]) -> String {
    lines.iter().map(|l| l.to_string() + "\n").collect()
}

fn meta_line(id: &str, cwd: &str, ts: &str) -> Value {
    json!({"timestamp": ts, "type": "session_meta", "payload": {"id": id, "cwd": cwd, "timestamp": ts}})
}

#[test]
fn rfc3339_parsing() {
    assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339_ms("2026-10-25T17:12:53.123Z"), Some(1_792_948_373_123));
    assert_eq!(parse_rfc3339_ms("2026-10-25T19:12:53.1+02:00"), Some(1_792_948_373_100));
    assert_eq!(parse_rfc3339_ms("2026-10-25T12:12:53-05:00"), Some(1_792_948_373_000));
    assert_eq!(parse_rfc3339_ms("2000-02-29T00:00:00Z"), Some(951_782_400_000));
    assert_eq!(parse_rfc3339_ms("not a date"), None);
    assert_eq!(parse_rfc3339_ms("1969-12-31T23:59:59Z"), None);
}

#[test]
fn meta_line_parsing() {
    let line = meta_line("019a-thread", "/srv/repo", "2026-10-25T17:12:53.123Z").to_string();
    let meta = parse_meta_line(line.as_bytes()).unwrap();
    assert_eq!(
        meta,
        RolloutMeta {
            id: "019a-thread".into(),
            cwd: "/srv/repo".into(),
            created_ms: 1_792_948_373_123,
            user_thread: true,
            parent: None,
            nickname: None,
            agent_path: None,
        }
    );
    assert!(parse_meta_line(br#"{"type": "event_msg", "payload": {}}"#).is_none());
    assert!(parse_meta_line(b"garbage").is_none());
}

#[test]
fn threads_codex_spawns_itself_are_not_user_threads() {
    let started = |payload: Value| {
        parse_meta_line(json!({"type": "session_meta", "payload": payload}).to_string().as_bytes()).unwrap().user_thread
    };
    assert!(started(json!({"id": "a", "source": "vscode", "thread_source": "user"})));
    assert!(started(json!({"id": "a", "source": "cli"})), "older versions record no thread_source");
    assert!(!started(
        json!({"id": "a", "source": {"subagent": {"thread_spawn": {"parent_thread_id": "p"}}}, "thread_source": "subagent"})
    ));
    assert!(!started(
        json!({"id": "a", "source": {"subagent": {"other": "guardian"}}, "thread_source": "guardian_review"})
    ));
    assert!(
        !started(json!({"id": "a", "source": {"subagent": {"thread_spawn": {}}}})),
        "older sub-agents carry only the source"
    );
}

#[test]
fn turn_state_reads_latest_turn_event() {
    let started = jsonl(&[
        json!({"type": "event_msg", "payload": {"type": "task_complete"}}),
        json!({"type": "event_msg", "payload": {"type": "task_started"}}),
        json!({"type": "response_item", "payload": {"type": "message"}}),
        json!({"type": "event_msg", "payload": {"type": "token_count"}}),
    ]);
    assert_eq!(turn_state(started.as_bytes()), Some(true));
    let aborted = started.clone() + &jsonl(&[json!({"type": "event_msg", "payload": {"type": "turn_aborted"}})]);
    assert_eq!(turn_state(aborted.as_bytes()), Some(false));
    assert_eq!(turn_state(b""), None);
}

#[test]
fn turn_end_is_the_rollouts_latest_completed_turn() {
    let event = |kind: &str, turn: &str, at: &str| json!({"timestamp": at, "type": "event_msg", "payload": {"type": kind, "turn_id": turn}});
    let done = jsonl(&[
        event("task_started", "t1", "2026-11-08T07:30:00.000Z"),
        event("task_complete", "t1", "2026-11-08T07:34:05.017Z"),
        json!({"type": "response_item", "payload": {"type": "message", "text": "task_started"}}),
        event("token_count", "", "2026-11-08T07:34:05.100Z"),
    ]);
    let end = turn_end(done.as_bytes()).unwrap();
    assert_eq!((end.id.as_str(), end.at_ms, end.superseded), ("t1", 1_794_123_245_017, false));
    // A turn begun after it, or begun and interrupted.
    for later in ["task_started", "turn_aborted"] {
        let tail = done.clone() + &jsonl(&[event(later, "t2", "2026-11-08T07:51:37.570Z")]);
        let end = turn_end(tail.as_bytes()).unwrap();
        assert_eq!((end.id.as_str(), end.superseded), ("t1", true), "{later}");
    }
    // A rollout from before turn ids is named by the time stamp.
    let old = jsonl(&[
        json!({"timestamp": "2026-10-01T10:00:00Z", "type": "event_msg", "payload": {"type": "task_complete"}}),
    ]);
    assert_eq!(turn_end(old.as_bytes()).unwrap().id, "2026-10-01T10:00:00Z");
    let running = jsonl(&[event("task_started", "t1", "2026-11-08T07:30:00.000Z")]);
    assert_eq!(turn_end(running.as_bytes()), None);
}

#[test]
fn rate_limits_from_latest_token_count() {
    let now = 1_800_000_000;
    let tail = jsonl(&[
        json!({"type": "event_msg", "payload": {"type": "token_count", "rate_limits": {"primary": {"used_percent": 10.0, "window_minutes": 10080, "resets_at": now + 100}}}}),
        json!({"type": "event_msg", "payload": {"type": "token_count", "rate_limits": {
            "primary": {"used_percent": 42.5, "window_minutes": 10080, "resets_at": now + 5000},
            "secondary": {"used_percent": 3.0, "window_minutes": 300, "resets_at": now - 10}
        }}}),
        json!({"type": "event_msg", "payload": {"type": "agent_message"}}),
    ]);
    let (at, windows) = rate_limits(tail.as_bytes(), 77_000, now).unwrap();
    assert_eq!(at, 77_000, "a record without its own time is dated by the caller");
    assert_eq!(windows.len(), 1, "an expired window is dropped");
    assert_eq!(windows[0].label, "wk");
    assert_eq!(windows[0].used_percent.unwrap(), 42.5);
    assert_eq!(windows[0].resets_at, Some(now + 5000));
    assert_eq!(windows[0].updated_at, 77);
    assert!(rate_limits(b"{}\n", 0, now).is_none());
    // Codex dates each record; that is when the sample was taken.
    let dated = jsonl(&[
        json!({"timestamp": "2026-10-25T17:12:53.123Z", "type": "event_msg", "payload": {"type": "token_count", "rate_limits": {"primary": {"used_percent": 5.0, "window_minutes": 10080}}}}),
    ]);
    let (at, windows) = rate_limits(dated.as_bytes(), 77_000, now).unwrap();
    assert_eq!((at, windows[0].updated_at), (1_792_948_373_123, 1_792_948_373));
}

#[test]
fn first_user_message_skips_injected_context() {
    let dir = TempDir::new("codex-first");
    let path = dir.write(
        "r.jsonl",
        jsonl(&[
            meta_line("t1", "/srv", "2026-10-25T00:00:00Z"),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "# AGENTS.md instructions"}]}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>x</environment_context>"}]}}),
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": "  wire up\nthe deck  "}}),
        ]),
    );
    assert_eq!(first_user_message(&path).as_deref(), Some("wire up the deck"));
}

#[test]
fn thread_names_keep_latest_entry() {
    let dir = TempDir::new("codex-names");
    dir.write(
        "session_index.jsonl",
        jsonl(&[
            json!({"id": "a", "thread_name": "first name", "updated_at": "2026-10-25T10:00:00Z"}),
            json!({"id": "a", "thread_name": "renamed", "updated_at": "2026-10-25T11:00:00Z"}),
            json!({"id": "b", "thread_name": "  "}),
            json!({"id": "c", "thread_name": "undated"}),
        ]),
    );
    let names = thread_names(dir.path());
    assert_eq!(names.get("a").map(|n| n.name.as_str()), Some("renamed"));
    assert_eq!(names["a"].at_ms, parse_rfc3339_ms("2026-10-25T11:00:00Z"));
    assert!(!names.contains_key("b"));
    // A name with no time is never outdated by a task.
    let task = crate::util::TaskPrompt { title: "a later task".into(), at_ms: u64::MAX, pos: 0 };
    assert!(names["a"].outdated_by(&task));
    assert!(!names["c"].outdated_by(&task));
}

#[test]
fn rollout_discovery_and_conversations() {
    let dir = TempDir::new("codex-rollouts");
    let older = dir.write(
        "sessions/2026/10/24/rollout-2026-10-24T10-00-00-aaaa.jsonl",
        jsonl(&[
            meta_line("aaaa", "/srv/repo", "2026-10-24T10:00:00Z"),
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": "older work"}}),
        ]),
    );
    dir.write(
        "sessions/2026/10/25/rollout-2026-10-25T10-00-00-bbbb.jsonl",
        jsonl(&[
            meta_line("bbbb", "/srv/repo", "2026-10-25T10:00:00Z"),
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": "newer work"}}),
        ]),
    );
    dir.write(
        "sessions/2026/10/25/rollout-2026-10-25T11-00-00-cccc.jsonl",
        jsonl(&[meta_line("cccc", "/elsewhere", "2026-10-25T11:00:00Z")]),
    );
    dir.write(
        "sessions/2026/10/25/rollout-2026-10-25T10-30-00-dddd.jsonl",
        jsonl(&[
            json!({"type": "session_meta", "payload": {"id": "dddd", "cwd": "/srv/repo", "timestamp": "2026-10-25T10:30:00Z", "thread_source": "subagent"}}),
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": "sub-agent brief"}}),
        ]),
    );
    dir.write("sessions/2026/10/25/notes.txt", "ignored");
    dir.write("session_index.jsonl", jsonl(&[json!({"id": "bbbb", "thread_name": "Named thread"})]));
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    fs::File::options().write(true).open(&older).unwrap().set_modified(past).unwrap();

    let days = day_dirs(dir.path(), 10);
    assert!(days[0].ends_with("2026/10/25") && days[1].ends_with("2026/10/24"));
    assert_eq!(day_dirs(dir.path(), 1).len(), 1);
    assert_eq!(rollouts_in(&days).len(), 4);
    assert_eq!(find_rollout(dir.path(), "aaaa"), Some(older.clone()));
    assert_eq!(find_rollout(dir.path(), "zzzz"), None);
    assert_eq!(user_rollout(dir.path(), "aaaa"), Some(older.clone()));
    assert_eq!(user_rollout(dir.path(), "dddd"), None, "a sub-agent thread is not a conversation");
    assert_eq!(user_rollout(dir.path(), "zzzz"), None, "a temporary thread writes no rollout");

    let convs = conversations(dir.path(), Path::new("/srv/repo"), 10);
    let got: Vec<(&str, &str)> = convs.iter().map(|c| (c.id.as_str(), c.title.as_str())).collect();
    assert_eq!(
        got,
        vec![("bbbb", "Named thread"), ("aaaa", "older work")],
        "sub-agent threads are not offered for resume"
    );
    assert_eq!(convs[1].created_at, 1_792_836_000);
    assert!(convs.iter().all(|c| c.tool == Tool::Codex));
}

#[test]
fn a_conversation_resumed_since_is_offered_from_any_day() {
    // Rollouts are filed by the day their conversation began. The picker
    // lists the conversations written most recently, but read only the 30
    // newest day folders, so one begun earlier and resumed since was missing
    // (the dated-folder cutoff of review CR-20261026-d5c080aeb2).
    let dir = TempDir::new("codex-resumed");
    for day in 1..=31 {
        let date = format!("2026-09-{day:02}");
        let path = format!("sessions/2026/09/{day:02}/rollout-{date}T10-00-00-day{day}.jsonl");
        let text = jsonl(&[
            meta_line(&format!("day{day}"), "/srv/repo", &format!("{date}T10:00:00Z")),
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": format!("work of {date}")}}),
        ]);
        let written = dir.write(&path, text);
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400 * (40 - day));
        fs::File::options().write(true).open(&written).unwrap().set_modified(past).unwrap();
    }
    let resumed = dir.write(
        "sessions/2026/08/20/rollout-2026-08-20T10-00-00-old.jsonl",
        jsonl(&[
            meta_line("old", "/srv/repo", "2026-08-20T10:00:00Z"),
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": "resumed work"}}),
        ]),
    );
    let convs = conversations(dir.path(), Path::new("/srv/repo"), 3);
    let ids: Vec<&str> = convs.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["old", "day31", "day30"]);
    assert_eq!(convs[0].updated_at, util::modified_ms(&resumed).unwrap() / 1000);
}

#[test]
fn lock_holders_are_read_from_the_kernel_table() {
    let table = "1: POSIX  ADVISORY  READ 3583467 00:1e:63314853 128 128\n\
                 2: FLOCK  ADVISORY  WRITE 3581105 00:1e:23526049 0 EOF\n\
                 2: -> FLOCK  ADVISORY  WRITE 4000 00:1e:23526049 0 EOF\n\
                 3: FLOCK  ADVISORY  WRITE -1 00:2b:23526049 0 EOF\n";
    assert_eq!(lock_holders(table, 23526049), vec![Some(3581105), None]);
    assert_eq!(lock_holders(table, 63314853), Vec::<Option<u32>>::new(), "a POSIX record lock is not Codex's");
    assert!(lock_holders("", 1).is_empty());
}

#[test]
fn a_thread_is_locked_only_while_a_process_holds_its_writer_lock() {
    // Codex's shared server keeps this lock after its client exits, and an
    // embedded Codex resuming the thread shows "This conversation is open in
    // another app" (owner screenshot, 2026-10-26).
    let dir = TempDir::new("codex-lock");
    assert!(!thread_locked(dir.path(), "019a-thread"), "no lock file");
    let path = dir.write("thread-writer-locks/019a-thread.lock", "");
    assert!(!thread_locked(dir.path(), "019a-thread"), "a lock file nobody holds");
    let file = fs::File::open(&path).unwrap();
    file.try_lock().unwrap();
    assert!(thread_locked(dir.path(), "019a-thread"));
    assert!(!thread_locked(dir.path(), "019a-other"));
    drop(file);
    assert!(!thread_locked(dir.path(), "019a-thread"), "released when its holder lets go");
}

#[test]
fn active_goals_are_read_only_and_exclude_inactive_statuses() {
    let dir = TempDir::new("codex-goals");
    let db = dir.path().join("goals_1.sqlite");
    let made = std::process::Command::new("python3")
        .args(["-I", "-c", "import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); c.execute('CREATE TABLE thread_goals (thread_id TEXT PRIMARY KEY, status TEXT, objective TEXT)'); c.executemany('INSERT INTO thread_goals VALUES (?, ?, ?)', [(s, s, 'private objective') for s in ['active', 'paused', 'blocked', 'usage_limited', 'budget_limited', 'complete']]); c.commit()"])
        .arg(&db).status().unwrap();
    assert!(made.success());
    let reader = dir.write(
        "sqlite3.py",
        r#"import sqlite3, sys
if sys.argv[1:8] != ['-init', '/dev/null', '-readonly', '-bail', '-noheader', '-cmd', '.timeout 100'] or len(sys.argv) != 10:
    sys.exit(2)
c = sqlite3.connect('file:' + sys.argv[8] + '?mode=ro', uri=True)
for row in c.execute(sys.argv[9]):
    print(row[0])
"#,
    );
    let read = || active_goals_by("python3", &["-I", reader.to_str().unwrap()], dir.path());
    let before = fs::read(&db).unwrap();
    assert_eq!(read(), Some(HashSet::from(["active".into()])));
    assert_eq!(fs::read(&db).unwrap(), before, "goal data must remain unchanged");
    let changed = std::process::Command::new("python3")
        .args(["-I", "-c", "import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); c.execute(\"UPDATE thread_goals SET status = 'complete'\"); c.commit()"])
        .arg(&db).status().unwrap();
    assert!(changed.success());
    assert_eq!(read(), Some(HashSet::new()), "a known inactive goal is distinct from an unavailable database");
    assert_eq!(active_goals_by("missing-goal-reader", &[], dir.path()), None);
    let empty = TempDir::new("codex-no-goals");
    assert_eq!(active_goals(empty.path()), None);
    assert!(!empty.path().join("goals_1.sqlite").exists(), "never create Codex's database");
}
