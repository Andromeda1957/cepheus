use crate::attention::{store::*, DeliveryState};
use crate::testsupport::TempDir;
fn target(session: u64) -> Target {
    Target { session, spawned_ms: 100, conversation: Some("chat".into()), pid: Some(12) }
}
#[test]
fn same_second_fifo_survives_reload_and_exact_retries_do_not_reorder() {
    let d = TempDir::new("attention-fifo");
    let mut s = Store::load(d.path()).unwrap();
    let mut ids = Vec::new();
    for n in 0..100 {
        ids.push(s.add(&format!("key-{n}"), "private body", target(1), 50).unwrap().id);
    }
    let retry = s.add("key-3", "private body", target(1), 51).unwrap();
    assert_eq!(retry.id, ids[3]);
    drop(s);
    let s = Store::load(d.path()).unwrap();
    let mut rows: Vec<_> = s.messages.values().collect();
    rows.sort_by_key(|m| m.sequence);
    assert_eq!(rows.iter().map(|m| m.info.id.clone()).collect::<Vec<_>>(), ids);
    assert!(rows.windows(2).all(|r| r[0].sequence < r[1].sequence));
}
#[test]
fn failed_publication_never_creates_runnable_ram_and_same_key_recovers_original_file() {
    let d = TempDir::new("attention-no-ram");
    let mut s = Store::load(d.path()).unwrap();
    let namespace = s.saved.namespace.clone();
    let hash = crate::remote::auth::sha256_hex(format!("{namespace}:bad").as_bytes());
    let id = format!("{}-{}-{}-{}-{}", &hash[..8], &hash[8..12], &hash[12..16], &hash[16..20], &hash[20..32]);
    let path = s.root.join("messages").join(format!("{id}.json"));
    std::fs::create_dir(&path).unwrap();
    assert!(s.add("bad", "never publish", target(1), 1).is_err());
    assert!(s.messages.is_empty());
    std::fs::remove_dir(path).unwrap();
    let first = s.add("bad", "never publish", target(1), 1).unwrap();
    let mut published = s.messages.remove(&first.id).unwrap();
    published.durable = false;
    s.messages.insert(first.id.clone(), published);
    assert!(s.begin(&first.id, 2).is_err());
    assert_eq!(s.add("bad", "never publish", target(1), 2).unwrap().id, first.id);
    s.begin(&first.id, 3).unwrap();
    drop(s);
    let s = Store::load(d.path()).unwrap();
    assert_eq!(s.messages[&first.id].info.state, DeliveryState::Unknown);
    assert!(s.messages[&first.id].attempted);
}
#[test]
fn expired_payload_keeps_retry_fence_and_capacity_is_reusable() {
    let d = TempDir::new("attention-retention");
    let mut s = Store::load(d.path()).unwrap();
    let id = s.add("old", "private", target(1), 1).unwrap().id;
    s.update(&id, DeliveryState::Submitted, "", 2).unwrap();
    s.gc(86404, 1).unwrap();
    assert!(s.messages.is_empty());
    assert!(s.add("old", "private", target(1), 86404).is_err());
    assert!(s.add("new", "new private", target(1), 86404).is_ok());
}
#[test]
fn near_capacity_receipts_roundtrip_and_oversize_changes_never_replace_them() {
    let d = TempDir::new("attention-capacity");
    let mut s = Store::load(d.path()).unwrap();
    let size = serde_json::to_vec(&s.saved).unwrap().len();
    s.saved.rules.push(serde_json::json!("r".repeat(RECEIPT_LIMIT - size - 128)));
    s.save().unwrap();
    let before = std::fs::read(s.root.join("state.json")).unwrap();
    assert!(before.len() <= RECEIPT_LIMIT && before.len() > RECEIPT_LIMIT - 256);
    s.saved.rules.push(serde_json::json!("r".repeat(512)));
    assert!(s.save().is_err());
    assert_eq!(std::fs::read(s.root.join("state.json")).unwrap(), before);
    drop(s);
    let s = Store::load(d.path()).unwrap();
    assert_eq!(s.saved.rules.len(), 1);
}
#[test]
fn state_written_before_the_inbox_removal_loads_and_its_retired_fields_go_on_the_next_save() {
    let d = TempDir::new("attention-retired-state");
    let root = Store::load(d.path()).unwrap().root;
    let old = serde_json::json!({
        "model": {"away": "a", "job": 4},
        "model_authorized_away": "a",
        "remote_digests": {"k": 1},
        "remote_off": {"origin": 2},
        "source_states": {"k": "merged"},
        "episodes": [],
        "stalls": [],
        "unsent": [],
        "namespace": "kept-namespace",
        "away_since": 1,
        "last_present": 2,
        "away": {"id": "a", "created_at": 1, "counts": {}, "model_state": "off"},
        "counts": {"finished": 3},
        "acknowledgments": ["a"],
        "supervisor": {"audit": [], "enabled": true, "revision": 9, "rules": 0, "pending": []},
        "rules": []
    });
    publish(&root.join("state.json"), &old).unwrap();
    let mut s = Store::load(d.path()).unwrap();
    assert_eq!(s.saved.namespace, "kept-namespace");
    assert_eq!(s.saved.remote_off["origin"], 2);
    assert!(s.saved.supervisor.enabled && s.saved.supervisor.revision == 9);
    s.saved.supervisor.enabled = false;
    s.save().unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("state.json")).unwrap()).unwrap();
    let mut keys: Vec<_> = saved.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["namespace", "remote_off", "rules", "supervisor"]);
}
