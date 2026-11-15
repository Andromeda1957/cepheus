use super::*;
use crate::testsupport::TempDir;

fn input(now: u64) -> Value {
    json!({
        "model": {"display_name": "Opus"},
        "context_window": {"used_percentage": 41.6},
        "rate_limits": {
            "five_hour": {"used_percentage": 12.4, "resets_at": now + 2 * 3600 + 5 * 60},
            "seven_day": {"used_percentage": 63.0, "resets_at": now + 86400}
        }
    })
}

#[test]
fn renders_model_context_and_windows() {
    let now = 1_800_000_000;
    assert_eq!(render(&input(now), now), "Opus · ctx 42% · 5h 12% (2h05m) · wk 63%");
}

#[test]
fn renders_partial_and_empty_input() {
    let now = 1_800_000_000;
    let v = json!({"rate_limits": {"five_hour": {"used_percentage": 50, "resets_at": now - 5}}});
    assert_eq!(render(&v, now), "5h 50%", "a past reset time is not shown");
    assert_eq!(render(&Value::Null, now), "");
}

#[test]
fn usage_snapshot_is_written_when_changed_or_stale() {
    let dir = TempDir::new("statusline-usage");
    let path = dir.path().join("state/claude-usage.json");
    let now = 1_800_000_000;
    let limits = input(now)["rate_limits"].clone();
    save_usage(&path, None, &limits, now);
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["updated_at"], now);
    assert_eq!(saved["rate_limits"], limits);

    // Unchanged and fresh: left alone.
    save_usage(&path, None, &limits, now + 30);
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["updated_at"], now);

    // Unchanged but a minute old: refreshed so the deck knows it is current.
    save_usage(&path, None, &limits, now + 61);
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["updated_at"], now + 61);

    // Changed: written at once.
    let mut changed = limits.clone();
    changed["five_hour"]["used_percentage"] = json!(13);
    save_usage(&path, None, &changed, now + 62);
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["rate_limits"], changed);
}

#[test]
fn concurrent_status_lines_take_turns_with_the_snapshot() {
    // Review CR-20261026-d5c080aeb2: two hosted Claude sessions' status lines
    // both read the old snapshot, and the one that wrote last replaced the
    // other's report. Each read, merge and write now holds a lock.
    let dir = TempDir::new("statusline-lock");
    let path = dir.path().join("state/claude-usage.json");
    let now = 1_800_000_000;
    let window = move |used: f64| json!({"five_hour": {"used_percentage": used, "resets_at": now + 3600}});
    save_usage(&path, Some("one"), &window(0.0), now);
    // Another status line is between its read and its write.
    let held = fs::OpenOptions::new().write(true).create(true).truncate(false).open(lock_path(&path)).unwrap();
    // SAFETY: flock only reads the descriptor, which `held` keeps open.
    assert_eq!(unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&held), libc::LOCK_EX) }, 0);
    let writer = {
        let path = path.clone();
        std::thread::spawn(move || save_usage(&path, Some("two"), &window(10.0), now + 1))
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    let during: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    // That status line writes its 40% and lets go.
    let both = {
        let mut v = during.clone();
        v["rate_limits"] = window(40.0);
        v["reports"]["one"] = window(40.0);
        v
    };
    util::write_atomic(&path, both.to_string().as_bytes()).unwrap();
    drop(held);
    writer.join().unwrap();
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(during["rate_limits"], window(0.0), "nothing is written while another holds the lock");
    assert_eq!(saved["reports"]["one"], window(40.0), "the waiting status line merges over the newer snapshot");
}

#[test]
fn a_session_repeating_its_last_reply_never_replaces_a_newer_one() {
    // Every hosted Claude reports the limits from its own last reply again
    // whenever its status line redraws, so an idle one repeats older numbers
    // than the snapshot holds (the Codex half is review
    // CR-20261026-3fdeb2041f). A newer reply wins even with less use: a plan
    // reset refills a window before it ends (owner, 2026-11-12: "i just used
    // one of my resets, and it still shows 0%").
    let dir = TempDir::new("statusline-merge");
    let path = dir.path().join("state/claude-usage.json");
    let saved = || serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap()["rate_limits"].clone();
    let now = 1_800_000_000;
    let (five, week) = (now + 3600, now + 86_400);
    let window = |used: f64, resets_at: u64| json!({"used_percentage": used, "resets_at": resets_at});
    let (idle, busy) = (Some("idle"), Some("busy"));
    let earlier = json!({"five_hour": window(10.0, five), "seven_day": window(65.0, week)});
    let used_up = json!({"five_hour": window(40.0, five), "seven_day": window(100.0, week)});
    save_usage(&path, idle, &earlier, now);
    save_usage(&path, busy, &used_up, now + 5);
    save_usage(&path, idle, &earlier, now + 10);
    assert_eq!(saved(), used_up);
    // The owner spends a reset and the busy session's next reply has the
    // same week refilled.
    let refilled = json!({"five_hour": window(41.0, five), "seven_day": window(0.0, week)});
    save_usage(&path, busy, &refilled, now + 15);
    assert_eq!(saved(), refilled);
    save_usage(&path, idle, &earlier, now + 20);
    assert_eq!(saved(), refilled);
    // Both windows end while the idle session waits, and it goes on reporting
    // them after the busy one's next reply opened new ones.
    let later = week + 60;
    let next = json!({"five_hour": window(3.0, later + 3600), "seven_day": window(1.0, later + 7 * 86_400)});
    save_usage(&path, busy, &next, later);
    save_usage(&path, idle, &earlier, later + 5);
    assert_eq!(saved(), next);
}

