//! Generic Markdown/GitHub task-source service integration.
use super::*;

#[test]
fn installed_generic_service_registers_literal_provider_argv_only_when_enabled() {
    let dir = TempDir::new("source-generic");
    dir.write("config/service/task-sources.lua", include_str!("../../lua/task_sources/service.lua"));
    dir.write("config/task-sources.json",r#"{"enabled":true,"sources":[{"id":"task-sources:repo:tasks","repository":"repo","provider":"markdown","enabled":true},{"id":"task-sources:repo:issues","repository":"repo","provider":"github","github_repo":"owner/project","enabled":false}]}"#);
    std::fs::create_dir_all(dir.path().join("start")).unwrap();
    let (tx, _rx) = mpsc::channel();
    let mut e = Engine::new_service(
        Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("start") },
        Arc::new(move |done| {
            let _ = tx.send(done);
        }),
    )
    .unwrap();
    let root = dir.path().join("start").to_string_lossy().into_owned();
    e.set_state(serde_json::json!({"work_queue_capable":true,"scheduler":{"repositories":{"repo":root}}}));
    e.load_service();
    assert!(e.take_errors().is_empty());
    let requests = e.take_requests();
    let [Request::Fleet { action, args, callback }] = requests.as_slice() else {
        panic!("configuration must reconcile before registering any command")
    };
    assert_eq!(action, "source-reconcile");
    assert_eq!(args, &serde_json::json!({"owner":"task-sources","ids":[]}));
    e.service_result(*callback, &serde_json::json!({"sources":[],"tasks":[]}), None);
    let requests = e.take_requests();
    let registrations: Vec<_> =
        requests.iter().filter(|r| matches!(r,Request::Fleet{action,..} if action=="source-register")).collect();
    assert_eq!(registrations.len(), 1, "disabled GitHub policy cannot contact its provider");
    let Request::Fleet { args, callback, .. } = registrations[0] else { unreachable!() };
    assert_eq!(args["mode"], "command");
    assert_eq!(
        args["argv"],
        serde_json::json!([
            std::env::current_exe().unwrap().to_string_lossy(),
            "task-source",
            "--provider",
            "markdown",
            "--source-id",
            "task-sources:repo:tasks",
            "--repository-root",
            root,
            "--file",
            "TASKS.md"
        ])
    );
    assert!(!e.has_jobs(), "generic service registers core-supervised command descriptors");
    e.service_result(*callback, &serde_json::json!({"source":{"id":"task-sources:repo:tasks"}}), None);
    assert_eq!(e.service_command("task-sources", "status", &serde_json::json!({})).unwrap()["registered"], 1);
}

#[test]
fn generic_disabled_missing_or_invalid_configuration_reconciles_to_no_execution() {
    for (name, config) in [
        ("missing", None),
        ("disabled", Some(r#"{"enabled":false,"sources":[]}"#)),
        ("removed", Some(r#"{"enabled":true,"sources":[]}"#)),
        ("source-disabled", Some(r#"{"enabled":true,"sources":[{"id":"previous","enabled":false}]}"#)),
        ("invalid-json", Some("{")),
        ("empty", Some("")),
        ("invalid-sources", Some(r#"{"enabled":true,"sources":"bad"}"#)),
    ] {
        let dir = TempDir::new(&format!("source-inactive-{name}"));
        dir.write("config/service/task-sources.lua", include_str!("../../lua/task_sources/service.lua"));
        if let Some(config) = config {
            dir.write("config/task-sources.json", config);
        }
        let (tx, _rx) = mpsc::channel();
        let mut e = Engine::new_service(
            Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("start") },
            Arc::new(move |done| {
                let _ = tx.send(done);
            }),
        )
        .unwrap();
        e.set_state(serde_json::json!({"work_queue_capable":true}));
        e.load_service();
        assert!(e.take_errors().is_empty(), "{name}");
        let requests = e.take_requests();
        let controls: Vec<_> = requests.into_iter().filter(|r| matches!(r, Request::Fleet { .. })).collect();
        let [Request::Fleet { action, args, callback }] = controls.as_slice() else {
            panic!("{name}: old descriptors must be reconciled")
        };
        assert_eq!(action, "source-reconcile", "{name}");
        assert_eq!(args, &serde_json::json!({"owner":"task-sources","ids":[]}), "{name}");
        e.service_result(*callback, &serde_json::json!({"sources":[],"tasks":[]}), None);
        assert!(e.take_requests().is_empty(), "{name}: no provider may be registered after opt-out");
        assert!(!e.has_jobs(), "{name}");
        assert_eq!(e.service_command("task-sources", "status", &serde_json::json!({})).unwrap()["registered"], 0);
    }
}
