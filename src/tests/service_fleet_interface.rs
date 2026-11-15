use super::*;
use crate::testsupport::TempDir;

#[test]
fn selected_viewer_keeps_native_service_free_of_recursive_federation() {
    let dir = TempDir::new("fleet-native-viewer");
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"hosts":[{
            "name":"coordinator","tag":1,"addresses":["127.0.0.1:1"],
            "fingerprint":"11".repeat(32)
        }]}))
        .unwrap(),
    )
    .unwrap();
    fs::write(path.with_file_name("coordinator.json"), br#"{"host":"coordinator"}"#).unwrap();
    let mut opts = tests::options(&dir, "true");
    opts.config_file = Some(path);
    let (tx, _events) = mpsc::channel();
    let mut service = Service::new(opts, tx);
    service.sync_hosts(Instant::now());
    assert!(service.hosts.is_empty(), "native registry must contain its own execution state only");
    assert!(service.hosts_checked.is_some());
}

#[test]
fn source_identity_and_resources_belong_to_native_service() {
    let dir = TempDir::new("fleet-native-label");
    let mut opts = tests::options(&dir, "true");
    opts.config.host_name = Some("Origin desktop".into());
    let (tx, _events) = mpsc::channel();
    let mut service = Service::new(opts, tx);
    service.sessions.insert(3, tests::fake_session(3, Tool::Codex));
    service.extensions.metadata.insert(
        3,
        std::collections::BTreeMap::from([
            ("fleet-workspace".into(), serde_json::json!({"source": "forged", "session": 900})),
            ("other-plugin".into(), serde_json::json!({"preserved":true})),
        ]),
    );
    let state = service.own_state();
    let session = &state.sessions[0];
    let identity = &session.metadata["fleet-workspace"];
    assert_eq!(identity["source"], service.native_identity);
    assert_eq!(identity["session"], 3);
    assert_eq!(session.metadata["other-plugin"]["preserved"], true);
    let (other_tx, _) = mpsc::channel();
    let mut other_opts = tests::options(&dir, "true");
    other_opts.state_dir = dir.path().join("other-profile");
    let other = Service::new(other_opts, other_tx);
    assert_ne!(service.native_identity, other.native_identity, "profiles on one machine must remain distinct sources");
    let native = state.native_host.unwrap();
    assert_eq!(native.label, "Origin desktop");
    assert!(native.online);
    assert_eq!(native.start_dir, dir.path().to_string_lossy());
    assert!(native.stats.is_some());
    assert!(state.available_tools.unwrap().iter().any(|tool| tool == "codex"));
}
