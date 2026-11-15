use super::*;
use crate::testsupport::TempDir;
#[test]
fn strict_eight_hour_fence_reload_midnight_and_stale_completion() {
    let d = TempDir::new("chore-fence");
    let spec:ChoreSpec=serde_json::from_value(serde_json::json!({"id":"daily","owner":"owner","repository":"repo","title":"daily","target":"job","schedule":{"cron":"0 0 * * *","timezone":"UTC"},"spec":{"key":"template","title":"daily","kind":"job","cwd":d.path(),"argv":["true"]}})).unwrap();
    let mut q = store::Queue::load(d.path()).unwrap();
    q.chore_register(spec, 100).unwrap();
    let c = q.chores.get_mut("daily").unwrap();
    let one = c.claim("2026-11-01T00:00", "2026-11-01", "one", "session", 100).unwrap();
    for n in 0..100 {
        assert!(c.claim("2026-11-01T00:00", "2026-11-01", &format!("other{n}"), "other", 100 + 8 * 3600).is_err());
    }
    let two = c.claim("2026-11-01T00:00", "2026-11-01", "two", "second", 101 + 8 * 3600).unwrap();
    assert_eq!(two.info.generation, 2);
    let context = Context {
        id: "daily".into(),
        day: two.info.day.clone(),
        key: two.key.clone(),
        generation: two.info.generation,
        token: two.token.clone(),
        session: two.info.session.clone(),
    };
    let env: BTreeMap<_, _> = context.env().into_iter().collect();
    assert_eq!(env.len(), 12, "every fenced value reaches new and existing chore workers");
    for (suffix, value) in [
        ("ID", "daily".to_string()),
        ("DAY", two.info.day.clone()),
        ("KEY", two.key.clone()),
        ("GENERATION", "2".to_string()),
        ("TOKEN", two.token.clone()),
        ("SESSION", two.info.session.clone()),
    ] {
        assert_eq!(env[&format!("CEPHEUS_CHORE_{suffix}")], value);
        assert_eq!(env[&format!("AGENT_DECK_CHORE_{suffix}")], value); // legacy-name
    }
    for n in 0..100 {
        assert!(c.claim("2026-11-01T00:00", "2026-11-01", &format!("stale{n}"), "other", 101 + 8 * 3600).is_err());
    }
    assert!(c.complete(&one.info.day, one.info.generation, &one.token, "session", "done").is_err());
    c.complete(&two.info.day, two.info.generation, &two.token, "second", "done").unwrap();
    q.save_chore("daily").unwrap();
    drop(q);
    let mut q = store::Queue::load(d.path()).unwrap();
    let c = q.chores.get_mut("daily").unwrap();
    assert!(c.claim("2026-11-01T00:00", "2026-11-01", "late", "late", 200000).is_err());
    let next = c.claim("2026-11-02T00:00", "2026-11-02", "next", "next", 200000).unwrap();
    assert_eq!(next.info.day, "2026-11-02");
    assert_eq!(next.info.generation, 1);
    assert_eq!(c.claim("2026-11-02T00:00", "2026-11-02", "next", "next", 200100).unwrap().token, next.token);
    assert!(!serde_json::to_string(&q.snapshot()).unwrap().contains(&next.token));
}

#[test]
fn automatic_stale_generation_changes_once_and_old_request_never_reclaims() {
    let d = TempDir::new("chore-auto-retake");
    let mut q = store::Queue::load(d.path()).unwrap();
    let spec:ChoreSpec=serde_json::from_value(serde_json::json!({"id":"daily","owner":"owner","repository":"repo","title":"daily","target":"job","schedule":{"cron":"0 9 * * *","timezone":"UTC"},"spec":{"key":"template","title":"daily","kind":"job","cwd":d.path(),"argv":["true"]}})).unwrap();
    q.chore_register(spec, 100).unwrap();
    let c = q.chores.get_mut("daily").unwrap();
    let occurrence = "2026-11-06T09:00";
    let first_key = c.scheduled_key(occurrence, 100).unwrap();
    let first = c.claim(occurrence, "2026-11-06", &first_key, "first", 100).unwrap();
    assert_eq!(c.scheduled_key(occurrence, 100 + 8 * 3600).unwrap(), first_key);
    let next_key = c.scheduled_key(occurrence, 101 + 8 * 3600).unwrap();
    assert_ne!(next_key, first_key);
    let second = c.claim(occurrence, "2026-11-06", &next_key, "second", 101 + 8 * 3600).unwrap();
    assert_eq!(second.info.generation, 2);
    for _ in 0..100 {
        assert_eq!(c.scheduled_key(occurrence, 102 + 8 * 3600).unwrap(), next_key);
        assert_eq!(c.claim(occurrence, "2026-11-06", &first_key, "first", 200000).unwrap().token, first.token);
    }
    assert_eq!(c.claims[occurrence].token, second.token);
    q.save_chore("daily").unwrap();
    let q = store::Queue::load(d.path()).unwrap();
    assert_eq!(q.chores["daily"].claims[occurrence].token, second.token);
}
#[test]
fn completed_chore_lease_retention_is_finite_without_losing_current_occurrence() {
    let d = TempDir::new("chore-retention");
    let mut q = store::Queue::load(d.path()).unwrap();
    let spec:ChoreSpec=serde_json::from_value(serde_json::json!({"id":"cron","owner":"owner","repository":"repo","title":"cron","target":"job","schedule":{"cron":"* * * * *","timezone":"UTC"},"spec":{"key":"template","title":"cron","kind":"job","cwd":d.path(),"argv":["true"]}})).unwrap();
    q.chore_register(spec, 100).unwrap();
    let c = q.chores.get_mut("cron").unwrap();
    for day in 1..=150 {
        let date = format!("2026-{day:03}");
        let key = format!("request{day}");
        let claim = c.claim(&date, &date, &key, "session", day * 86400).unwrap();
        c.complete(&date, 1, &claim.token, "session", "done").unwrap();
    }
    assert!(c.claims.len() < 100);
    assert_eq!(c.claims.len(), 31);
    q.save_chore("cron").unwrap();
    assert!(store::Queue::load(d.path()).unwrap().chores["cron"].claims.contains_key("2026-150"));
}
