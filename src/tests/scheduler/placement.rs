use super::*;
fn host(memory: u64, load: f64) -> Headroom {
    Headroom {
        available_mib: Some(memory),
        total_mib: Some(16384),
        cpus: 8,
        at: 100,
        load: Some(load),
        cpu_pressure: Some(0.0),
        memory_pressure: Some(0.0),
    }
}
#[test]
fn saturated_desktop_loses_to_idle_peer_and_stale_capacity_is_rejected() {
    assert_eq!(score(&host(8000, 44.0), 100, 0, true), None);
    assert!(score(&host(8000, 1.0), 100, 0, false).is_some());
    assert_eq!(score(&host(8000, 1.0), 131, 0, false), None);
}
#[test]
fn bursts_consume_reservations_and_keep_desktop_capacity() {
    let h = host(4000, 1.0);
    assert!(score(&h, 100, 0, true).unwrap() > score(&h, 100, 1, true).unwrap());
    assert_eq!(score(&h, 100, 2, true), None);
    assert!(score(&h, 100, 2, false).is_some());
}

#[test]
fn native_provider_readiness_uses_protocol_tool_keys_and_plugin_policy() {
    let command = vec!["/bin/sh".into()];
    let mut config = crate::config::Config {
        claude_command: command.clone(),
        codex_command: command.clone(),
        antigravity_command: command.clone(),
        grok_command: command.clone(),
        opencode_command: command,
        ..Default::default()
    };
    let ready = available_tools(&config);
    for tool in [
        crate::protocol::Tool::Claude,
        crate::protocol::Tool::Codex,
        crate::protocol::Tool::Antigravity,
        crate::protocol::Tool::Grok,
        crate::protocol::Tool::OpenCode,
    ] {
        assert!(ready.iter().any(|key| key == tool.key()));
    }
    config.plugins.insert("provider-agy".into(), false);
    assert!(!available_tools(&config).iter().any(|key| key == "agy"));
}
