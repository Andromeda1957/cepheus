use super::*;
use crate::testsupport::TempDir;
fn descriptor(d: &TempDir, id: &str) -> SourceSpec {
    serde_json::from_value(serde_json::json!({"id":id,"owner":"owner","repository":"repo","mode":"command","cwd":d.path(),"argv":["true"]})).unwrap()
}
fn task() -> Task {
    serde_json::from_value(serde_json::json!({"id":"task","title":"task","state":"ready"})).unwrap()
}
#[test]
fn wrapped_source_argv_boundary_is_validated_before_registration() {
    let d = TempDir::new("source-argv-bound");
    let mut spec = descriptor(&d, "source");
    for argv in [vec![], vec![String::new()]] {
        spec.argv = argv;
        assert!(spec.validate().is_err());
    }
    spec.argv = vec!["true".into(); 123];
    assert!(spec.validate().is_ok());
    for n in [124, 128] {
        spec.argv = vec!["true".into(); n];
        assert!(spec.validate().is_err());
    }
}
#[test]
fn disabled_owner_and_reconciled_source_keep_rows_stale_until_fresh_publication() {
    let d = TempDir::new("source-lifecycle");
    let mut q = store::Queue::load(d.path()).unwrap();
    let mut a = descriptor(&d, "one");
    a.mode = "push".into();
    a.argv.clear();
    let result = q.source_register(a.clone()).unwrap();
    let token = result["publisher_token"].as_str().unwrap();
    assert!(task().target.is_none(), "legacy rows have no conversation target");
    assert!(
        serde_json::from_value::<TaskTarget>(serde_json::json!({
            "tool":"codex","conversation":"alpha-chat","host":"viewer"
        }))
        .is_err(),
        "a target cannot override its source endpoint"
    );
    for (tool, conversation) in
        [("shell", "alpha-chat"), ("codex", "--exec"), ("codex", "a b"), ("$(run)", "alpha-chat")]
    {
        let mut invalid = task();
        invalid.target = Some(TaskTarget { tool: tool.into(), conversation: conversation.into() });
        assert!(q.source_publish("one", "owner", token, 1, Some(vec![invalid]), 100).is_err());
        assert!(q.sources["one"].tasks.is_empty(), "invalid metadata cannot replace the snapshot");
    }
    let mut item = task();
    item.target = Some(TaskTarget { tool: "codex".into(), conversation: "alpha-chat".into() });
    q.source_publish("one", "owner", token, 1, Some(vec![item.clone()]), 100).unwrap();
    assert_eq!(q.sources["one"].tasks[0].target, item.target);
    assert_eq!(store::Queue::load(d.path()).unwrap().sources["one"].tasks[0].target, item.target);
    assert!(q.sources["one"].info(100).fresh);
    let item:Spec=serde_json::from_value(serde_json::json!({"key":"dependent","title":"dependent","kind":"job","cwd":d.path(),"argv":["true"],"dependencies":[{"source":"one","task":"task","condition":"ready"}]})).unwrap();
    let id = q.add_batch(vec![item], 100).unwrap()[0].id;
    assert!(q.dependencies_ready(id, 100));
    q.sources.get_mut("one").unwrap().owner_live = false;
    assert!(!q.dependencies_ready(id, 100));
    assert!(q.source_publish("one", "owner", token, 2, Some(vec![task()]), 100).is_err());
    q.refresh_sources(std::path::Path::new("/missing-fixture-executor"), 100, true).unwrap();
    assert!(q.refresh_jobs.records.is_empty());
    q.sources.get_mut("one").unwrap().owner_live = true;
    q.source_reconcile("different-owner", &[]).unwrap();
    assert!(q.sources["one"].spec.enabled);
    q.source_reconcile("owner", &[]).unwrap();
    assert!(!q.sources["one"].info(100).fresh);
    assert_eq!(q.sources["one"].tasks.len(), 1);
    let result = q.source_register(a).unwrap();
    let next = result["publisher_token"].as_str().unwrap();
    assert_ne!(next, token);
    assert!(!q.dependencies_ready(id, 100));
    assert!(q.source_publish("one", "owner", token, 2, Some(vec![task()]), 100).is_err());
    q.source_publish("one", "owner", next, 1, Some(vec![task()]), 101).unwrap();
    assert!(q.dependencies_ready(id, 101));
}
#[test]
fn invalid_command_cannot_starve_a_later_source_and_failures_are_categorical() {
    let d = TempDir::new("source-launch-isolation");
    let mut q = store::Queue::load(d.path()).unwrap();
    q.source_register(descriptor(&d, "a-bad")).unwrap();
    q.source_register(descriptor(&d, "z-good")).unwrap();
    q.sources.get_mut("a-bad").unwrap().spec.argv = vec!["true".into(); 128];
    q.refresh_sources(std::path::Path::new("/missing-fixture-executor"), 100, true).unwrap();
    assert!(q.sources["a-bad"].refresh_job.is_none());
    assert!(q.sources["z-good"].refresh_job.is_some(), "a later healthy descriptor reaches its durable launch intent");
    assert_eq!(q.sources["a-bad"].error.as_deref(), Some("source refresh failed"));
}
#[test]
fn an_unchanged_publication_renews_freshness_without_a_write() {
    let d = TempDir::new("source-unchanged");
    let mut q = store::Queue::load(d.path()).unwrap();
    let mut spec = descriptor(&d, "one");
    spec.mode = "push".into();
    spec.argv.clear();
    let token = q.source_register(spec).unwrap()["publisher_token"].as_str().unwrap().to_string();
    q.source_publish("one", "owner", &token, 1, Some(vec![task()]), 100).unwrap();
    let path = q.source_path("one");
    std::fs::remove_file(&path).unwrap();
    q.source_publish("one", "owner", &token, 2, Some(vec![task()]), 500).unwrap();
    assert!(!path.exists(), "the same rows are not written again");
    assert_eq!((q.sources["one"].sequence, q.sources["one"].updated_at), (2, Some(500)));
    assert!(q.sources["one"].info(500).fresh);
    let mut changed = task();
    changed.state = "stopped".into();
    q.source_publish("one", "owner", &token, 3, Some(vec![changed]), 501).unwrap();
    assert!(path.exists(), "changed rows are written");
    q.source_publish("one", "owner", &token, 4, None, 502).unwrap();
    std::fs::remove_file(&path).unwrap();
    q.source_publish("one", "owner", &token, 5, None, 503).unwrap();
    assert!(!path.exists(), "a repeated failure is not written again");
    assert_eq!(q.sources["one"].tasks.len(), 1, "a failure keeps the previous rows");
}
