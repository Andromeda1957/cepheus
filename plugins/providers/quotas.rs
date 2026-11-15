//! Account allowances through the providers' own metadata commands. Their CLIs
//! own authentication and refresh; no credential or inference prompt is read here.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::protocol::{Tool, Usage, UsageWindow};

const LIMIT: Duration = Duration::from_secs(20);

/// How long a metadata child that answered has to exit by itself once its
/// input closes, before its process group is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// The least time between two reads of one provider's allowance. A read
/// starts that provider's CLI (`agy` took about 3 s of CPU on 2026-11-08), so
/// turns that end sooner share one later read.
const READ_GAP: Duration = Duration::from_secs(300);

/// When each provider's allowance is read: only after one of its turns ends,
/// at most once per gap. A turn that ends during the gap, or during the read
/// itself, is answered by one read when the gap has passed.
#[derive(Debug)]
pub(crate) struct Schedule {
    gap: Duration,
    /// Providers with a turn ended since their last read began.
    due: Vec<Tool>,
    /// When each provider's last read began.
    began: Vec<(Tool, Instant)>,
}

impl Schedule {
    pub(crate) fn new(gap: Duration) -> Self {
        Schedule { gap, due: Vec::new(), began: Vec::new() }
    }

    pub(crate) fn turn_ended(&mut self, tool: Tool) {
        if !self.due.contains(&tool) {
            self.due.push(tool);
        }
    }

    fn ready_at(&self, tool: Tool, now: Instant) -> Instant {
        self.began.iter().find(|(t, _)| *t == tool).map_or(now, |(_, at)| *at + self.gap)
    }

    /// The provider to read now, else how long until one is due; `None`
    /// when no turn has ended since the last reads.
    pub(crate) fn next(&mut self, now: Instant) -> Result<Tool, Option<Duration>> {
        match self.due.iter().position(|tool| self.ready_at(*tool, now) <= now) {
            Some(i) => {
                let tool = self.due.remove(i);
                self.began.retain(|(t, _)| *t != tool);
                self.began.push((tool, now));
                Ok(tool)
            }
            None => Err(self.due.iter().map(|tool| self.ready_at(*tool, now).saturating_duration_since(now)).min()),
        }
    }
}

#[derive(Debug)]
struct Readings {
    schedule: Schedule,
    grok: Vec<UsageWindow>,
    gemini: Vec<UsageWindow>,
    claude_models: Vec<UsageWindow>,
}

/// Grok's and Gemini's allowances and Claude's per-model limits, read by one
/// thread on its own schedule (`Schedule`), so reads never overlap and never
/// block the service.
#[derive(Debug)]
pub struct Refresh {
    commands: Vec<(Tool, Vec<String>)>,
    cwd: PathBuf,
    readings: Mutex<Readings>,
    wake: Condvar,
}

impl Refresh {
    /// `carried` holds the readings an in-place update handed over; they show
    /// until their reset or the next read.
    pub fn start(commands: Vec<(Tool, Vec<String>)>, cwd: PathBuf, carried: &Usage) -> Arc<Self> {
        let shared = |windows: &[UsageWindow]| windows.iter().filter(|w| w.used_percent.is_some()).cloned().collect();
        let mut gemini: Vec<UsageWindow> = shared(&carried.gemini);
        // Builds before 2026-11-08 named windows by their pool ("Gemini Models
        // weekly"); those wait for the next read.
        gemini.retain(|w| !w.label.contains(' ') || w.label.starts_with("Argon "));
        let readings = Readings {
            schedule: Schedule::new(READ_GAP),
            grok: shared(&carried.grok),
            gemini,
            claude_models: shared(&carried.claude_models),
        };
        let refresh = Arc::new(Refresh { commands, cwd, readings: Mutex::new(readings), wake: Condvar::new() });
        if !refresh.commands.is_empty() {
            let reader = refresh.clone();
            let _ = std::thread::Builder::new().name("provider-quotas".into()).spawn(move || reader.run());
        }
        refresh
    }

    /// A hosted session of `tool` finished a turn, so its allowance changed.
    pub fn turn_ended(&self, tool: Tool) {
        if self.commands.iter().any(|(t, _)| *t == tool) {
            self.lock().schedule.turn_ended(tool);
            self.wake.notify_one();
        }
    }

    /// Puts the current readings in `usage`, without those whose reset passed.
    pub fn fill(&self, usage: &mut Usage, now: u64) {
        let current = |windows: &[UsageWindow]| {
            windows.iter().filter(|w| w.resets_at.is_none_or(|at| at > now)).cloned().collect()
        };
        let readings = self.lock();
        usage.grok = current(&readings.grok);
        usage.gemini = current(&readings.gemini);
        usage.claude_models = current(&readings.claude_models);
    }

