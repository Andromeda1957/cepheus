//! Codex transient-error retry policy; session core only schedules returned work.
use super::*;

/// Automatic "continue" after an error: the first waits this long, each
/// further one twice as long as the one before, up to `AUTO_CONTINUE_MAX_WAIT`.
pub(super) const AUTO_CONTINUE_FIRST_WAIT: Duration = Duration::from_secs(10);
pub(super) const AUTO_CONTINUE_MAX_WAIT: Duration = Duration::from_secs(300);
/// At most this many automatic "continue"s per session in `AUTO_CONTINUE_WINDOW`.
pub(super) const AUTO_CONTINUE_LIMIT: usize = 8;
pub(super) const AUTO_CONTINUE_WINDOW: Duration = Duration::from_secs(1800);

/// Typing "continue" for a Codex session that stopped on a passing error such as "Selected model is at
/// capacity".
#[derive(Default)]
pub(super) struct AutoContinue {
    pub(super) quota_reset: Option<u64>,
    /// Require a newly parsed provider frame after session input.
    pub(super) input_revision: Option<u64>,
    pub(super) quota_unknown: bool,
    /// When the session was first seen waiting after the error.
    pub(super) seen: Option<Instant>,
    /// When "continue" was typed, within `AUTO_CONTINUE_WINDOW`.
    pub(super) sent: Vec<Instant>,
    /// The limit was reached and reported.
    pub(super) gave_up: bool,
}

/// What an automatic "continue" reports.
#[derive(Debug, PartialEq)]
pub(super) enum Retry {
    Typed(String),
    GaveUp(String),
}

impl AutoContinue {
    /// Whether to type "continue" now for a session waiting after an error,
    /// with the wait doubling after each recent attempt.
    pub(super) fn due(&mut self, now: Instant) -> bool {
        self.sent.retain(|at| now.duration_since(*at) < AUTO_CONTINUE_WINDOW);
        if self.sent.len() >= AUTO_CONTINUE_LIMIT {
            return false;
        }
        let seen = *self.seen.get_or_insert(now);
        let wait = AUTO_CONTINUE_FIRST_WAIT.saturating_mul(1 << self.sent.len().min(8)).min(AUTO_CONTINUE_MAX_WAIT);
        now.duration_since(seen) >= wait
    }
}

pub(super) struct RetryContext<'a> {
    pub now: Instant,
    pub now_ms: u64,
    pub usage: &'a crate::protocol::Usage,
    pub threshold: f64,
    pub input_ready: bool,
}

