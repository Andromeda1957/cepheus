use super::*;

use std::fs;
use std::io::Write;

use serde_json::json;

use crate::protocol::{Status, Tool};
use crate::testsupport::TempDir;

fn w(query: &str) -> Vec<String> {
    words(query).unwrap()
}

fn user(text: &str, at: &str) -> Value {
    json!({"type": "user", "timestamp": at, "message": {"role": "user", "content": text}})
}

fn assistant(text: &str, at: &str) -> Value {
    json!({"type": "assistant", "timestamp": at, "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}})
}

fn lines(records: &[Value]) -> String {
    records.iter().map(|r| format!("{r}\n")).collect()
}

fn session(id: u64, tool: Tool, conversation: &str) -> SessionInfo {
    SessionInfo {
        spawned_ms: 1000 + id,
        account_root: None,
        metadata: Default::default(),
        id,
        tool,
        title: String::new(),
        status: Status::Idle,
        detail: String::new(),
        pid: None,
        conversation: Some(conversation.into()),
        worktree: None,
        containers: Vec::new(),
        review: false,
        alive: true,
        has_screen: true,
        created_at: 0,
        status_since: 0,
        subagents: Vec::new(),
        activity: None,
        last_active: 0,
        cwd: None,
        job: None,
        host: None,
    }
}

const CLAUDE_ID: &str = "11111111-1111-4111-8111-111111111111";
const OTHER_ID: &str = "22222222-2222-4222-8222-222222222222";
const CODEX_ID: &str = "01a19c8a-50b2-7fd0-a169-0be1ff3e0044";

/// Claude and Codex folders under `dir`, the start directory `/work/site`.
fn dirs(dir: &TempDir) -> Dirs {
    Dirs {
        claude: dir.path().join("claude"),
        codex: dir.path().join("codex"),
        opencode_db: dir.path().join("opencode.db"),
        antigravity: dir.path().join("antigravity"),
        start_dir: PathBuf::from("/work/site"),
        sqlite3: vec!["sqlite3".into()],
    }
}

fn transcript(dirs: &Dirs, id: &str) -> PathBuf {
    let path = crate::claude::transcript_path(&dirs.claude, &dirs.start_dir.to_string_lossy(), id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    path
}

fn append(path: &Path, text: &str) {
    fs::OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
}

#[test]
fn a_search_is_its_lowercased_words_once_each() {
    assert_eq!(w("  AGY   agents agy "), ["agy", "agents"]);
    assert!(words("").is_err(), "nothing to search for");
    assert!(words("   ").is_err());
    assert!(words(&"a".repeat(QUERY_MAX + 1)).is_err(), "longer than the phone may send");
    assert!(words("a b c d e f g h i").is_err(), "more words than a search has");
}

#[test]
fn every_word_must_be_in_one_message() {
    let messages = [
        (Role::User, "make the agy agents start in master", Some(1)),
        (Role::User, "agy only", Some(2)),
        (Role::Assistant, "the agents are done", Some(3)),
    ];
    let found = hit(7, messages.iter().copied(), &w("Agents AGY")).unwrap();
    assert_eq!((found.id, found.count, found.mine), (7, 1, 1));
    assert_eq!(
        found.snippets,
        [Snippet { role: Role::User, text: "make the agy agents start in master".into(), at: Some(1) }]
    );
    assert_eq!(hit(7, messages.iter().copied(), &w("agy deck")), None, "no message has both");
}

#[test]
fn the_owners_messages_come_first_then_the_newest() {
    let messages = [
        (Role::Assistant, "agy reply one", Some(1)),
        (Role::User, "agy ask one", Some(2)),
        (Role::Assistant, "agy reply two", Some(3)),
        (Role::User, "agy ask two", Some(4)),
        (Role::Assistant, "agy reply three", Some(5)),
    ];
    let found = hit(1, messages.iter().copied(), &w("agy")).unwrap();
    assert_eq!((found.count, found.mine), (5, 2));
    let shown: Vec<&str> = found.snippets.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(shown, ["agy ask two", "agy ask one", "agy reply three"]);
}

#[test]
fn a_long_message_is_cut_around_its_match_on_one_line() {
    let text = format!("{}\n\nplease add AGY here{}", "x ".repeat(200), " y".repeat(200));
    let found = hit(1, std::iter::once((Role::User, text.as_str(), None)), &w("agy")).unwrap();
    let shown = &found.snippets[0].text;
    assert!(shown.starts_with('…') && shown.ends_with('…'), "{shown}");
    assert!(shown.contains("please add AGY here"), "the match keeps its own case: {shown}");
    assert!(!shown.contains('\n'));
    assert_eq!(shown.chars().count(), SNIPPET_CHARS + 2);
    let near_end = format!("{} the end mentions agy", "z".repeat(300));
    let found = hit(1, std::iter::once((Role::User, near_end.as_str(), None)), &w("agy")).unwrap();
    let shown = &found.snippets[0].text;
    assert!(shown.starts_with('…') && shown.ends_with("mentions agy"), "a full window up to the end: {shown}");
    assert_eq!(shown.chars().count(), SNIPPET_CHARS + 1);
}

#[test]
fn letters_that_lowercase_longer_still_cut_in_the_right_place() {
    // `İ` lowercases to two characters; folding keeps it one. Counted in
    // lowercase characters, the match would sit 300 places later, past the
    // window shown.
    let text = format!("{} agy {}", "İ".repeat(300), "x".repeat(300));
    let found = hit(1, std::iter::once((Role::User, text.as_str(), None)), &w("agy")).unwrap();
    assert!(found.snippets[0].text.contains("İ agy x"), "{}", found.snippets[0].text);
}

/// The phone's chat reads only a log's last 24 MB (owner's live Claude
/// transcripts reach 160 MB); an instruction at the start must be found.
#[test]
fn an_early_instruction_in_a_long_transcript_is_found() {
    let dir = TempDir::new("search-long");
    let dirs = dirs(&dir);
    let path = transcript(&dirs, CLAUDE_ID);
    append(&path, &lines(&[user("make the agent deck start agy agents", "2026-11-01T10:00:00Z")]));
    let filler = lines(&[assistant(&"filler ".repeat(10_000), "2026-11-01T10:01:00Z")]);
    let mut big = String::new();
    while big.len() < (chat_window() + (1 << 20)) as usize {
        big.push_str(&filler);
    }
    append(&path, &big);
    let mut index = Index::default();
    let found = index.search(&[session(4, Tool::Claude, CLAUDE_ID)], &dirs, &w("agy agents"));
    assert_eq!(found.hits.len(), 1, "{found:?}");
    assert_eq!(found.hits[0].id, 4);
    assert_eq!(found.hits[0].snippets[0].text, "make the agent deck start agy agents");
    assert_eq!(found.hits[0].snippets[0].at, Some(1_793_527_200_000), "2026-11-01T10:00:00Z");
}

/// Bytes of a log the phone's chat starts from.
fn chat_window() -> u64 {
    24 << 20
}

#[test]
fn a_later_search_reads_only_what_the_log_gained() {
    let dir = TempDir::new("search-grow");
    let dirs = dirs(&dir);
    let path = transcript(&dirs, CLAUDE_ID);
    append(&path, &lines(&[user("first task about the parser", "2026-11-01T10:00:00Z")]));
    let sessions = [session(1, Tool::Claude, CLAUDE_ID)];
    let mut index = Index::default();
    assert!(index.search(&sessions, &dirs, &w("agy")).hits.is_empty());
    // Half a line is still being written: not read yet, and not lost.
    let next = lines(&[user("now the agy agents", "2026-11-01T11:00:00Z")]);
    append(&path, &next[..10]);
    assert!(index.search(&sessions, &dirs, &w("agy")).hits.is_empty());
    append(&path, &next[10..]);
    let found = index.search(&sessions, &dirs, &w("agy"));
    assert_eq!(found.hits.len(), 1);
    assert_eq!(index.search(&sessions, &dirs, &w("parser")).hits[0].count, 1, "nothing read twice");
    // Rewritten shorter: read again from the start.
    fs::write(&path, lines(&[user("only agy now", "2026-11-01T12:00:00Z")])).unwrap();
    let found = index.search(&sessions, &dirs, &w("agy"));
    assert_eq!(found.hits[0].count, 1);
    assert_eq!(found.hits[0].snippets[0].text, "only agy now");
}

#[test]
fn injected_text_tool_output_and_thinking_are_not_searched() {
    let dir = TempDir::new("search-injected");
    let dirs = dirs(&dir);
    let path = transcript(&dirs, CLAUDE_ID);
    append(
        &path,
        &lines(&[
            user("<system-reminder>agy is installed</system-reminder>", "2026-11-01T10:00:00Z"),
            json!({"type": "user", "isMeta": true, "message": {"content": "agy meta"}}),
            json!({"type": "assistant", "message": {"content": [
                {"type": "thinking", "thinking": "maybe agy"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "grep agy"}}]}}),
            json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "agy found"}]}}),
        ]),
    );
    let found = Index::default().search(&[session(1, Tool::Claude, CLAUDE_ID)], &dirs, &w("agy"));
    assert_eq!(found, Found::default());
}

