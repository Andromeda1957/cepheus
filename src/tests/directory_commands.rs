use super::*;
use crate::tui::tests::{app_with, press, session};

#[test]
fn cd_prompts_then_sends_the_selected_agents_literal_relative_path() {
    let selected = session(crate::protocol::host_id(2, 17), Tool::Codex, Status::Idle, true);
    let (mut app, messages) = app_with(vec![selected.clone()], vec![]);
    for version in [12, 13] {
        app.service_version = version;
        app.run_command("cd '../other project'");
        assert!(messages.try_iter().next().is_none(), "protocol {version} cannot change directories");
    }
    app.service_version = 14;
    app.run_command("cd");
    assert_eq!(app.mode, Mode::Command);
    assert_eq!(app.command, "cd ");
    assert!(messages.try_iter().next().is_none(), "opening the path prompt must not stop anything");
    for c in "'../other project'".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let actual: Vec<_> = messages.try_iter().map(|m| serde_json::to_value(m).unwrap()).collect();
    assert_eq!(actual, vec![serde_json::json!({"t":"change_directory", "id":selected.id, "path":"../other project"})]);
    app.state.providers.push(crate::providers::Info {
        id: "codex".into(),
        label: "Cloud fixture".into(),
        cloud: true,
        host: selected.host.clone(),
        ..Default::default()
    });
    app.run_command("cd ..");
    assert!(messages.try_iter().next().is_none(), "cloud agents must never be stopped for a directory change");
    assert!(app.notice.as_ref().unwrap().0.contains("cloud agents"));
}
