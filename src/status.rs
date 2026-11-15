//! Session status: screen heuristics and the per-session state machine that
//! decides when a session is working, needs the owner, or has finished.

use crate::protocol::{Status, QUEUED_QUESTION};
use crate::util;

pub use crate::providers::screen::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Notify {
    NeedsYou(String),
    Finished,
}

/// A question with the same reason is announced at most once in this long.
const NEEDS_REPEAT_MS: u64 = 30_000;

/// How long a screen activity indicator keeps a session "working" after it
/// was last seen, smoothing over redraw gaps.
const SCREEN_WORKING_HOLD_MS: u64 = 1500;
const STARTING_GRACE_MS: u64 = 5000;
/// How long after the program's log said its turn was over the screen may
/// still show it running: the log and its reading lag the screen.
const LOG_LAG_MS: u64 = 10_000;
/// The hold once the log is shown not to be the session's: its working line
/// is all there is, and Codex hides it for seconds at a time mid-turn.
const UNCONFIRMED_HOLD_MS: u64 = 30_000;
/// How long after a reported finish a working signal may still belong to that turn. Claude runs Stop hooks
/// concurrently and can keep its spinner and busy status until the last hook ends. The hold ends sooner when
/// both screen and log are quiet; work started by a Stop hook remains busy beyond the hold.
const FINISH_SETTLE_MS: u64 = 15_000;

/// The hold on working signals after a finished turn (`FINISH_SETTLE_MS`).
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Settle {
    pub until_ms: u64,
    /// The screen has shown no working line since the finish.
    pub screen_quiet: bool,
    /// The program's log has not said busy since the finish.
    pub log_quiet: bool,
}

/// Carried to the next build of the service when it updates in place
/// (`handover.rs`), so a session keeps its status; its times are wall-clock.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tracker {
    pub status: Status,
    pub since: u64,
    /// Monotonic completed turns, retained through handover and chat changes.
    #[serde(default)]
    pub completed_turns: u64,
    /// The last turn finished and the owner has not moved on from it
    /// (`clear_finished`). Looking at the session cleared it until
    /// 2026-10-27; the name stays for the handover format.
    pub unseen: bool,
    pub ever_active: bool,
    pub screen_working_until_ms: u64,
    pub screen_prompt: Option<String>,
    pub hook_needs: Option<String>,
    pub tool_busy: Option<bool>,
    /// The program reported its turn's end; `update` applies the report once
    /// nothing asks.
    pub pending_finish: bool,
    pub spawned_ms: u64,
    /// The program reports the end of each turn (Codex's notify hook, Claude's Stop hook), so only that report
    /// finishes a turn. The working line on screen comes and goes mid-turn: Codex hides it while it streams,
    /// and each gap was announced as "finished".
    pub finish_hook: bool,
    /// The last question announced and when.
    pub announced: Option<(String, u64)>,
    /// The last turn ended with background work still running (tests, a monitor, a sub-agent) that wakes the
    /// program when it ends, so the session is still at work.
    #[serde(default)]
    pub background: bool,
    /// What that work is, as the session's detail names it ("a shell").
    #[serde(default)]
    pub background_work: Option<String>,
    /// An active persistent goal spans individual completed turns.
    #[serde(default)]
    pub goal_active: bool,
    /// Since when the program's log has said its turn is over (unix ms).
    #[serde(default)]
    pub log_idle_since_ms: Option<u64>,
    /// The last turn just finished and its Stop hooks may still be running.
    #[serde(default)]
    pub settle: Option<Settle>,
    /// The screen shows Codex questions queued behind its own key
    /// (`ScreenSignals::queued_question`).
    #[serde(default)]
    pub screen_queued_question: bool,
    /// The hook's question (`hook_needs`) has not been on screen yet. Once
    /// it has, a screen without it means it closed, answered or not
    /// (`observe_screen`). A tracker handed over from an older build lacks
    /// it and counts as seen, so a stale question it carries ends too.
    #[serde(default)]
    pub hook_awaits_screen: bool,
    /// A persistent connection (Dot) rather than a worker: a turn ending
    /// returns it to Idle (shown Ready), never Finished or a completion alert.
    #[serde(default)]
    pub persistent: bool,
    /// The program reported this finished turn's end and it is not yet announced: `update` announces it once
    /// its Stop hooks are over with the session still finished. A Stop hook that sends the program on (decision
    /// "block") makes that report no finish.
    #[serde(default)]
    pub finish_unannounced: bool,
    /// The last turn's end was announced, seen, or moved on from, and the
    /// session has not been at work since: another report of an end is that
    /// same end again.
    #[serde(default)]
    pub finish_announced: bool,
    /// The latest completed turn the program's own record showed
    /// (`turn_recorded`).
    #[serde(default)]
    pub turn_end: Option<String>,
    /// A record read first that ended at or after this (unix ms) ended in
    /// this session's chat; one before it is where the record already stood.
    /// A tracker handed over without it takes the first record as it stands.
    #[serde(default = "never_ms")]
    pub turn_end_since_ms: u64,
}

