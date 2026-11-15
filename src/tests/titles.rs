use super::*;
use crate::testsupport::TempDir;

const LONG: &str = "the android deck app looks really messed up, it was supposed to be based off the claude app";

/// A summarizer that prints `reply` after reading its input, which it
/// copies to `seen` when given.
fn fake(reply: &str, seen: Option<&std::path::Path>) -> Vec<String> {
    let copy = seen.map(|p| format!("cat > '{}'", p.display())).unwrap_or_else(|| "cat >/dev/null".into());
    vec!["sh".into(), "-c".into(), format!("{copy}; printf '%s\\n' '{reply}'")]
}

/// Wait for the summaries asked for to come back.
fn settle(titles: &mut Titles) -> bool {
    let started = Instant::now();
    loop {
        if titles.collect() {
            return true;
        }
        if titles.running == 0 && titles.queue.is_empty() || started.elapsed() > Duration::from_secs(10) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_reply_becomes_a_title_of_at_most_four_words() {
    assert_eq!(clean("Fix broken Android deck app\n").as_deref(), Some("Fix broken Android deck"));
    assert_eq!(clean("\"Sidebar disk readings.\"").as_deref(), Some("Sidebar disk readings"));
    assert_eq!(clean("\n\nTitle: Host picker arrows\nMore text").as_deref(), Some("Host picker arrows"));
    assert_eq!(clean("- **Deck titles**").as_deref(), Some("Deck titles"));
    assert_eq!(clean("Split\u{1b}[31m red").as_deref(), Some("Split[31m red"), "no control characters");
    assert_eq!(clean("  \n \"\" \n"), None);
    let long = clean(&"x".repeat(80)).unwrap();
    assert_eq!(long.chars().count(), MAX_CHARS);
}

#[test]
fn short_titles_stay_as_they_are_and_long_ones_get_a_summary() {
    // Owner, 2026-11-01: "titles should be a 2-4 word summary".
    let tmp = TempDir::new("titles-summary");
    let dir = tmp.path().to_path_buf();
    let seen = dir.join("seen");
    let mut titles = Titles::new(Some(fake("Fix broken Android deck app.", Some(&seen))), Some(dir.clone()));
    assert_eq!(titles.short("Install font tools"), "Install font tools");
    assert!(titles.queue.is_empty() && titles.running == 0, "nothing to summarize");
    assert_eq!(titles.short(LONG), LONG, "the long title shows until its summary comes");
    assert!(settle(&mut titles));
    assert_eq!(std::fs::read_to_string(&seen).unwrap(), LONG, "the text goes on standard input");
    assert_eq!(titles.short(LONG), "Fix broken Android deck");
    // A new service (an update in place, a restart) reads it back.
    let mut again = Titles::new(Some(fake("Something else entirely", None)), Some(dir));
    assert_eq!(again.short(LONG), "Fix broken Android deck");
    assert!(again.queue.is_empty() && again.running == 0, "not asked again");
}

#[test]
fn a_failed_summary_keeps_the_long_title_and_is_not_asked_again_at_once() {
    let tmp = TempDir::new("titles-failed");
    let command = vec!["sh".into(), "-c".into(), "cat >/dev/null; exit 3".into()];
    let mut titles = Titles::new(Some(command), Some(tmp.path().to_path_buf()));
    assert_eq!(titles.short(LONG), LONG);
    let started = Instant::now();
    while titles.running > 0 && started.elapsed() < Duration::from_secs(10) {
        titles.collect();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(titles.failed.contains_key(&key(LONG)));
    assert_eq!(titles.short(LONG), LONG);
    assert!(titles.queue.is_empty() && titles.running == 0, "a failure waits before another try");
}

#[test]
fn the_claude_summarizer_loads_no_settings_tools_or_session() {
    let titles = Titles::claude(&["claude".into()], PathBuf::from("/state"));
    let command = titles.command.unwrap();
    let flag = |name: &str| command.iter().position(|a| a == name).map(|i| command[i + 1].clone());
    assert_eq!(command[0], "claude");
    assert_eq!(flag("--model").as_deref(), Some("haiku"));
    assert_eq!(flag("--setting-sources").as_deref(), Some(""), "no hooks, plugins or CLAUDE.md");
    assert_eq!(flag("--tools").as_deref(), Some(""));
    for f in ["-p", "--strict-mcp-config", "--no-session-persistence", "--disable-slash-commands"] {
        assert!(command.iter().any(|a| a == f), "{f}");
    }
    assert_eq!(titles.dir.as_deref(), Some(std::path::Path::new("/state")), "run outside any checkout");
}