impl Sess {
    /// Type "continue" into a Codex session that stopped on one of the
    /// `patterns` errors and waits with an empty input box, once its wait is
    /// over; the Enter follows after `SUBMIT_DELAY`. Returns what to report.
    pub(super) fn retry_after_error(
        &mut self,
        lines: &[String],
        cursor: Option<(usize, usize)>,
        patterns: &[String],
        context: RetryContext<'_>,
    ) -> Option<Retry> {
        let RetryContext { now, now_ms, usage, threshold, input_ready } = context;
        let quota = quota_stopped(self.tool, lines, cursor);
        if self.recovery.paused || self.recovery.phase.is_some() {
            self.retry.seen = None;
            return None;
        }
        let stopped = quota.or_else(|| status::stopped_on_error(lines, cursor, patterns)).filter(|pattern| {
            // A revoked turn has completed even if its persistent goal or
            // background shell survives. Require the monitor's completed-turn
            // evidence and a quiet screen before retrying in that case.
            let revoked_turn_ended = pattern.eq_ignore_ascii_case("application network permission was revoked")
                && self.tracker.tool_busy == Some(false)
                && !crate::providers::screen_signals(self.tool, lines, self.hosted.as_ref().and_then(|h| h.pid))
                    .working;
            (!self.tracker.is_working(now_ms) || revoked_turn_ended) && self.tracker.needs_reason().is_none()
        });
        let Some(pattern) = stopped else {
            self.retry.seen = None;
            self.retry.quota_reset = None;
            self.retry.quota_unknown = false;
            return None;
        };
        if quota.is_some() {
            let now_secs = now_ms / 1000;
            let windows = match self.tool {
                Tool::Claude => &usage.claude,
                Tool::Codex => &usage.codex,
                _ => return None,
            };
            let high: Vec<_> =
                windows.iter().filter(|w| w.used_percent.is_some_and(|p| p.is_finite() && p >= threshold)).collect();
            if quota == Some("provider credits exhausted") || high.iter().any(|w| w.resets_at.is_none()) {
                self.retry.quota_reset = None;
                self.retry.quota_unknown = true;
                return None;
            }
            if let Some(reset) = high.iter().filter_map(|w| w.resets_at).max() {
                self.retry.quota_reset = Some(reset);
                self.retry.quota_unknown = false;
            }
            let Some(reset) = self.retry.quota_reset else {
                self.retry.quota_unknown = true;
                return None;
            };
            if now_secs < reset {
                return None;
            }
        } else {
            self.retry.quota_reset = None;
            self.retry.quota_unknown = false;
        }
        if !input_ready {
            return None;
        }
        let h = self.hosted.as_ref().filter(|_| self.alive)?;
        if !h.lock().term.mode().contains(TermMode::BRACKETED_PASTE) {
            return None;
        }
        let who = session_label(self.tool, &self.title);
        if !self.retry.due(now) {
            if self.retry.sent.len() >= AUTO_CONTINUE_LIMIT && !self.retry.gave_up {
                self.retry.gave_up = true;
                return Some(Retry::GaveUp(format!(
                    "{who}: still \"{pattern}\" after {AUTO_CONTINUE_LIMIT} automatic continues; it waits for you"
                )));
            }
            return None;
        }
        h.follow_output();
        h.write(crate::keys::encode_message(&crate::keys::non_owner_message("continue")).into_bytes());
        self.recovery.replay_submitted = Some("continue".into());
        self.typed_input();
        self.retry.sent.push(now);
        self.retry.seen = None;
        self.retry.quota_reset = None;
        self.retry.quota_unknown = false;
        self.retry.gave_up = false;
        Some(Retry::Typed(format!(
            "{who}: \"{pattern}\"; typed continue ({} of {AUTO_CONTINUE_LIMIT} in 30 minutes)",
            self.retry.sent.len()
        )))
    }
}

pub(super) fn patterns(tool: Tool, config: &Config) -> &[String] {
    if tool == Tool::Codex {
        &config.auto_continue
    } else {
        &[]
    }
}

/// Quota policy stays with provider adapters. An unknown reset is explicitly held.
pub(crate) fn quota_hold(tool: Tool, usage: &crate::protocol::Usage, threshold: f64, now: u64) -> Option<Option<u64>> {
    let windows = match tool {
        Tool::Claude => &usage.claude,
        Tool::Codex => &usage.codex,
        _ => return None,
    };
    let high: Vec<_> = windows
        .iter()
        .filter(|w| {
            w.used_percent.is_some_and(|p| p.is_finite() && p >= threshold) && w.resets_at.is_none_or(|at| at > now)
        })
        .collect();
    if high.is_empty() {
        None
    } else if high.iter().any(|w| w.resets_at.is_none()) {
        Some(None)
    } else {
        Some(high.iter().filter_map(|w| w.resets_at).max())
    }
}

/// Recognize only provider error chrome immediately above an empty input.
pub(super) fn quota_stopped(tool: Tool, lines: &[String], cursor: Option<(usize, usize)>) -> Option<&'static str> {
    let (x, y) = cursor?;
    let typed: String = lines.get(y)?.chars().take(x).collect();
    let marker = match tool {
        Tool::Codex => '›',
        Tool::Claude => '❯',
        _ => return None,
    };
    if !typed.trim_start().strip_prefix(marker)?.trim().is_empty() {
        return None;
    }
    for line in lines[..y].iter().rev().filter(|l| !l.trim().is_empty()).take(3) {
        let text = line.trim();
        let provider_error = match tool {
            Tool::Codex => line.starts_with('■'),
            Tool::Claude => text.starts_with("⎿") || text.starts_with("API Error: 429"),
            _ => false,
        };
        if !provider_error {
            continue;
        }
        let lower = text.to_lowercase().replace('’', "'");
        if lower.contains("usage credits")
            || lower.contains("extra usage")
            || lower.contains("workspace credit limit")
            || lower.contains("org is out of usage")
        {
            return Some("provider credits exhausted");
        }
        if lower.contains("you've reached your")
            || lower.contains("you've hit your limit")
            || lower.contains("you've hit your usage limit")
            || lower.contains("usage limit reached")
            || lower.contains("rate limit reached")
        {
            return Some("provider usage limit");
        }
    }
    None
}
