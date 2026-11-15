use super::*;
use crate::testsupport::TempDir;
fn record(repo: PathBuf, key: &str, text: String) -> a::store::DecisionRecord {
    a::store::DecisionRecord {
        key: key.into(),
        repository: repo.clone(),
        request_repository: repo.to_string_lossy().into_owned(),
        requested: None,
        text,
        info: a::store::Decision { id: util::uuid_v4().unwrap(), created_at: 1, recipients: vec![1] },
        targets: vec![a::store::Target { session: 1, spawned_ms: 1, conversation: None, pid: None }],
        completed: false,
        durable: true,
        peers: vec![],
    }
}
#[test]
fn separate_large_decision_receipts_reload_beyond_old_single_file_capacity() {
    let d = TempDir::new("attention-decision-records");
    let s = a::store::Store::load(d.path()).unwrap();
    let root = s.root.clone();
    drop(s);
    for i in 0..40 {
        let r = record(d.path().to_path_buf(), &format!("key-{i}"), "x".repeat(15900));
        crate::job_sessions::store::write(
            &root.join("decisions").join(format!("{}.json", crate::remote::auth::sha256_hex(r.key.as_bytes()))),
            &r,
        )
        .unwrap();
    }
    let s = a::store::Store::load(d.path()).unwrap();
    assert_eq!(s.decisions.len(), 40);
    assert!(s.decisions.values().all(|r| r.text.len() == 15900));
}
#[test]
fn two_services_publish_under_one_repository_lock_without_losing_or_duplicating_records() {
    let repo = TempDir::new("attention-shared-decisions");
    let root = repo.path().to_path_buf();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|i| {
            let root = root.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let d = TempDir::new(&format!("attention-publisher-{i}"));
                let (tx, _) = mpsc::channel();
                let svc = Service::new(super::super::tests::options(&d, "true"), tx);
                let r = record(root, &format!("decision-{i}"), format!("UNIQUE_BODY_{i}"));
                barrier.wait();
                let deadline = Instant::now() + crate::testsupport::scaled_timeout(Duration::from_secs(10));
                loop {
                    match svc.attention_publish_decision(&r) {
                        Ok(()) => break,
                        Err(e) if e.to_string().contains("busy") => {
                            assert!(Instant::now() < deadline);
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("{e:#}"),
                    }
                }
                svc.attention_publish_decision(&r).unwrap();
                r.info.id
            })
        })
        .collect();
    let ids: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    let text = fs::read_to_string(root.join(".cepheus/decisions.md")).unwrap();
    for i in 0..2 {
        assert_eq!(text.matches(&format!("UNIQUE_BODY_{i}")).count(), 1);
        assert_eq!(text.matches(&format!("cepheus-decision:{}:complete", ids[i])).count(), 1);
    }
}
#[test]
fn lost_ack_retry_uses_original_recipient_set_after_session_changes() {
    let d = TempDir::new("attention-decision-frozen");
    let (tx, _) = mpsc::channel();
    let mut svc = Service::new(super::super::tests::options(&d, "true"), tx);
    svc.sessions.insert(1, super::super::tests::fake_session(1, Tool::Codex));
    svc.sessions.insert(2, super::super::tests::fake_session(2, Tool::Claude));
    let args = serde_json::json!({"key":"same","repository":d.path(),"text":"Keep this decision"});
    let first = svc.attention_decide(&args).unwrap();
    svc.sessions.remove(&2);
    svc.sessions.insert(3, super::super::tests::fake_session(3, Tool::Codex));
    let retry = svc.attention_decide(&args).unwrap();
    assert_eq!(first, retry);
    assert_eq!(retry["decision"]["recipients"], serde_json::json!([1, 2]));
    assert_eq!(svc.attention.as_ref().unwrap().store.messages.len(), 2);
}

#[test]
fn expanded_discovery_receipt_cannot_replace_a_reloadable_decision_with_oversized_json() {
    let d = TempDir::new("attention-decision-discovery-bound");
    let store = a::store::Store::load(d.path()).unwrap();
    let r = record(d.path().to_path_buf(), "discovery", "private decision".into());
    let path = store.root.join("decisions").join(format!("{}.json", crate::remote::auth::sha256_hex(r.key.as_bytes())));
    a::store::publish(&path, &r).unwrap();
    let before = fs::read(&path).unwrap();
    let mut expanded = r;
    expanded.targets = (1..=1000)
        .map(|session| a::store::Target { session, spawned_ms: 1, conversation: Some("c".repeat(1024)), pid: None })
        .collect();
    assert!(a::store::publish(&path, &expanded).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(store);
    let recovered = a::store::Store::load(d.path()).unwrap();
    assert_eq!(recovered.decisions["discovery"].targets.len(), 1);
}
