use super::*;
use crate::tui::tests::{app_with, host};

fn sent(rx: &Receiver<ClientMsg>) -> Vec<serde_json::Value> {
    rx.try_iter().map(|m| serde_json::to_value(m).unwrap()).collect()
}

#[test]
fn a_number_after_the_program_starts_that_many_sessions_at_once() {
    let (mut app, rx) = app_with(vec![], vec![]);
    app.service_version = PROTOCOL_VERSION;
    app.handle_server(ServerMsg::Hosts { hosts: vec![host("fedora", "Fedora", true)] });
    let start = |tool: &str| serde_json::json!({"t": "create", "request": 0, "tool": tool});
    app.run_command("claude 5");
    assert_eq!(sent(&rx), vec![start("claude"); 5]);
    app.run_command("new codex 3");
    assert_eq!(sent(&rx), vec![start("codex"); 3]);
    for count in [10, 16, 24] {
        app.run_command(&format!("codex {count}"));
        assert_eq!(sent(&rx), vec![start("codex"); count]);
    }
    let on_fedora =
        serde_json::json!({"t": "create", "request": 0, "tool": "grok", "host": "fedora", "model": "sonnet"});
    for line in ["grok fedora 2 --model sonnet", "grok 2 fedora --model sonnet"] {
        app.run_command(line);
        assert_eq!(sent(&rx), vec![on_fedora.clone(); 2], "{line}");
    }
    let too_many = format!("claude {}", crate::protocol::MAX_START_BATCH + 1);
    for line in ["claude 0", too_many.as_str(), "claude 2 3"] {
        app.run_command(line);
        assert!(sent(&rx).is_empty(), "{line} must start nothing");
    }
}
