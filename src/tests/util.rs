use super::*;
use crate::testsupport::TempDir;

#[test]
fn write_atomic_replaces_with_private_file() {
    let dir = TempDir::new("util-atomic");
    let path = dir.path().join("nested/state.json");
    write_atomic(&path, b"one").unwrap();
    write_atomic(&path, b"two").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"two");
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().flatten().collect();
    assert_eq!(leftovers.len(), 1, "no temporary file is left behind");
}

#[test]
fn ensure_private_dir_sets_owner_only() {
    let dir = TempDir::new("util-private");
    let sub = dir.path().join("a/b");
    ensure_private_dir(&sub).unwrap();
    assert_eq!(fs::metadata(&sub).unwrap().permissions().mode() & 0o777, 0o700);
}

#[test]
fn read_tail_drops_partial_first_line() {
    let dir = TempDir::new("util-tail");
    let path = dir.write("f", "first line\nsecond\nthird\n");
    assert_eq!(read_tail(&path, 1000).unwrap(), b"first line\nsecond\nthird\n");
    assert_eq!(read_tail(&path, 10).unwrap(), b"third\n");
    assert_eq!(read_tail(&path, 3).unwrap(), b"");
}

#[test]
fn read_head_ends_on_line_boundary() {
    let dir = TempDir::new("util-head");
    let path = dir.write("f", "abc\ndef\nghi");
    assert_eq!(read_head(&path, 100).unwrap(), b"abc\ndef\nghi");
    assert_eq!(read_head(&path, 6).unwrap(), b"abc\n");
}

#[test]
fn read_range_returns_new_bytes_and_end() {
    let dir = TempDir::new("util-range");
    let path = dir.write("f", "0123456789");
    assert_eq!(read_range(&path, 4, 100).unwrap(), (b"456789".to_vec(), 10));
    assert_eq!(read_range(&path, 0, 3).unwrap(), (b"789".to_vec(), 10));
    assert_eq!(read_range(&path, 10, 3).unwrap(), (Vec::new(), 10));
    // A file that shrank reports its new length so callers restart from it.
    assert_eq!(read_range(&path, 50, 3).unwrap(), (Vec::new(), 10));
}

#[test]
fn base64_matches_rfc4648_vectors() {
    let cases = [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ];
    for (input, want) in cases {
        assert_eq!(base64(input.as_bytes()), want, "{input}");
        assert_eq!(unbase64(want).as_deref(), Some(input.as_bytes()), "{want}");
    }
    let bytes: Vec<u8> = (0..=255).collect();
    assert_eq!(unbase64(&base64(&bytes)), Some(bytes), "every byte value comes back");
    for bad in ["Zg=", "Z===", "Zg==Zm8=", "Zm9*", "Zm 9"] {
        assert_eq!(unbase64(bad), None, "{bad}");
    }
}

#[test]
fn uuid_v4_has_version_and_variant() {
    let a = uuid_v4().unwrap();
    let b = uuid_v4().unwrap();
    assert_ne!(a, b);
    let parts: Vec<&str> = a.split('-').collect();
    assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
    assert!(parts[2].starts_with('4'));
    assert!(matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b')));
}

#[test]
fn shell_quote_only_quotes_when_needed() {
    assert_eq!(shell_quote("/usr/bin/cepheus"), "/usr/bin/cepheus");
    assert_eq!(shell_quote("a b"), "'a b'");
    assert_eq!(shell_quote("it's"), "'it'\\''s'");
    assert_eq!(shell_quote(""), "''");
}

#[test]
fn short_duration_formats() {
    assert_eq!(short_duration(0), "0s");
    assert_eq!(short_duration(59), "59s");
    assert_eq!(short_duration(60), "1m");
    assert_eq!(short_duration(3599), "59m");
    assert_eq!(short_duration(3600 + 5 * 60), "1h05m");
    assert_eq!(short_duration(2 * 86400 + 4 * 3600), "2d04h");
}

#[test]
fn truncate_width_counts_display_columns() {
    assert_eq!(truncate_width("hello", 5), "hello");
    assert_eq!(truncate_width("hello world", 6), "hello…");
    assert_eq!(truncate_width("日本語テキスト", 7), "日本語…");
    assert_eq!(truncate_width("abc", 0), "");
}

#[test]
fn one_line_collapses_whitespace_and_controls() {
    assert_eq!(one_line("  fix\n\tthe\x07  bug  "), "fix the bug");
    assert_eq!(one_line(""), "");
}

#[test]
fn json_epoch_secs_accepts_seconds_millis_and_strings() {
    use serde_json::json;
    assert_eq!(json_epoch_secs(&json!(1_760_000_000)), Some(1_760_000_000));
    assert_eq!(json_epoch_secs(&json!(1_760_000_000_123u64)), Some(1_760_000_000));
    assert_eq!(json_epoch_secs(&json!("1760000000")), Some(1_760_000_000));
    assert_eq!(json_epoch_secs(&json!(null)), None);
    assert_eq!(json_epoch_secs(&json!(-5)), None);
}

#[test]
fn a_task_title_is_a_prompt_that_names_work() {
    // Owner, 2026-10-26: "titles dont update on clear or when given a new task".
    assert_eq!(
        task_title("[Image #24] and titles dont update on clear").as_deref(),
        Some("titles dont update on clear")
    );
    assert_eq!(task_title("also  show the\ngit commits").as_deref(), Some("show the git commits"));
    assert_eq!(task_title("merge everything to master").as_deref(), Some("merge everything to master"));
    // A reply names no task, nor does a command or a message a program wrapped in tags.
    for reply in ["continue", "yes do", "[Image #3]", "/clear now please", "<task-notification> done here", "  "] {
        assert_eq!(task_title(reply), None, "{reply:?}");
    }
    // A question is no task, unless it asks for something to be done.
    assert_eq!(task_title("status of the tests?"), None);
    assert_eq!(task_title("how do i even restart the service manually?"), None);
    assert_eq!(task_title("Can you make it send continue?").as_deref(), Some("Can you make it send continue?"));
    assert!(task_title(&"word ".repeat(80)).unwrap().ends_with('…'));
}

#[test]
fn the_last_task_prompt_is_found_by_its_line() {
    let text = b"{\"timestamp\":\"2026-10-25T10:00:00Z\",\"p\":\"fix the parser now\"}\n\
{\"timestamp\":\"2026-10-25T10:01:00Z\",\"p\":\"continue\"}\n\
{\"q\":\"no prompt here at all\"}\n\
{\"timestamp\":\"2026-10-25T10:02:00Z\",\"p\":\"merge everything to master\"}\n\
{\"timestamp\":\"2026-10-25T10:03:00Z\",\"p\":\"ok\"}\n";
    let prompt = |v: &serde_json::Value| v.get("p").and_then(serde_json::Value::as_str).map(str::to_string);
    let keep = |line: &[u8]| contains(line, b"\"p\"");
    let last = last_task_prompt(text, keep, prompt).unwrap();
    assert_eq!(last.title, "merge everything to master");
    assert_eq!(last.at_ms, crate::codex::parse_rfc3339_ms("2026-10-25T10:02:00Z").unwrap());
    assert!(text[last.pos..].starts_with(b"{\"timestamp\":\"2026-10-25T10:02"));
    assert_eq!(last_task_prompt(b"{\"p\":\"ok\"}\n", keep, prompt), None);
    // A line the filter drops is never read.
    assert_eq!(last_task_prompt(text, |_: &[u8]| false, prompt), None);
}