fn never_ms() -> u64 {
    u64::MAX
}

impl Tracker {
    pub fn new(now_ms: u64, alive: bool) -> Self {
        Tracker {
            status: if alive { Status::Starting } else { Status::Ended },
            since: now_ms / 1000,
            completed_turns: 0,
            unseen: false,
            ever_active: false,
            screen_working_until_ms: 0,
            screen_prompt: None,
            hook_needs: None,
            tool_busy: None,
            pending_finish: false,
            spawned_ms: now_ms,
            finish_hook: false,
            announced: None,
            background: false,
            background_work: None,
            goal_active: false,
            log_idle_since_ms: None,
            settle: None,
            screen_queued_question: false,
            hook_awaits_screen: false,
            persistent: false,
            finish_unannounced: false,
            finish_announced: false,
            turn_end: None,
            turn_end_since_ms: now_ms,
        }
    }

    /// Whether a working signal now still belongs to the turn that just
    /// finished; ends the hold once it no longer can.
    fn settling(&mut self, now_ms: u64) -> bool {
        let held = self.settle.is_some_and(|s| now_ms < s.until_ms && !(s.screen_quiet && s.log_quiet));
        if !held {
            self.settle = None;
        }
        held
    }

    pub fn observe_screen(&mut self, signals: &ScreenSignals, now_ms: u64) {
        if !signals.working {
            if let Some(settle) = &mut self.settle {
                settle.screen_quiet = true;
            }
        } else if !self.settling(now_ms) {
            // A log that has long said the turn is over while the screen shows one running is not the session's
            // thread: a gap in the working line is no finish then.
            let stale = self.log_idle_since_ms.is_some_and(|since| now_ms >= since + LOG_LAG_MS);
            let hold = if stale { UNCONFIRMED_HOLD_MS } else { SCREEN_WORKING_HOLD_MS };
            self.screen_working_until_ms = now_ms + hold;
        }
        self.screen_prompt = signals.prompt.map(str::to_string);
        self.screen_queued_question = signals.queued_question;
        // The hook's question closed once the screen that showed it no
        // longer does: Claude Code denies one nobody answers and works on,
        // and the hook alone held the session at "needs you" (2026-11-02).
        if self.hook_needs.is_some() {
            if signals.prompt.is_some() {
                self.hook_awaits_screen = false;
            } else if !self.hook_awaits_screen {
                self.hook_needs = None;
            }
        }
    }

