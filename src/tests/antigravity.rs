use super::*;
use crate::testsupport::TempDir;
use serde_json::json;

const A: &str = "20a8a689-1111-4222-8333-944445555666";
const B: &str = "ffca2fa8-1111-4222-8333-944445555666";

fn line(conversation: Option<&str>, text: &str, at_ms: u64, workspace: &str, command: bool) -> String {
    let mut v = json!({"display": text, "timestamp": at_ms, "workspace": workspace});
    if let Some(c) = conversation {
        v["conversationId"] = json!(c);
    }
    if command {
        v["type"] = json!("slash_command");
    }
    v.to_string() + "\n"
}

fn history_dir() -> TempDir {
    let dir = TempDir::new("agy");
    let text = [
        line(None, "an old prompt before conversations were recorded", 1_000_000, "/srv/repo", false),
        line(Some(B), "/model", 1_792_945_000_000, "/srv/repo", true),
        line(Some(B), "fix the\nparser", 1_792_945_174_043, "/srv/repo", false),
        line(Some(B), "/clear", 1_792_950_126_521, "/srv/repo", true),
        line(Some(A), "plan the release", 1_792_987_666_832, "/srv/repo", false),
        line(Some("elsewhere-1"), "another project", 1_792_990_000_000, "/srv/other", false),
        "not json\n".to_string(),
    ]
    .concat();
    dir.write("history.jsonl", text);
    dir
}

#[test]
fn prompts_name_their_conversation_workspace_and_time() {
    let p = parse_prompt(line(Some(A), "hi", 5, "/w", true).trim_end().as_bytes()).unwrap();
    assert_eq!(
        p,
        Prompt { conversation: A.into(), text: "hi".into(), at_ms: 5, workspace: "/w".into(), command: true }
    );
    assert_eq!(parse_prompt(line(None, "hi", 5, "/w", false).trim_end().as_bytes()), None);
    assert_eq!(parse_prompt(b"{"), None);
}

#[test]
fn conversations_are_those_of_the_start_directory_titled_with_their_first_message() {
    let dir = history_dir();
    let convs = conversations(dir.path(), Path::new("/srv/repo"), 10);
    let ids: Vec<&str> = convs.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec![A, B], "newest first, other projects left out");
    assert_eq!(convs[1].title, "fix the parser", "the first message, not the slash command before it");
    assert_eq!(convs[1].created_at, 1_792_945_000);
    assert_eq!(convs[1].updated_at, 1_792_950_126);
    assert!(convs.iter().all(|c| c.tool == Tool::Antigravity));
    assert_eq!(conversations(dir.path(), Path::new("/srv/repo"), 1).len(), 1);
    assert!(conversations(&dir.path().join("missing"), Path::new("/srv/repo"), 10).is_empty());
}

#[test]
fn a_conversation_written_since_its_last_prompt_is_dated_by_its_database() {
    let dir = history_dir();
    let db = dir.write(&format!("conversations/{B}.db"), "");
    let convs = conversations(dir.path(), Path::new("/srv/repo"), 10);
    assert_eq!(convs[0].id, B, "its database was written just now");
    assert_eq!(convs[0].updated_at, util::modified_ms(&db).unwrap() / 1000);
}

#[test]
fn the_open_database_names_the_conversation() {
    let dir = TempDir::new("agy-open");
    let base = dir.path();
    assert_eq!(conversation_of(base, &base.join(format!("conversations/{A}.db"))).as_deref(), Some(A));
    assert_eq!(conversation_of(base, &base.join(format!("conversations/{A}.db-wal"))).as_deref(), Some(A));
    assert_eq!(conversation_of(base, &base.join(format!("presence/{A}.lock"))), None);
    assert_eq!(conversation_of(base, &base.join("conversation_summaries.db")), None);
    assert_eq!(conversation_of(base, &base.join("conversations/-x.db")), None);

    let path = dir.write(&format!("conversations/{A}.db"), "");
    let _open = fs::File::open(&path).unwrap();
    assert_eq!(open_conversations(base, std::process::id()), vec![A.to_string()]);
}

#[test]
fn a_new_session_takes_the_only_unclaimed_conversation_prompted_since_it_started() {
    let dir = history_dir();
    let prompts = history(dir.path());
    let none = HashSet::new();
    assert_eq!(started_conversation(&prompts, "/srv/repo", 1_792_987_000_000, &none).as_deref(), Some(A));
    assert_eq!(
        started_conversation(&prompts, "/srv/repo", 1_792_940_000_000, &none),
        None,
        "two conversations since then: unknown"
    );
    let claimed: HashSet<String> = [B.to_string()].into();
    assert_eq!(started_conversation(&prompts, "/srv/repo", 1_792_940_000_000, &claimed).as_deref(), Some(A));
    assert_eq!(started_conversation(&prompts, "/srv/repo", 1_800_000_000_000, &none), None, "no prompt yet");
    assert_eq!(first_prompt(&prompts, B).as_deref(), Some("fix the parser"));
}

#[test]
fn a_conversation_summary_gone_idle_is_its_completed_turn() {
    // `sqlite3`'s line for conversation 86888adb-… on 2026-11-01.
    let idle = "35|CASCADE_RUN_STATUS_IDLE|0|0|2026-11-01 21:34:53.611834649+00:00\n";
    let end = summary_turn_end(idle).unwrap();
    assert_eq!((end.id.as_str(), end.at_ms, end.superseded), ("35", 1_793_568_893_611, false));
    for not_over in [
        "36|CASCADE_RUN_STATUS_RUNNING|0|0|2026-11-01 21:35:00+00:00",
        "36|CASCADE_RUN_STATUS_IDLE|1|0|2026-11-01 21:35:00+00:00",
        "36|CASCADE_RUN_STATUS_IDLE|0|1|2026-11-01 21:35:00+00:00",
        "0|CASCADE_RUN_STATUS_IDLE|0|0|0001-01-01 00:00:00+00:00",
        "",
    ] {
        assert_eq!(summary_turn_end(not_over), None, "{not_over}");
    }
    let db = TempDir::new("agy-summaries");
    assert_eq!(turn_end(&summaries_db(db.path()), A), None, "no database");
    assert_eq!(turn_end(&summaries_db(db.path()), "x' or '1'='1"), None);
}