#[test]
fn running_cost_snapshot_includes_internal_calls_without_conversation_text() {
    let dir = TempDir::new("statusline-cost");
    let payload = json!({"session_id": "session-123", "cost": {"total_cost_usd": 50.3011672},
                         "transcript_path": "/private/conversation", "private_message": "private"});
    save_cost(dir.path(), &payload, 1_800_000_000.75);
    let path = dir.path().join("claude-session-123.json");
    assert!(path.exists(), "Claude's running dollar total must reach the API-cost collector");
    let raw = fs::read(&path).unwrap();
    let saved: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        saved,
        json!({"type": "cost-state", "sessionId": "session-123",
                           "totalCostUSD": 50.3011672, "through": 1_800_000_000.75})
    );
    assert_eq!(raw.last(), Some(&b'\n'), "the incremental collector consumes complete JSON lines");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn cost_snapshot_ignores_idle_reports_and_advances_when_cost_grows() {
    let dir = TempDir::new("statusline-cost-order");
    let report = |cost| json!({"session_id": "one", "cost": {"total_cost_usd": cost}});
    let saved = || serde_json::from_slice::<Value>(&fs::read(dir.path().join("claude-one.json")).unwrap()).unwrap();
    save_cost(dir.path(), &report(10.0), 100.5);
    assert!(dir.path().join("claude-one.json").exists(), "running costs must be persisted");
    save_cost(dir.path(), &report(5.0), 110.0);
    save_cost(dir.path(), &report(10.0), 120.0);
    assert_eq!(saved()["through"], 100.5, "repeated totals must keep their original coverage time");
    save_cost(dir.path(), &report(11.0), 130.75);
    assert_eq!(saved()["totalCostUSD"], 11.0);
    assert_eq!(saved()["through"], 130.75);
}

#[test]
fn cost_snapshots_reject_invalid_ids_values_and_symlink_targets() {
    let dir = TempDir::new("statusline-cost-invalid");
    for session in ["", "../escape", "/absolute", "one/two", "one\\two", &"x".repeat(129)] {
        save_cost(dir.path(), &json!({"session_id": session, "cost": {"total_cost_usd": 1.0}}), 100.0);
    }
    for cost in [json!(-1), json!("1"), Value::Null] {
        save_cost(dir.path(), &json!({"session_id": "one", "cost": {"total_cost_usd": cost}}), 100.0);
    }
    save_cost(dir.path(), &json!({"session_id": "one"}), 100.0);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    let protected = dir.path().join("protected");
    fs::write(&protected, b"preserve").unwrap();
    std::os::unix::fs::symlink(&protected, dir.path().join("claude-one.json")).unwrap();
    std::os::unix::fs::symlink(&protected, dir.path().join("claude-two.json.lock")).unwrap();
    for session in ["one", "two"] {
        save_cost(dir.path(), &json!({"session_id": session, "cost": {"total_cost_usd": 1.0}}), 100.0);
    }
    assert_eq!(fs::read(&protected).unwrap(), b"preserve");
    assert!(!dir.path().join("claude-two.json").exists());
}

#[test]
fn concurrent_cost_reports_preserve_the_largest_total() {
    let dir = TempDir::new("statusline-cost-concurrent");
    let writers: Vec<_> = (1..=8)
        .map(|cost| {
            let path = dir.path().to_path_buf();
            std::thread::spawn(move || {
                save_cost(&path, &json!({"session_id": "one", "cost": {"total_cost_usd": cost}}), cost as f64);
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let saved: Value = serde_json::from_slice(&fs::read(dir.path().join("claude-one.json")).unwrap()).unwrap();
    assert_eq!(saved["totalCostUSD"], 8.0);
    assert_eq!(saved["through"], 8.0);
}

#[test]
fn profile_usage_cannot_follow_an_account_switch_or_merge_other_account_limits() {
    use crate::{hosts::logins, protocol::Tool, providers::accounts::Profile};
    let dir = TempDir::new("profile-usage");
    let profile = Profile { provider: Tool::Claude, directory: dir.path().join("profile") };
    let login = |id: &str| json!({"claudeAiOauth":{"refreshToken":"synthetic","expiresAt":2000},"oauthAccount":{"accountUuid":id}});
    logins::import(&profile, logins::LoginTool::Claude, &login("first")).unwrap();
    let limits = |used| json!({"five_hour":{"used_percentage":used,"resets_at":500}});
    save_profile_usage(&profile.directory, "first", None, &limits(99), 100);
    assert_eq!(profile.usage(100)[0].used_percent.unwrap(), 99.0);
    let launched = profile.launch_env();
    for name in crate::paths::env_names("ACCOUNT_ID") {
        assert_eq!(launched.iter().find(|(key, _)| key == &name).unwrap().1, "first");
    }
    logins::import(&profile, logins::LoginTool::Claude, &login("second")).unwrap();
    assert!(profile.usage(100).is_empty(), "old account usage is unknown after manual sign-in changes");
    save_profile_usage(&profile.directory, "second", None, &limits(10), 101);
    assert_eq!(profile.usage(101)[0].used_percent.unwrap(), 10.0, "old account's high reading is not merged");
    save_profile_usage(&profile.directory, "first", None, &limits(100), 102);
    assert_eq!(
        profile.usage(102)[0].used_percent.unwrap(),
        10.0,
        "an older live session cannot overwrite another account observation"
    );
}
