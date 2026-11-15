use super::*;
use crate::testsupport::TempDir;
fn spec(dir: &TempDir, key: &str) -> Spec {
    serde_json::from_value(
        serde_json::json!({"key":key,"title":key,"kind":"job","cwd":dir.path(),"argv":["printf","PRIVATE_SENTINEL"]}),
    )
    .unwrap()
}
#[test]
fn atomic_batch_dedup_private_payload_and_reload() {
    let d = TempDir::new("work-queue-batch");
    let mut q = Queue::load(d.path()).unwrap();
    let s = spec(&d, "one");
    let first = q.add_batch(vec![s.clone(), spec(&d, "two")], 1).unwrap();
    assert_eq!(q.add_batch(vec![s], 2).unwrap()[0].id, first[0].id);
    let mut invalid = spec(&d, "bad");
    invalid.dependencies.push(Dependency { queue: Some(999), source: None, task: None, condition: "finished".into() });
    assert!(q.add_batch(vec![spec(&d, "three"), invalid], 2).is_err());
    assert_eq!(q.records.len(), 2);
    drop(q);
    let q = Queue::load(d.path()).unwrap();
    assert_eq!(q.records.len(), 2);
    assert!(!q.enabled);
    assert!(!serde_json::to_string(&q.snapshot()).unwrap().contains("PRIVATE_SENTINEL"));
}
#[test]
fn dependencies_cycles_cancellation_and_reorder_preserve_ownership() {
    let d = TempDir::new("work-queue-dependencies");
    let mut q = Queue::load(d.path()).unwrap();
    let a = q.add_batch(vec![spec(&d, "A")], 1).unwrap()[0].id;
    let mut b = spec(&d, "B");
    b.dependencies.push(Dependency { queue: Some(a), source: None, task: None, condition: "finished".into() });
    let b = q.add_batch(vec![b], 2).unwrap()[0].id;
    assert!(!q.dependencies_ready(b, 3));
    q.transition(a, "failed", "failed", 3, None).unwrap();
    assert!(!q.dependencies_ready(b, 3));
    q.transition(a, "done", "finished", 4, None).unwrap();
    assert!(q.dependencies_ready(b, 4));
    q.begin_attempt(b).unwrap();
    assert!(q.cancel(b, 5).is_err());
    assert!(q.reorder(&[b], 5).is_err());
}
#[test]
fn unchanged_progress_has_no_disk_writes_and_real_transition_reloads() {
    let d = TempDir::new("work-queue-dirty");
    let mut q = Queue::load(d.path()).unwrap();
    let id = q.add_batch(vec![spec(&d, "dirty")], 1).unwrap()[0].id;
    q.transition(id, "held", "memory", 2, None).unwrap();
    let path = q.progress_path(id);
    let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
    q.transition(id, "held", "memory", 3, None).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), stamp);
    q.transition(id, "dispatched", "accepted", 4, Some(Dispatch { kind: "job".into(), id: 42 })).unwrap();
    let q = Queue::load(d.path()).unwrap();
    assert_eq!(q.records[&id].info.dispatch.as_ref().unwrap().id, 42);
}

