use super::*;
use crate::testsupport::TempDir;

#[test]
fn an_opencode_session_id_has_opencodes_own_form() {
    // opencode's newest session on 2026-10-25 was `ses_ead42ac29ffei5Xcr4QiFwE9uu`,
    // created at 1792389502021 ms: the time, times 4096, bits inverted.
    let id = opencode_session_id(1792389502021, &[0; 14]);
    assert_eq!(id, "ses_ead42abbafff00000000000000");
    assert!(id[..12].starts_with("ses_ead42a"), "{id}");
    let tail = opencode_session_id(1792389502021, &[61, 62, 10, 36, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255]);
    assert!(tail.ends_with("z0Aa0000000007"), "{tail}");
    // A later session sorts first, as opencode lists them.
    assert!(opencode_session_id(1792389502022, &[0; 14]) < id);
    assert!(is_opencode_id(&id) && is_opencode_id(&new_opencode_session_id().unwrap()));
    for bad in ["", "ses_", "ses_a'b", "ses_a b", "x_abc", "../ses_a"] {
        assert!(!is_opencode_id(bad), "{bad:?}");
    }
}

#[test]
fn a_grok_title_is_the_one_grok_generated() {
    let home = TempDir::new("grok-home");
    let id = "0b4f8a2e-1c3d-4e5f-8a9b-0c1d2e3f4a5b";
    home.write(
        &format!("sessions/%2Fsrv%2Frepo/{id}/summary.json"),
        r#"{"info": {"id": "x"}, "session_summary": "Earlier summary", "generated_title": "Port the parser\nto Rust"}"#,
    );
    assert_eq!(grok_title(home.path(), id).as_deref(), Some("Port the parser to Rust"));
    // Before Grok generates a title, its summary; another conversation has none.
    let other = "1b4f8a2e-1c3d-4e5f-8a9b-0c1d2e3f4a5b";
    home.write(
        &format!("sessions/%2Fsrv%2Fother/{other}/summary.json"),
        r#"{"session_summary": "Fix the build", "generated_title": ""}"#,
    );
    assert_eq!(grok_title(home.path(), other).as_deref(), Some("Fix the build"));
    assert_eq!(grok_title(home.path(), "2b4f8a2e-1c3d-4e5f-8a9b-0c1d2e3f4a5b"), None);
    // An id that is not a UUID never becomes part of a path.
    assert_eq!(grok_title(home.path(), "../../etc"), None);
}

/// Grok 1.0.70's `events.jsonl` for one turn, as in its session folders.
fn grok_turn(start: &str, end: &str, outcome: &str) -> String {
    let phases = "{\"ts\":\"x\",\"type\":\"phase_changed\",\"phase\":\"streaming_text\"}\n".repeat(50);
    format!(
        "{{\"ts\":\"{start}\",\"type\":\"turn_started\",\"turn_number\":0}}\n{phases}\
         {{\"ts\":\"{end}\",\"type\":\"turn_ended\",\"outcome\":\"{outcome}\"}}\n"
    )
}

#[test]
fn groks_completed_turn_is_its_latest_turn_ended_completed() {
    let home = TempDir::new("grok-events");
    let id = "01a157f0-ee7a-7cd3-8d82-aad3603dd763";
    home.write(&format!("sessions/%2Fsrv%2Frepo/{id}/summary.json"), "{}");
    assert_eq!(grok_events(home.path(), id), None, "no record yet");
    home.write(
        &format!("sessions/%2Fsrv%2Frepo/{id}/events.jsonl"),
        &grok_turn("2026-11-01T10:05:00.000Z", "2026-11-01T10:05:04.000Z", "completed"),
    );
    let path = grok_events(home.path(), id).unwrap();
    let end = grok_turn_end(&fs::read(&path).unwrap()).unwrap();
    assert_eq!((end.id.as_str(), end.at_ms, end.superseded), ("2026-11-01T10:05:04.000Z", 1_793_527_504_000, false));
    // A turn begun after it, or one that ended in an error, supersedes it.
    let first = grok_turn("2026-11-01T10:05:00.000Z", "2026-11-01T10:05:04.000Z", "completed");
    let failed = first.clone() + &grok_turn("2026-11-01T10:06:00.000Z", "2026-11-01T10:06:01.000Z", "error");
    let end = grok_turn_end(failed.as_bytes()).unwrap();
    assert_eq!((end.id.as_str(), end.superseded), ("2026-11-01T10:05:04.000Z", true));
    let running = first + "{\"ts\":\"2026-11-01T10:07:00.000Z\",\"type\":\"turn_started\",\"turn_number\":1}\n";
    assert!(grok_turn_end(running.as_bytes()).unwrap().superseded);
    assert_eq!(grok_turn_end(grok_turn("a", "b", "error").as_bytes()), None);
    assert_eq!(grok_events(home.path(), "../../etc"), None);
}