    fn lock(&self) -> MutexGuard<'_, Readings> {
        self.readings.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn run(&self) {
        let mut readings = self.lock();
        loop {
            match readings.schedule.next(Instant::now()) {
                Ok(tool) => {
                    drop(readings);
                    let windows = self.read(tool);
                    readings = self.lock();
                    // A failed read clears the old reading instead of
                    // presenting it as current.
                    match tool {
                        Tool::Grok => readings.grok = windows,
                        Tool::Antigravity => readings.gemini = windows,
                        Tool::Claude => readings.claude_models = windows,
                        _ => {}
                    }
                }
                Err(Some(wait)) => {
                    readings = self.wake.wait_timeout(readings, wait).map_or_else(|p| p.into_inner().0, |(g, _)| g)
                }
                Err(None) => readings = self.wake.wait(readings).unwrap_or_else(|p| p.into_inner()),
            }
        }
    }

    fn read(&self, tool: Tool) -> Vec<UsageWindow> {
        let Some((_, command)) = self.commands.iter().find(|(t, _)| *t == tool) else { return Vec::new() };
        let now = crate::util::now_secs();
        match tool {
            Tool::Antigravity => gemini(command, &self.cwd).map(|v| gemini_windows(&v, now)).unwrap_or_default(),
            Tool::Grok => grok(command, &self.cwd).map(|v| grok_windows(&v, now)).unwrap_or_default(),
            Tool::Claude => claude(command, &self.cwd).map(|v| claude_model_windows(&v, now)).unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

fn epoch(value: &Value) -> Option<u64> {
    value
        .as_str()
        .filter(|text| text.len() <= 80)
        .and_then(crate::hosts::logins::rfc3339_ms)
        .and_then(|at| u64::try_from(at).ok())
        .map(|at| at / 1000)
}

/// Missing or malformed percentages stay unknown, rather than looking unused.
fn percent(value: &Value) -> Option<f64> {
    value.as_f64().filter(|p| p.is_finite() && (0.0..=100.0).contains(p))
}

/// Gemini's own pools from Antigravity's `/usage`: its models' weekly and 5-hour limits, labelled `wk` and
/// `5h`, and Argon's, labelled `Argon wk`. The third-party pool (bucket ids `3p-*`, Claude and GPT models) is
/// left out. Pools are told apart by their bucket ids, not display names.
pub fn gemini_windows(value: &Value, now: u64) -> Vec<UsageWindow> {
    if value["status"] != "SUCCESS" || value["command"]["name"] != "usage" {
        return Vec::new();
    }
    let Some(groups) = value["command"]["data"]["groups"].as_array() else { return Vec::new() };
    let mut windows: Vec<UsageWindow> = Vec::new();
    for group in groups.iter().take(32) {
        let Some(buckets) = group["buckets"].as_array() else { continue };
        for bucket in buckets.iter().take(8) {
            let Some(id) = bucket["id"].as_str().filter(|id| valid_label(id)) else { continue };
            if id.split('-').next() != Some("gemini") {
                continue;
            }
            let Some(window) = bucket["window"].as_str().filter(|name| valid_label(name)) else { continue };
            let window = if window == "weekly" { "wk" } else { window };
            let label =
                if id.split('-').any(|part| part == "argon") { format!("Argon {window}") } else { window.into() };
            let resets_at = epoch(&bucket["reset_time"]);
            if resets_at.is_some_and(|at| at <= now) {
                continue;
            }
            // A pool without a reported share is not shown as a reading.
            let Some(used_percent) = bucket["remaining_fraction"]
                .as_f64()
                .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
                .map(|remaining| 100.0 * (1.0 - remaining))
            else {
                continue;
            };
            if windows.iter().any(|w| w.label == label) {
                continue;
            }
            windows.push(UsageWindow { label, used_percent: Some(used_percent), resets_at, updated_at: now });
        }
    }
    windows
}

fn valid_label(label: &str) -> bool {
    !label.trim().is_empty() && label.len() <= 100 && !label.chars().any(char::is_control)
}

/// Claude's per-model limits from the usage rows its `get_usage` reply carries (`rate_limits.limits`), each
/// labelled with its model and window: `Fable wk`. Rows without a model are the account's 5-hour and weekly
/// limits, which the status line already reports; rows scoped to a surface rather than a model are left out.
/// Rows are told apart by their scope, never by a display label.
pub fn claude_model_windows(value: &Value, now: u64) -> Vec<UsageWindow> {
    let Some(rows) = value["rate_limits"]["limits"].as_array() else { return Vec::new() };
    let mut windows: Vec<UsageWindow> = Vec::new();
    for row in rows.iter().take(64) {
        let Some(model) = row["scope"]["model"]["display_name"].as_str().filter(|name| valid_label(name)) else {
            continue;
        };
        let window = match row["group"].as_str() {
            Some("weekly") => "wk",
            Some("session") => "5h",
            Some(group) if valid_label(group) && !group.contains(char::is_whitespace) => group,
            _ => continue,
        };
        // A row without a reported share is not shown as a reading.
        let Some(used_percent) = percent(&row["percent"]) else { continue };
        let resets_at = epoch(&row["resets_at"]);
        if resets_at.is_some_and(|at| at <= now) {
            continue;
        }
        let label = format!("{} {window}", model.trim());
        if windows.iter().any(|w| w.label == label) {
            continue;
        }
        windows.push(UsageWindow { label, used_percent: Some(used_percent), resets_at, updated_at: now });
    }
    windows
}

/// Grok's plan allowance from its ACP billing reply: `creditUsagePercent` of
/// the current period, which ends at the reset. A reply without the share has
/// no reading to show. Measured 2026-11-08 (Grok 1.0.77, X Premium, no Grok
/// turn that week): the reply named the weekly period and its end, but no share.
pub fn grok_windows(value: &Value, now: u64) -> Vec<UsageWindow> {
    let config = &value["config"];
    if !config.is_object() {
        return Vec::new();
    }
    let period = &config["currentPeriod"];
    let resets_at = epoch(&period["end"]).or_else(|| epoch(&config["billingPeriodEnd"]));
    if resets_at.is_some_and(|at| at <= now) {
        return Vec::new();
    }
    let Some(used_percent) = percent(&config["creditUsagePercent"]) else { return Vec::new() };
    let label = match period["type"].as_str() {
        Some("USAGE_PERIOD_TYPE_WEEKLY") => "wk",
        Some("USAGE_PERIOD_TYPE_DAILY") => "day",
        Some("USAGE_PERIOD_TYPE_MONTHLY") => "mo",
        _ => "quota",
    };
    vec![UsageWindow { label: label.into(), used_percent: Some(used_percent), resets_at, updated_at: now }]
}

/// Whether launch arguments only choose how tool calls are approved: the owner's hosted agy starts with
/// `--dangerously-skip-permissions` and a session-only `--settings {"agentMode":""}`, Default mode rather than
/// the Auto mode agy 1.5.7 made its default. A `/usage` print calls no tools, so the read leaves these out.
fn approval_only(args: &[String]) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dangerously-skip-permissions" => {}
            "--settings" => match args.next().and_then(|value| serde_json::from_str::<Value>(value).ok()) {
                Some(Value::Object(settings)) if settings.keys().all(|key| key == "agentMode") => {}
                _ => return false,
            },
            _ => return false,
        }
    }
    true
}

