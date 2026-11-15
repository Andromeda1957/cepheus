use super::*;
use crate::testsupport::TempDir;
use serde_json::json;

#[test]
fn session_file_fields() {
    let v = json!({
        "pid": 1234, "sessionId": "abc", "cwd": "/w", "name": " fix\nbug ",
        "status": "busy", "kind": "interactive", "procStart": "5551", "updatedAt": 1
    });
    let p = parse_session_file(&v).unwrap();
    assert_eq!(p.pid, 1234);
    assert_eq!(p.session_id.as_deref(), Some("abc"));
    assert_eq!(p.name.as_deref(), Some("fix bug"));
    assert_eq!(p.busy, Some(true));
    assert!(p.interactive);
    assert_eq!(p.proc_start, Some(5551));
    let headless = parse_session_file(&json!({"pid": "7", "kind": "sdk", "status": "idle"})).unwrap();
    assert!(!headless.interactive);
    assert_eq!(headless.busy, Some(false));
    assert!(parse_session_file(&json!({"sessionId": "x"})).is_none());
}

#[test]
fn a_name_claude_made_from_the_folder_is_not_a_title() {
    // Owner, 2026-10-26: "the claude sessions dont get a proper title".
    // Claude Code 2.1.310 names each session "<folder>-<two hex digits>"
    // ("project-e6") with nameSource "derived" until it or the owner names
    // the conversation; that name was shown instead of Claude's own title.
    let derived = json!({"pid": 1, "name": "project-e6", "nameSource": "derived"});
    assert_eq!(parse_session_file(&derived).unwrap().name, None);
    for source in ["user", "auto", "peer", "hook", "collision"] {
        let named = json!({"pid": 1, "name": "Fix the SSD line", "nameSource": source});
        assert_eq!(parse_session_file(&named).unwrap().name.as_deref(), Some("Fix the SSD line"), "{source}");
    }
}

#[test]
fn slug_matches_claude_project_names() {
    assert_eq!(project_slug("/home/user/projects/example/app"), "-home-user-projects-example-app");
    assert_eq!(project_slug("/a/b_c.d"), "-a-b-c-d");
    assert_eq!(transcript_path(Path::new("/c"), "/w/x", "id1"), PathBuf::from("/c/projects/-w-x/id1.jsonl"));
}

fn transcript(lines: &[serde_json::Value]) -> String {
    lines.iter().map(|l| l.to_string() + "\n").collect()
}

#[test]
fn first_prompt_skips_meta_and_command_records() {
    let dir = TempDir::new("claude-prompt");
    let text = transcript(&[
        json!({"type": "summary", "summary": "x"}),
        json!({"type": "user", "isMeta": true, "message": {"content": "meta"}}),
        json!({"type": "user", "message": {"content": "<command-name>/clear</command-name>"}}),
        json!({"type": "user", "message": {"content": "Caveat: generated"}}),
        json!({"type": "assistant", "message": {"content": "hi"}}),
        json!({"type": "user", "message": {"content": [{"type": "image"}, {"type": "text", "text": "  build the\n deck "}]}}),
        json!({"type": "user", "message": {"content": "second"}}),
    ]);
    let path = dir.write("t.jsonl", text);
    assert_eq!(first_prompt(&path).as_deref(), Some("build the deck"));
}

#[test]
fn conversations_lists_newest_first_with_titles() {
    let dir = TempDir::new("claude-convs");
    let start = Path::new("/srv/repo");
    let project = format!("projects/{}", project_slug("/srv/repo"));
    let old = dir.write(
        &format!("{project}/old-id.jsonl"),
        transcript(&[json!({"type": "user", "message": {"content": "older task"}})]),
    );
    dir.write(
        &format!("{project}/new-id.jsonl"),
        transcript(&[json!({"type": "user", "message": {"content": "newer task"}})]),
    );
    dir.write(&format!("{project}/empty-id.jsonl"), "");
    dir.write("projects/-other/x.jsonl", transcript(&[json!({"type": "user", "message": {"content": "elsewhere"}})]));
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options().write(true).open(&old).unwrap().set_modified(past).unwrap();
    let convs = conversations(dir.path(), start, 10);
    let got: Vec<(&str, &str)> = convs.iter().map(|c| (c.id.as_str(), c.title.as_str())).collect();
    assert_eq!(got, vec![("new-id", "newer task"), ("old-id", "older task")]);
    assert!(convs.iter().all(|c| c.tool == Tool::Claude && c.open_in.is_none()));
    assert_eq!(conversations(dir.path(), start, 1).len(), 1);
}
