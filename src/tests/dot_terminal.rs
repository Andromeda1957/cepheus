use super::*;
use serde_json::json;

fn screen(terminal: &Terminal, columns: usize, rows: usize) -> Vec<String> {
    let drawn = terminal.render(columns, rows);
    drawn.strip_prefix("\x1b[2J\x1b[H").unwrap().split("\r\n").map(String::from).collect()
}

#[test]
fn the_terminal_screen_wraps_between_words_and_shows_only_failures() {
    let mut terminal = Terminal::default();
    terminal.messages = vec![
        ("user".into(), "please summarise the meeting notes".into()),
        ("assistant".into(), "Sure, here is a short summary of everything".into()),
    ];
    let rows = screen(&terminal, 20, 30);
    assert!(rows.iter().all(|row| row.chars().count() <= 20), "{rows:#?}");
    assert_eq!(
        rows,
        [
            "Dot | /reconnect",
            "/resolve /quit",
            "",
            "You: please",
            "     summarise the",
            "     meeting notes",
            "",
            "Dot: Sure, here is a",
            "     short summary",
            "     of everything",
            "",
            "› ",
        ]
    );
    terminal.failure = Some("Cannot connect to Dot; retrying. Nothing is resent.".into());
    let rows = screen(&terminal, 20, 6);
    assert_eq!(rows, ["     of everything", "", "Cannot connect to", "Dot; retrying.", "Nothing is resent.", "› "]);
    let all = terminal.render(80, 30).to_lowercase();
    assert!(!all.contains("ready") && !all.contains("working") && !all.contains("needs attention"), "{all}");
}

#[test]
fn only_a_failed_turn_counts_as_a_failure() {
    use super::super::bridge::activity_failure;
    assert_eq!(activity_failure(&json!({"activity_label": "Turn failed"})).as_deref(), Some("Turn failed in ChatGPT."));
    for label in ["Working", "Needs input", "Paused", "Idle"] {
        assert_eq!(activity_failure(&json!({"activity_label": label, "working": true})), None, "{label}");
    }
    assert_eq!(activity_failure(&json!({"activity_known": false})), None);
}

#[test]
fn the_composer_is_busy_from_the_first_key_until_the_message_is_sent() {
    let mut terminal = Terminal::default();
    assert!(!terminal.composing());
    terminal.feed("hi");
    assert!(terminal.composing());
    terminal.feed("\r");
    assert!(!terminal.composing(), "sent");
    terminal.feed("\x1b[200~line one\n");
    assert!(terminal.composing(), "mid-paste");
    terminal.feed("\x1b[201~\x15");
    assert!(!terminal.composing(), "cleared");
    terminal.feed("\x1b[");
    assert!(terminal.composing(), "a key sequence in progress");
}

#[test]
fn the_owners_terminal_settings_survive_the_switch_to_a_newer_build() {
    // SAFETY: an all-zero termios is a valid value.
    let mut settings: libc::termios = unsafe { std::mem::zeroed() };
    settings.c_iflag = 0o2405;
    settings.c_lflag = 0o105073;
    settings.c_cflag = 0o277;
    settings.c_line = 3;
    settings.c_cc[libc::VINTR] = 3;
    settings.c_cc[libc::VMIN] = 1;
    settings.c_ispeed = 15;
    settings.c_ospeed = 15;
    let text = encode_settings(&settings);
    let back = decode_settings(&text).unwrap();
    assert_eq!(encode_settings(&back), text);
    assert_eq!((back.c_iflag, back.c_lflag, back.c_cc[libc::VINTR], back.c_ospeed), (0o2405, 0o105073, 3, 15));
    for broken in ["", "1 2 3", &format!("{text} 9"), &text.replace(' ', "x")] {
        assert!(decode_settings(broken).is_none(), "{broken:?}");
    }
}