    /// What the program's log says of its turn (`SessionFacts::busy`).
    pub fn log_busy(&mut self, busy: Option<bool>, now_ms: u64) {
        if busy == Some(true) {
            if self.settling(now_ms) {
                return;
            }
        } else if let Some(settle) = &mut self.settle {
            settle.log_quiet = true;
        }
        match busy {
            Some(false) if self.tool_busy == Some(true) => {
                // The log saw the turn end: the working line goes with it.
                self.screen_working_until_ms = self.screen_working_until_ms.min(now_ms + SCREEN_WORKING_HOLD_MS);
                self.log_idle_since_ms = Some(now_ms);
            }
            Some(false) => {
                self.log_idle_since_ms.get_or_insert(now_ms);
            }
            _ => self.log_idle_since_ms = None,
        }
        self.tool_busy = busy;
    }

    pub fn observe_goal(&mut self, active: Option<bool>) {
        if let Some(active) = active {
            // A completion hook may have arrived while the monitor still saw
            // the goal active. Finish once its inactive status catches up.
            if self.goal_active && !active && self.tool_busy == Some(false) {
                self.pending_finish = true;
            }
            self.goal_active = active;
        }
    }

    /// A program reported that its turn ended (Codex notify hook).
    pub fn turn_complete(&mut self) {
        self.pending_finish = true;
        self.screen_working_until_ms = 0;
        self.tool_busy = Some(false);
        self.background = false;
        self.background_work = None;
    }

    /// The program's own record of its latest completed turn
    /// (`SessionFacts::turn_end`), for a program with no hook that reports
    /// the end of its turns: Grok, opencode, Antigravity, and a Codex run
    /// without its notify hook. A record not seen before is a turn's end,
    /// unless the record shows a newer turn begun since (`superseded`).
    pub fn turn_recorded(&mut self, id: &str, at_ms: u64, superseded: bool) {
        if self.finish_hook || self.turn_end.as_deref() == Some(id) {
            return;
        }
        let new = self.turn_end.is_some() || at_ms >= self.turn_end_since_ms;
        self.turn_end = Some(id.to_string());
        if new && !superseded {
            self.turn_complete();
        }
    }

    /// A turn ended but left background `work` running; the turn the program
    /// starts when that work ends reports the finish.
    pub fn turn_waits(&mut self, work: String) {
        self.background = true;
        self.background_work = Some(work);
    }

    /// The background work the session only waits on: none once the
    /// program takes up a turn again, which it may before that work ends.
    pub fn waiting_on(&self) -> Option<&str> {
        self.background_work.as_deref().filter(|_| self.background && self.tool_busy != Some(true))
    }

    /// The program's hook reported a question (Claude's permission prompt),
    /// which may not be drawn yet.
    pub fn hook_asks(&mut self, reason: String) {
        self.hook_needs = Some(reason);
        self.hook_awaits_screen = true;
    }

    /// The owner typed into the session, answering whatever it asked. A
    /// finished turn keeps its mark until the input is submitted.
    pub fn user_input(&mut self) {
        self.hook_needs = None;
    }

    /// The owner moved on from a finished turn: input was submitted, or the session began a new chat. Until
    /// then the session shows as finished, on screen or not.
    pub fn clear_finished(&mut self) {
        // A report of the end moved on from is no news to announce.
        if self.unseen || self.finish_unannounced {
            self.finish_announced = true;
        }
        self.unseen = false;
        self.settle = None;
        self.finish_unannounced = false;
    }

    pub fn is_working(&self, now_ms: u64) -> bool {
        self.goal_active || self.tool_busy == Some(true) || self.background || now_ms < self.screen_working_until_ms
    }

    pub fn needs_reason(&self) -> Option<&str> {
        self.hook_needs
            .as_deref()
            .or(self.screen_prompt.as_deref())
            .or(self.screen_queued_question.then_some(QUEUED_QUESTION))
    }

    /// Whether typed input or an Enter would answer what the session asks.
    /// Queued Codex questions open only with Codex's own key.
    pub fn blocks_input(&self) -> bool {
        self.hook_needs.is_some() || self.screen_prompt.is_some()
    }