#[test]
fn codex_rollouts_are_searched_and_screens_left_to_the_caller() {
    let dir = TempDir::new("search-codex");
    let dirs = dirs(&dir);
    let day = dirs.codex.join("sessions/2026/11/02");
    fs::create_dir_all(&day).unwrap();
    let rollout = day.join(format!("rollout-2026-11-02T07-54-32-{CODEX_ID}.jsonl"));
    append(
        &rollout,
        &lines(&[
            json!({"type": "session_meta", "payload": {"id": CODEX_ID, "cli_version": "0.166.0", "cwd": "/work/site"}}),
            json!({"type": "event_msg", "timestamp": "2026-11-02T07:55:00Z", "payload": {"type": "item_completed",
                "item": {"type": "UserMessage", "id": "u1", "content": [{"type": "text", "text": "Teach the deck about agy"}]}}}),
        ]),
    );
    let sessions = [
        session(1, Tool::Codex, CODEX_ID),
        session(2, Tool::Antigravity, OTHER_ID),
        SessionInfo { metadata: Default::default(), conversation: None, ..session(3, Tool::Codex, CODEX_ID) },
    ];
    let found = Index::default().search(&sessions, &dirs, &w("agy"));
    assert_eq!(found.hits.len(), 1, "{found:?}");
    assert_eq!((found.hits[0].id, found.hits[0].mine), (1, 1));
    assert_eq!(found.screen, [2, 3], "Antigravity, and a Codex not yet linked, show their screen");
}