#[test]
fn published_batch_barrier_failure_keeps_keys_fenced_and_retry_reload_unique() {
    let d = TempDir::new("work-queue-postrename");
    let mut q = Queue::load(d.path()).unwrap();
    q.fail_batch_sync = true;
    let s = spec(&d, "immutable");
    assert!(q.add_batch(vec![s.clone()], 1).is_err());
    assert_eq!(q.records.len(), 1);
    assert!(q.error.is_some());
    assert!(q.add_batch(vec![s.clone()], 2).is_err());
    assert_eq!(q.records.len(), 1);
    q.fail_batch_sync = false;
    let accepted = q.add_batch(vec![s], 3).unwrap()[0].id;
    assert!(q.error.is_none());
    drop(q);
    let q = Queue::load(d.path()).unwrap();
    assert_eq!(q.records.len(), 1);
    assert!(q.records.contains_key(&accepted));
}
#[test]
fn independent_origin_execution_keys_are_distinct_and_fifo_stable() {
    let a = TempDir::new("work-queue-origin-a");
    let b = TempDir::new("work-queue-origin-b");
    let mut one = Queue::load(a.path()).unwrap();
    let mut two = Queue::load(b.path()).unwrap();
    let payload = spec(&a, "shared-client-key");
    let id = one.add_batch(vec![payload.clone()], 1).unwrap()[0].id;
    two.add_batch(vec![payload], 1).unwrap();
    assert_ne!(one.records[&id].dispatch_key, two.records[&id].dispatch_key);
    let key = one.records[&id].dispatch_key.clone();
    let added = one.add_batch(vec![spec(&a, "third"), spec(&a, "second")], 1).unwrap();
    assert_eq!(one.ordered(), vec![id, added[0].id, added[1].id]);
    drop(one);
    let one = Queue::load(a.path()).unwrap();
    assert_eq!(one.records[&id].dispatch_key, key);
    assert_eq!(one.ordered(), vec![id, added[0].id, added[1].id]);
}
#[test]
fn cyclic_atomic_batch_is_rejected_and_gc_keeps_live_dependencies() {
    let d = TempDir::new("work-queue-cycle-gc");
    let mut q = Queue::load(d.path()).unwrap();
    let mut a = spec(&d, "a");
    let mut b = spec(&d, "b");
    a.dependencies.push(Dependency {
        queue: Some(identity("b")),
        source: None,
        task: None,
        condition: "finished".into(),
    });
    b.dependencies.push(Dependency {
        queue: Some(identity("a")),
        source: None,
        task: None,
        condition: "finished".into(),
    });
    assert!(q.add_batch(vec![a, b], 1).is_err());
    assert!(q.records.is_empty());
    let a = q.add_batch(vec![spec(&d, "a")], 1).unwrap()[0].id;
    let mut b = spec(&d, "b");
    b.dependencies.push(Dependency { queue: Some(a), source: None, task: None, condition: "finished".into() });
    let b = q.add_batch(vec![b], 2).unwrap()[0].id;
    q.transition(a, "done", "complete", 3, None).unwrap();
    q.gc(40 * 86400).unwrap();
    assert!(q.records.contains_key(&a));
    q.transition(b, "done", "complete", 4, None).unwrap();
    q.gc(40 * 86400).unwrap();
    assert!(q.records.is_empty());
    assert!(Queue::load(d.path()).unwrap().records.is_empty());
}
#[test]
fn unchanged_mixed_queue_poll_is_read_only_at_one_ten_and_hundred_records() {
    for count in [1, 10, 100] {
        let d = TempDir::new("work-queue-scale-dirty");
        let mut q = Queue::load(d.path()).unwrap();
        let items = q.add_batch((0..count).map(|i| spec(&d, &format!("item-{i}"))).collect(), 1).unwrap();
        for (i, item) in items.iter().enumerate() {
            q.transition(item.id, if i % 2 == 0 { "held" } else { "done" }, "measured", 2, None).unwrap();
        }
        let stamps: Vec<_> =
            items.iter().map(|i| std::fs::metadata(q.progress_path(i.id)).unwrap().modified().unwrap()).collect();
        for _ in 0..3 {
            let _ = q.snapshot();
            for (i, item) in items.iter().enumerate() {
                q.transition(item.id, if i % 2 == 0 { "held" } else { "done" }, "measured", 99, None).unwrap();
            }
        }
        for (i, item) in items.iter().enumerate() {
            assert_eq!(std::fs::metadata(q.progress_path(item.id)).unwrap().modified().unwrap(), stamps[i]);
        }
        q.transition(items[0].id, "done", "actual transition", 100, None).unwrap();
        assert_eq!(Queue::load(d.path()).unwrap().records[&items[0].id].info.updated_at, 100);
    }
}