    /// Recompute the status; returns a notification for the owner when a
    /// session they are not looking at changes state in a way they care about.
    pub fn update(&mut self, alive: bool, focused: bool, now_ms: u64) -> Option<Notify> {
        let prev = self.status;
        let needs = if alive { self.needs_reason().map(str::to_string) } else { None };
        // A question comes first, and the report of the turn's end waits
        // until nothing asks. The service applies a Stop hook at once but
        // reads screens every `SCREEN_SCAN_INTERVAL`, so a turn that ends
        // just after the owner answers is reported while the last read still
        // shows the question. Taken then, the report was lost: the session
        // went from "needs you" to idle, its turn neither counted nor
        // announced (2026-11-09).
        let finish_pending = needs.is_none() && std::mem::take(&mut self.pending_finish);
        if finish_pending {
            // Seen since the report, they are its Stop hooks still running.
            self.settle = Some(Settle { until_ms: now_ms + FINISH_SETTLE_MS, ..Settle::default() });
            self.screen_working_until_ms = 0;
            if self.tool_busy == Some(true) {
                self.tool_busy = Some(false);
            }
        }
        // A turn end wins over stale turn activity, but an active goal spans
        // turns. Prompts still take priority over either kind of work.
        let working = alive && (self.goal_active || (!finish_pending && self.is_working(now_ms)));
        let mut notify = None;
        let next = if !alive {
            Status::Ended
        } else if let Some(reason) = needs {
            let repeated = self
                .announced
                .as_ref()
                .is_some_and(|(said, at)| *said == reason && now_ms < at.saturating_add(NEEDS_REPEAT_MS));
            if prev != Status::NeedsYou && !focused && !repeated {
                self.announced = Some((reason.clone(), now_ms));
                notify = Some(Notify::NeedsYou(reason));
            }
            Status::NeedsYou
        } else if working {
            Status::Working
        } else {
            let ended = !self.persistent
                && if self.finish_hook { finish_pending } else { prev == Status::Working || finish_pending };
            if self.persistent {
                self.unseen = false;
            }
            if ended {
                self.completed_turns = self.completed_turns.saturating_add(1);
                // Marked even on screen. Only the program's own report of
                // its turn's end is announced: a screen or log that went
                // quiet is no evidence that the work is done. A report of
                // an end already announced or moved on from, with no work
                // seen since, is that end again (a hook delivered twice, or
                // late): still a turn for the work queue, but no news.
                if finish_pending && !self.finish_announced {
                    self.finish_unannounced = true;
                }
                self.unseen = true;
            }
            if self.unseen {
                Status::Finished
            } else if !self.ever_active && now_ms < self.spawned_ms + STARTING_GRACE_MS {
                Status::Starting
            } else {
                Status::Idle
            }
        };
        if matches!(next, Status::Working | Status::NeedsYou) {
            self.ever_active = true;
            self.unseen = false;
            self.finish_announced = false;
        }
        if next == Status::Finished || finish_pending {
            self.ever_active = true;
        }
        if next != Status::Finished {
            self.finish_unannounced = false;
        } else if self.finish_unannounced && self.settle.is_none_or(|s| s.screen_quiet && s.log_quiet) {
            // Its Stop hooks are over and nothing sent it on: announced
            // once, and only to an owner who is not watching it. The hold
            // ends without that only on a working signal, which is no
            // finish (`settling`).
            self.finish_unannounced = false;
            self.finish_announced = true;
            if !focused {
                notify = Some(Notify::Finished);
            }
        }
        if next != prev {
            self.status = next;
            self.since = now_ms / 1000;
        }
        notify
    }
}

/// Strip spinner glyphs and generic program names from a terminal title.
pub fn clean_title(raw: &str) -> Option<String> {
    let trimmed = raw.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '(' && c != '[');
    let title = util::one_line(trimmed);
    let lower = title.to_lowercase();
    if title.is_empty() || lower == "claude code" || lower == "claude" || lower == "codex" {
        return None;
    }
    Some(title)
}

#[cfg(test)]
#[path = "tests/status.rs"]
mod tests;
