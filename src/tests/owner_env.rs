use super::*;

fn vars(list: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    list.iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect()
}

fn marker_of(list: &[(&str, &str)]) -> Option<&'static str> {
    let list = vars(list);
    agent_marker(list.iter().map(|(name, value)| (name.as_os_str(), value.as_os_str())))
}

#[test]
fn agent_tool_environments_are_recognized_and_the_owners_is_not() {
    // Names seen in the service a Codex session started on 2026-11-07, which
    // itself ran under a Claude Code session, and in the deck's own sessions.
    for marker in ["CLAUDECODE", "AI_AGENT", "CODEX_THREAD_ID", "CODEX_CI", "CEPHEUS_SESSION", "AGENT_DECK_SESSION"] {
        // legacy-name
        assert_eq!(marker_of(&[("HOME", "/home/o"), (marker, "1")]), Some(marker));
    }
    // The owner's terminal: its own configuration, terminal and colour
    // settings are not an agent's.
    let owner = [
        ("HOME", "/home/o"),
        ("PATH", "/usr/bin"),
        ("CLAUDE_CONFIG_DIR", "/home/o/.claude-work"),
        ("CODEX_HOME", "/home/o/.codex"),
        ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000"),
        ("KITTY_WINDOW_ID", "3"),
        ("TERM", "xterm-kitty"),
        ("NO_COLOR", "1"),
        ("CEPHEUS_STATE_DIR", "/home/o/.local/state/agent-deck"), // legacy-name
    ];
    assert_eq!(marker_of(&owner), None);
    assert_eq!(marker_of(&[("CLAUDECODE", "")]), None, "an empty variable is not set");
}

#[test]
fn an_agent_started_service_takes_the_owners_session_and_keeps_only_the_decks_addresses() {
    let agent = vars(&[
        ("CODEX_THREAD_ID", "t"),
        ("CODEX_CI", "1"),
        ("CLAUDECODE", "1"),
        ("NO_COLOR", "1"),
        ("TERM", "dumb"),
        ("COLORTERM", ""),
        ("LANG", "C.UTF-8"),
        ("LC_ALL", "C.UTF-8"),
        ("PAGER", "cat"),
        ("CLAUDE_CONFIG_DIR", "/nonexistent/agent-guard"),
        ("TMPDIR", "/proc/1/fd/3"),
        ("PATH", "/home/o/.codex/tmp/arg0:/usr/bin"),
        ("HOME", "/home/o"),
        ("CEPHEUS_STATE_DIR", "/home/o/.local/state/agent-deck"), // legacy-name
        ("AGENT_DECK_STATE_DIR", "/home/o/.local/state/agent-deck"), // legacy-name
        ("CEPHEUS_SOCKET", "/run/user/1/agent-deck/deck.sock"),   // legacy-name
        ("CEPHEUS_START_DIR", "/home/o/work"),
        ("CEPHEUS_SESSION", "7"),
    ]);
    let session = vars(&[
        ("HOME", "/home/o"),
        ("PATH", "/home/o/.local/bin:/usr/bin"),
        ("LANG", "en_US.UTF-8"),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1/bus"),
        ("CEPHEUS_STATE_DIR", "/elsewhere"),
    ]);
    let mut got: Vec<(String, String)> = owner_environment(&agent, session)
        .into_iter()
        .map(|(name, value)| (name.into_string().unwrap(), value.into_string().unwrap()))
        .collect();
    got.sort();
    let want: Vec<(String, String)> = [
        ("AGENT_DECK_STATE_DIR", "/home/o/.local/state/agent-deck"), // legacy-name
        ("CEPHEUS_SOCKET", "/run/user/1/agent-deck/deck.sock"),      // legacy-name
        ("CEPHEUS_START_DIR", "/home/o/work"),
        ("CEPHEUS_STATE_DIR", "/home/o/.local/state/agent-deck"), // legacy-name
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1/bus"),
        ("HOME", "/home/o"),
        ("LANG", "en_US.UTF-8"),
        ("PATH", "/home/o/.local/bin:/usr/bin"),
    ]
    .iter()
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect();
    assert_eq!(got, want);
}

#[test]
fn the_user_managers_environment_is_read_exactly() {
    let text = r#"{"type":"as","data":["HOME=/home/o","EDITOR=omarchy-launch-editor --inline","EMPTY=","EQ=a=b"]}"#;
    let got = parse_manager_environment(text).unwrap();
    assert_eq!(
        got,
        vars(&[("HOME", "/home/o"), ("EDITOR", "omarchy-launch-editor --inline"), ("EMPTY", ""), ("EQ", "a=b")])
    );
    for bad in [
        r#"{"type":"s","data":"HOME=/home/o"}"#,
        r#"{"type":"as","data":["NOEQUALS"]}"#,
        r#"{"type":"as","data":["=value"]}"#,
        r#"{"type":"as","data":[1]}"#,
        "not json",
    ] {
        assert!(parse_manager_environment(bad).is_err(), "{bad}");
    }
}
