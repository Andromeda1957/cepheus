//! `cepheus statusline`: the status line hosted Claude sessions run. It
//! prints a short usage line and saves the plan usage windows Claude reports
//! so the deck can show them, plus running session dollars for API accounting.

use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::{paths, util};

pub fn run() -> i32 {
    let mut buf = Vec::new();
    let _ = std::io::stdin().take(4 << 20).read_to_end(&mut buf);
    let input: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
    let now = util::now_secs();
    if let Some(limits) = input.get("rate_limits").filter(|r| r.is_object()) {
        let reporter = claude_session(&input);
        // A namespace binds the whole pair, even if one field is empty or
        // missing: never borrow another prefix's identity or write it globally.
        let binding = paths::env_names("ACCOUNT_DIRECTORY")
            .into_iter()
            .zip(paths::env_names("ACCOUNT_ID"))
            .map(|(directory, account)| (std::env::var_os(directory), std::env::var_os(account)))
            .find(|(directory, account)| directory.is_some() || account.is_some());
        match binding {
            Some((Some(root), Some(account))) => {
                if let Some(account) = account.to_str().filter(|id| !id.is_empty()) {
                    save_profile_usage(Path::new(&root), account, reporter, limits, now);
                }
            }
            None => save_usage(&paths::usage_file(), reporter, limits, now),
            _ => {}
        }
    }
    let now_ms = util::now_ms();
    save_cost(&paths::state_dir().join("api-cost"), &input, now_ms as f64 / 1000.0);
    println!("{}", render(&input, now));
    0
}

/// The Claude session a status line input comes from: its `session_id`, when
/// it is a plain id short enough to name a file.
fn claude_session(input: &Value) -> Option<&str> {
    input
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// Claude's running total includes internal calls absent from its transcript.
/// Keep a small accounting-only snapshot; unchanged costs keep their original
/// observation time so the collector can still add newer message receipts.
fn save_cost(dir: &Path, input: &Value, now: f64) {
    let Some(session) = claude_session(input) else { return };
    let Some(cost) = input.get("cost").and_then(|v| v.get("total_cost_usd")).and_then(Value::as_f64) else {
        return;
    };
    if !cost.is_finite() || cost < 0.0 || util::ensure_private_dir(dir).is_err() {
        return;
    }
    let path = dir.join(format!("claude-{session}.json"));
    if [&path, &lock_path(&path)].iter().any(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_symlink())) {
        return;
    }
    let Some(_lock) = lock(&path) else { return };
    let mut bytes = Vec::new();
    if let Ok(file) = fs::File::open(&path) {
        let _ = file.take(4096).read_to_end(&mut bytes);
    }
    let old = serde_json::from_slice::<Value>(&bytes).ok();
    if old.as_ref().and_then(|v| v.get("totalCostUSD")).and_then(Value::as_f64).is_some_and(|old| old >= cost) {
        return;
    }
    let body = json!({"type": "cost-state", "sessionId": session, "totalCostUSD": cost, "through": now});
    let _ = util::write_atomic(&path, format!("{body}\n").as_bytes());
}

fn window_text(label: &str, w: &Value, now: u64, with_reset: bool) -> Option<String> {
    let used = w.get("used_percentage").and_then(Value::as_f64)?;
    let reset = w.get("resets_at").and_then(util::json_epoch_secs).filter(|r| *r > now);
    Some(match reset {
        Some(r) if with_reset => format!("{label} {used:.0}% ({})", util::short_duration(r - now)),
        _ => format!("{label} {used:.0}%"),
    })
}

pub fn render(input: &Value, now: u64) -> String {
    let mut parts = Vec::new();
    if let Some(model) = input.get("model").and_then(|m| m.get("display_name")).and_then(Value::as_str) {
        parts.push(model.to_string());
    }
    if let Some(ctx) = input.get("context_window").and_then(|c| c.get("used_percentage")).and_then(Value::as_f64) {
        parts.push(format!("ctx {ctx:.0}%"));
    }
    if let Some(limits) = input.get("rate_limits") {
        if let Some(text) = limits.get("five_hour").and_then(|w| window_text("5h", w, now, true)) {
            parts.push(text);
        }
        if let Some(text) = limits.get("seven_day").and_then(|w| window_text("wk", w, now, false)) {
            parts.push(text);
        }
    }
    parts.join(" · ")
}

/// The usage windows the snapshot merges.
const WINDOWS: [&str; 2] = ["five_hour", "seven_day"];

/// Resets closer together than this belong to the same window: the shortest
/// window lasts five hours.
const SAME_WINDOW_SECS: u64 = 3600;