pub fn gemini(command: &[String], cwd: &Path) -> Option<Value> {
    // Interactive launch flags can disable slash commands or supply a prompt.
    // Any other launch argument leaves the allowance unavailable instead of
    // risking a model turn.
    let (program, args) = command.split_first()?;
    if !approval_only(args) {
        return None;
    }
    let mut argv = vec![program.clone()];
    argv.extend(
        [
            "--print",
            "/usage",
            "--output-format",
            "json",
            "--disable-slash-commands=false",
            "--settings",
            "{\"disableSlashCommands\":false}",
            "--print-timeout",
            "15s",
        ]
        .map(str::to_string),
    );
    let result = crate::run::run(&argv, Some(cwd), &crate::client_temp::vars("quota").ok()?, LIMIT).ok()?;
    if !result.ok() {
        return None;
    }
    serde_json::from_str(&result.stdout).ok()
}

struct MetadataChild(Child);
impl Drop for MetadataChild {
    fn drop(&mut self) {
        // SAFETY: this private child started its own process group and has not
        // been reaped. It cannot name an owner session or a reused group ID.
        unsafe {
            libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What one line a metadata child printed means to `exchange`.
enum Next {
    /// Not the answer yet.
    Wait,
    /// Write this request next.
    Send(Value),
    /// The answer; `None` when it carries nothing usable.
    Done(Option<Value>),
}

/// Exchange JSON lines with a private metadata child: write `first`, then
/// hand each line it prints to `on_line` until that gives the answer. Input
/// stays open until then: Grok drops outstanding requests on EOF. Once
/// answered, the input closes and the child has `EXIT_GRACE` to exit by itself
/// before its process group is killed, so a program that saves its state on
/// the way out is not cut off.
fn exchange(mut command: Command, first: &Value, mut on_line: impl FnMut(&Value) -> Next) -> Option<Value> {
    command.process_group(0).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    crate::client_temp::contain(&mut command, "quota").ok()?;
    let mut child = MetadataChild(command.spawn().ok()?);
    let mut input = child.0.stdin.take()?;
    let mut output = child.0.stdout.take()?;
    // SAFETY: stdout owns this valid pipe; only its nonblocking flag changes.
    unsafe {
        let flags = libc::fcntl(output.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return None;
        }
    }
    writeln!(input, "{first}").ok()?;
    let deadline = Instant::now() + LIMIT;
    let mut pending = Vec::new();
    let mut total = 0;
    while Instant::now() < deadline {
        let mut bytes = [0u8; 8192];
        match output.read(&mut bytes) {
            Ok(0) => return None,
            Ok(n) => {
                total += n;
                if total > crate::run::OUTPUT_CAP {
                    return None;
                }
                pending.extend_from_slice(&bytes[..n]);
                while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    let Ok(message) = serde_json::from_slice::<Value>(&line) else { continue };
                    match on_line(&message) {
                        Next::Wait => {}
                        Next::Send(request) => writeln!(input, "{request}").ok()?,
                        Next::Done(answer) => {
                            drop(input);
                            drain_until_exit(&mut output);
                            return answer;
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    None
}

/// Discard what a child still prints until it closes its output by exiting,
/// for at most `EXIT_GRACE`. The child is not reaped here, so its process
/// group stays reserved for `MetadataChild`'s kill.
fn drain_until_exit(output: &mut impl Read) {
    let deadline = Instant::now() + EXIT_GRACE;
    let mut bytes = [0u8; 8192];
    while Instant::now() < deadline {
        match output.read(&mut bytes) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// ACP billing is an extension request, never a session/new or session/prompt.
pub fn grok(command: &[String], cwd: &Path) -> Option<Value> {
    if command.len() != 1 {
        return None;
    }
    let mut grok = Command::new(&command[0]);
    grok.args(["agent", "--no-leader", "stdio"]).current_dir(cwd);
    let initialize = json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
        "protocolVersion":1, "clientCapabilities":{}, "clientInfo":{"name":"cepheus-quota", "version":"0.1"}
    }});
    let mut billing = Some(json!({"jsonrpc":"2.0", "id":2, "method":"_x.ai/billing",
        "params":{"cwd":cwd.to_string_lossy(), "format":"credits"}}));
    exchange(grok, &initialize, |reply| match billing.take() {
        Some(request) if reply["id"] == 1 => {
            if reply["result"].is_object() {
                Next::Send(request)
            } else {
                Next::Done(None)
            }
        }
        Some(request) => {
            billing = Some(request);
            Next::Wait
        }
        None if reply["id"] == 2 => Next::Done(reply.get("result").filter(|r| r.is_object()).cloned()),
        None => Next::Wait,
    })
}

/// Claude's usage, rows included, through its `get_usage` control request to
/// a print-mode session that is sent no prompt, so no model turn runs.
/// Claude reads its own login. No settings sources are loaded, so the owner's
/// hooks, plugins and MCP servers do not start for a read, and the scan of
/// local transcripts behind `/usage`'s advice is skipped: 1.5 s against 9 s for
/// `claude --print /usage` (Claude Code 2.1.322, 2026-11-12).
pub fn claude(command: &[String], cwd: &Path) -> Option<Value> {
    if command.len() != 1 {
        return None;
    }
    let mut claude = Command::new(&command[0]);
    claude
        .args(["--print", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"])
        .args(["--setting-sources", "", "--strict-mcp-config", "--no-session-persistence"])
        .current_dir(cwd);
    const ID: &str = "cepheus-usage";
    let request = json!({"type": "control_request", "request_id": ID,
        "request": {"subtype": "get_usage", "skip_behaviors": true}});
    exchange(claude, &request, |message| {
        let reply = &message["response"];
        if message["type"] != "control_response" || reply["request_id"] != ID {
            return Next::Wait;
        }
        let usage = Some(&reply["response"]).filter(|usage| reply["subtype"] == "success" && usage.is_object());
        Next::Done(usage.cloned())
    })
}

#[cfg(test)]
#[path = "../../src/tests/provider_quotas.rs"]
mod tests;