#[test]
fn opencodes_completed_turn_is_read_from_its_succeeded_idle_message() {
    // `sqlite3`'s line for opencode's session `ses_ead44b35fffemvet0fDrfr3dHL`
    // on 2026-10-25: message 70, idle, succeeded, nothing from the person after.
    let end = opencode_turn_row("70|1792389420651|0\n").unwrap();
    assert_eq!((end.id.as_str(), end.at_ms, end.superseded), ("70", 1_792_389_420_651, false));
    assert!(opencode_turn_row("70|1792389420651|1").unwrap().superseded, "a message after it");
    assert_eq!(opencode_turn_row(""), None, "no succeeded idle message");
    assert_eq!(opencode_turn_end(Path::new("/nonexistent/opencode.db"), "ses_ead44b35fffemvet0fDrfr3dHL"), None);
}

/// One `prompt_history.jsonl` line as Grok 1.0.70 writes it.
fn grok_prompt(id: &str, at: &str, prompt: &str, bash: bool) -> String {
    format!("{}\n", serde_json::json!({"timestamp": at, "session_id": id, "prompt": prompt, "is_bash": bash}))
}

#[test]
fn grok_conversations_are_the_prompted_ones_of_the_start_directory() {
    // Owner, 2026-10-31: "resume a existing session either claude/codex/agy
    // etc where you pick from a list of sessions". Grok keeps a folder for
    // every start, prompted or not (11 of the owner's 12 were empty).
    let home = TempDir::new("grok-list");
    let folder = "sessions/%2Fsrv%2Frepo";
    let (old, new, empty) = (
        "01a0e9dc-b21a-7ca2-b615-1624ec790b18",
        "01a16bf6-a28a-7ce3-b29d-e722992ee7d8",
        "01a157fd-928e-70a1-bafb-453b68404744",
    );
    home.write(
        &format!("{folder}/prompt_history.jsonl"),
        "{\"cut off by the tail\n".to_string()
            + &grok_prompt(old, "2026-09-28T21:12:53.733638045Z", "ls -la", true)
            + &grok_prompt(old, "2026-09-28T21:12:54Z", "Port the\nparser", false)
            + &grok_prompt(new, "2026-10-24T03:31:30Z", "Fix the build", false)
            + &grok_prompt("../../etc", "2026-10-24T03:31:30Z", "not an id", false),
    );
    home.write(&format!("{folder}/{new}/summary.json"), r#"{"generated_title": "Build fixes", "session_summary": ""}"#);
    home.write(&format!("{folder}/{empty}/summary.json"), r#"{"generated_title": null, "session_summary": ""}"#);
    home.write(
        "sessions/%2Fsrv%2Fother/prompt_history.jsonl",
        grok_prompt("11111111-2222-3333-4444-555555555555", "2026-10-30T00:00:00Z", "elsewhere", false),
    );
    let list = grok_conversations(home.path(), Path::new("/srv/repo/"), 10);
    let got: Vec<(&str, &str, u64, u64)> =
        list.iter().map(|c| (c.id.as_str(), c.title.as_str(), c.created_at, c.updated_at)).collect();
    assert_eq!(
        got,
        vec![(new, "Build fixes", 1792812690, 1792812690), (old, "Port the parser", 1790629973, 1790629974),],
        "Grok's own title, else the first prompt that is not a shell command"
    );
    assert!(list.iter().all(|c| c.tool == Tool::Grok && c.open_in.is_none()));
    assert_eq!(grok_conversations(home.path(), Path::new("/srv/repo"), 1).len(), 1);
    assert!(grok_conversations(home.path(), Path::new("/srv/none"), 10).is_empty());
    assert!(grok_conversations(&home.path().join("missing"), Path::new("/srv/repo"), 10).is_empty());
}

#[test]
fn opencode_conversations_are_the_start_directorys_own_sessions() {
    let dir = TempDir::new("opencode-list");
    let db = dir.path().join("opencode.db");
    let made = Command::new("python3")
        .args(["-I", "-c", "import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); c.executescript(sys.argv[2]); c.commit()"])
        .arg(&db)
        .arg(
            "create table session_v2 (id text primary key, directory text, title text, parent_id text, \
             time_created integer, time_updated integer, time_archived integer); \
             insert into session_v2 values ('ses_ead42ac29ffei5Xcr4QiFwE9uu', '/srv/repo', 'Compare the two parsers', null, 1792389502021, 1792389610866, null); \
             insert into session_v2 values ('ses_ead44b35fffemvet0fDrfr3dHL', '/srv/repo', 'New session - 2026-10-25T10:00:00.000Z', null, 1792389369132, 1792389420624, null); \
             insert into session_v2 values ('ses_ead45b35fffemvet0fDrfr3dHL', '/srv/repo', 'A subagent', 'ses_ead42ac29ffei5Xcr4QiFwE9uu', 1792389369132, 1792389700000, null); \
             insert into session_v2 values ('ses_ead46b35fffemvet0fDrfr3dHL', '/srv/repo', 'Archived', null, 1792389369132, 1792389800000, 1792389900000); \
             insert into session_v2 values ('ses_ead47b35fffemvet0fDrfr3dHL', '/srv/other', 'Elsewhere', null, 1792389369132, 1792389900000, null); \
             insert into session_v2 values ('x'' or ''1''=''1', '/srv/repo', 'Not an id', null, 1, 1792389950000, null);",
        )
        .status()
        .unwrap();
    assert!(made.success());
    // A stand-in for the `sqlite3` program (the test container has none)
    // that accepts only the read-only JSON call the deck makes.
    let script = dir.write(
        "sqlite3.py",
        "import json, sqlite3, sys\n\
         if sys.argv[1:6] != ['-readonly', '-bail', '-json', '-cmd', '.timeout 500'] or len(sys.argv) != 8:\n    sys.exit(2)\n\
         con = sqlite3.connect('file:' + sys.argv[6] + '?mode=ro', uri=True)\n\
         cur = con.execute(sys.argv[7])\n\
         names = [d[0] for d in cur.description]\n\
         rows = [dict(zip(names, row)) for row in cur]\n\
         print(json.dumps(rows) if rows else '', end='')\n",
    );
    let reader = [OsStr::new("python3"), OsStr::new("-I"), script.as_os_str()];
    let list = opencode_conversations_by(&reader, &db, Path::new("/srv/repo"), 10);
    let got: Vec<(&str, &str, u64, u64)> =
        list.iter().map(|c| (c.id.as_str(), c.title.as_str(), c.created_at, c.updated_at)).collect();
    assert_eq!(
        got,
        vec![
            ("ses_ead42ac29ffei5Xcr4QiFwE9uu", "Compare the two parsers", 1792389502, 1792389610),
            ("ses_ead44b35fffemvet0fDrfr3dHL", "opencode", 1792389369, 1792389420),
        ],
        "no subagent, archived, other folder's or malformed session; the placeholder is no title"
    );
    assert!(list.iter().all(|c| c.tool == Tool::OpenCode));
    assert_eq!(opencode_conversations_by(&reader, &db, Path::new("/srv/repo"), 1).len(), 1);
    assert!(opencode_conversations_by(&reader, &db, Path::new("/srv/none"), 10).is_empty(), "no rows print nothing");
    assert!(opencode_conversations_by(&reader, &dir.path().join("missing.db"), Path::new("/srv/repo"), 10).is_empty());
    let none = dir.path().join("none");
    assert!(opencode_conversations_by(&[none.as_os_str()], &db, Path::new("/srv/repo"), 10).is_empty());
}

#[test]
fn an_opencode_title_comes_from_its_database() {
    // A real SQLite database in opencode's form, read by a stand-in for the
    // `sqlite3` program that runs the deck's query with Python's SQLite (the
    // test container has no `sqlite3` program). The stand-in accepts only the
    // read-only call the deck makes, so a write or a changed call fails here.
    let dir = TempDir::new("opencode-db");
    let db = dir.path().join("opencode.db");
    let made = Command::new("python3")
        .args(["-I", "-c", "import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); c.executescript(sys.argv[2]); c.commit()"])
        .arg(&db)
        .arg(
            "create table session_v2 (id text primary key, title text, time_created integer); \
             insert into session_v2 values ('ses_ead42ac29ffei5Xcr4QiFwE9uu', 'Compare the two parsers', 1); \
             insert into session_v2 values ('ses_ead44b35fffemvet0fDrfr3dHL', 'New session - 2026-10-25T10:00:00.000Z', 2); \
             insert into session_v2 values ('ses_ead45b35fffemvet0fDrfr3dHL', 'Child session - 2026-10-25T10:01:00.000Z', 3);",
        )
        .status()
        .unwrap();
    assert!(made.success());
    let script = dir.write(
        "sqlite3.py",
        "import sqlite3, sys\n\
         if sys.argv[1:5] != ['-readonly', '-bail', '-cmd', '.timeout 500'] or len(sys.argv) != 7:\n    sys.exit(2)\n\
         con = sqlite3.connect('file:' + sys.argv[5] + '?mode=ro', uri=True)\n\
         for row in con.execute(sys.argv[6]):\n    print('|'.join(str(v) for v in row))\n",
    );
    let reader = [OsStr::new("python3"), OsStr::new("-I"), script.as_os_str()];
    let title = |db: &Path, id: &str| opencode_title_by(&reader, db, id);
    assert_eq!(title(&db, "ses_ead42ac29ffei5Xcr4QiFwE9uu").as_deref(), Some("Compare the two parsers"));
    assert_eq!(title(&db, "ses_ead44b35fffemvet0fDrfr3dHL"), None, "opencode's placeholder is no title");
    assert_eq!(title(&db, "ses_ead45b35fffemvet0fDrfr3dHL"), None, "nor is a subagent's");
    assert_eq!(title(&db, "ses_unknown"), None);
    assert_eq!(title(&db, "ses_x' or '1'='1"), None, "never a query from an id that is not one");
    assert_eq!(title(&dir.path().join("missing.db"), "ses_ead42ac29ffei5Xcr4QiFwE9uu"), None);
    // Without a `sqlite3` program there is simply no title.
    let none = dir.path().join("none");
    assert_eq!(opencode_title_by(&[none.as_os_str()], &db, "ses_ead42ac29ffei5Xcr4QiFwE9uu"), None);
}