/// The windows to save when the Claude session `reporter` reports `limits` over the snapshot `old`, and what
/// each session last reported. Every Claude reports the account's limits as of its own last reply, again each
/// time its status line redraws, so an idle one repeats older numbers than the snapshot holds. A window that
/// has ended, or that ends before a saved one that has not, is an older reply's and never replaces it: a
/// session idle across a window's end keeps reporting the window it last saw. Within one window, a report a
/// session repeats unchanged is no news and the saved one stands; one it reports differently comes from a newer
/// reply and replaces the saved one, even with less use. Keeping the higher use within a window kept a used-up
/// week after a plan reset refilled it. A report without a session cannot be told from a repeat and is taken as
/// new.
fn merged(old: Option<&Value>, reporter: Option<&str>, limits: &Value, now: u64) -> (Value, Value) {
    let saved = old.and_then(|o| o.get("rate_limits")).filter(|r| r.is_object());
    let reports = old.and_then(|o| o.get("reports")).and_then(Value::as_object);
    let previous = reporter.and_then(|id| reports?.get(id));
    let resets = |w: &Value| w.get("resets_at").and_then(util::json_epoch_secs);
    let mut out = limits.clone();
    for key in WINDOWS {
        let (Some(was), Some(window)) = (saved.and_then(|s| s.get(key)).filter(|w| w.is_object()), limits.get(key))
        else {
            continue;
        };
        let keep = match (resets(was), resets(window)) {
            (Some(a), Some(b)) if a > now && (b <= now || b.saturating_add(SAME_WINDOW_SECS) <= a) => true,
            (Some(a), Some(b)) if b >= a.saturating_add(SAME_WINDOW_SECS) => false,
            _ => previous.and_then(|p| p.get(key)) == Some(window),
        };
        if keep {
            out[key] = was.clone();
        }
    }
    // A session's report is dropped once all its windows have ended: an ended
    // window never replaces a running one, so its repeats need no record.
    let mut kept: serde_json::Map<String, Value> = reports
        .into_iter()
        .flatten()
        .filter(|(id, report)| Some(id.as_str()) != reporter && current(report, now))
        .map(|(id, report)| (id.clone(), report.clone()))
        .collect();
    if let Some(reporter) = reporter {
        let own: serde_json::Map<String, Value> = WINDOWS
            .iter()
            .filter_map(|key| limits.get(*key).filter(|w| w.is_object()).map(|w| (key.to_string(), w.clone())))
            .collect();
        let own = Value::Object(own);
        if current(&own, now) {
            kept.insert(reporter.to_string(), own);
        }
    }
    (out, Value::Object(kept))
}

/// Whether any window of a session's report has not ended.
fn current(report: &Value, now: u64) -> bool {
    WINDOWS.iter().any(|key| {
        report.get(*key).and_then(|w| w.get("resets_at")).and_then(util::json_epoch_secs).is_some_and(|at| at > now)
    })
}

/// The lock file beside the usage snapshot.
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// Hold the snapshot's lock until the returned file is dropped. Every hosted
/// Claude runs its own status line, and two that read the same snapshot each
/// wrote their own merge, so the one that wrote last replaced the other's
/// higher use (review CR-20261026-d5c080aeb2).
fn lock(path: &Path) -> Option<fs::File> {
    let file =
        fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(lock_path(path)).ok()?;
    loop {
        // SAFETY: flock only reads the descriptor, which `file` keeps open.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Some(file);
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
}

/// Write the usage snapshot when it changed, or refresh it once a minute. The
/// read, merge and write happen under the snapshot's lock.
fn save_usage(path: &Path, reporter: Option<&str>, limits: &Value, now: u64) {
    save_usage_for(path, reporter, limits, now, None);
}

/// Profile quota observations stay separate from the default account cache.
pub(crate) fn save_profile_usage(root: &Path, account: &str, reporter: Option<&str>, limits: &Value, now: u64) {
    if !root.is_absolute() || root.parent().is_none() || account.is_empty() || account.len() > 256 {
        return;
    }
    let profile =
        crate::providers::accounts::Profile { provider: crate::protocol::Tool::Claude, directory: root.to_owned() };
    if crate::hosts::logins::state(&profile, crate::hosts::logins::LoginTool::Claude, now.saturating_mul(1000) as i64)
        .account
        .as_deref()
        != Some(account)
    {
        return;
    }
    let path = root.join(".agent-deck-usage.json"); // legacy-name
    if [&path, &lock_path(&path)].iter().any(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_symlink())) {
        return;
    }
    save_usage_for(&path, reporter, limits, now, Some(account));
}

fn save_usage_for(path: &Path, reporter: Option<&str>, limits: &Value, now: u64, account: Option<&str>) {
    if let Some(dir) = path.parent() {
        let _ = util::ensure_private_dir(dir);
    }
    let Some(_lock) = lock(path) else { return };
    let old = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|old| old.get("account").and_then(Value::as_str) == account);
    let (limits, reports) = merged(old.as_ref(), reporter, limits, now);
    if let Some(old) = &old {
        let same = old.get("rate_limits") == Some(&limits) && old.get("reports") == Some(&reports);
        let fresh = old.get("updated_at").and_then(util::json_epoch_secs).is_some_and(|t| now.saturating_sub(t) < 60);
        if same && fresh {
            return;
        }
    }
    let mut body = json!({ "updated_at": now, "rate_limits": limits, "reports": reports });
    if let Some(account) = account {
        body["account"] = json!(account);
        body["provider"] = json!("claude");
    }
    let _ = util::write_atomic(path, body.to_string().as_bytes());
}

#[cfg(test)]
#[path = "../../src/tests/statusline.rs"]
mod tests;