#[test]
fn logs_of_sessions_no_longer_searched_are_let_go() {
    let dir = TempDir::new("search-forget");
    let dirs = dirs(&dir);
    for id in [CLAUDE_ID, OTHER_ID] {
        append(&transcript(&dirs, id), &lines(&[user("agy", "2026-11-01T10:00:00Z")]));
    }
    let mut index = Index::default();
    index.search(&[session(1, Tool::Claude, CLAUDE_ID), session(2, Tool::Claude, OTHER_ID)], &dirs, &w("agy"));
    assert_eq!(index.logs.len(), 2);
    let found = index.search(&[session(2, Tool::Claude, OTHER_ID)], &dirs, &w("agy"));
    assert_eq!(found.hits.iter().map(|h| h.id).collect::<Vec<_>>(), [2]);
    assert_eq!(index.logs.len(), 1, "the closed session's log is not kept");
}

#[test]
fn chat_items_search_as_messages() {
    let items = [
        Item::User { client_id: None, text: "agy one".into(), images: Vec::new(), at: Some(5) },
        Item::Tool {
            call: "c".into(),
            name: "Bash".into(),
            summary: "agy".into(),
            input: "agy".into(),
            label: None,
            diff: None,
        },
        Item::Result { call: "c".into(), text: "agy".into(), error: false },
        Item::Thinking { text: "agy".into() },
        Item::Notice { text: "agy".into() },
        Item::Assistant { text: "agy two".into(), questions: Vec::new(), at: None },
        Item::Screen { text: "agy screen".into() },
    ];
    let found = hit(9, item_messages(&items), &w("agy")).unwrap();
    assert_eq!((found.count, found.mine), (3, 1));
    let roles: Vec<Role> = found.snippets.iter().map(|s| s.role).collect();
    assert_eq!(roles, [Role::User, Role::Screen, Role::Assistant]);
}
