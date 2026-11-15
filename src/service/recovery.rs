//! Relaunch a failed hosted client, preserving its conversation and owned input.
use std::io::{BufRead, Read, Seek, SeekFrom};

use serde::{Deserialize, Serialize};

use super::*;
use crate::providers::recovery::{
    codex_draft, codex_image_draft_recoverable, codex_reconnect_failed, codex_recovery_draft, codex_slash_command,
    logged_user, normalized, UserReceipt,
};

const INPUT_LIMIT: usize = 2 * 1024 * 1024;
/// How often a captured Codex session's missing conversation log is looked for.
const ROLLOUT_PROBE: Duration = Duration::from_secs(2);
const MESSAGE_LIMIT: usize = 64;
const INCOMPLETE_DRAFT: &str =
    "the failed Codex client contains an incomplete draft; it was preserved without relaunch";
const IMAGE_DRAFT: &str = "the image draft cannot be restored exactly; the client and input are preserved";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientRecovery {
    pub draft: Option<String>,
    pub pending: Vec<String>,
    pub rollout: Option<PathBuf>,
    pub offset: u64,
    pub attempts_ms: Vec<u64>,
    pub blocked: Option<String>,
    pub phase: Option<Phase>,
    pub phase_at_ms: u64,
    pub ready_at_ms: Option<u64>,
    pub interrupted: bool,
    pub replay: Vec<String>,
    pub awaiting_continue: bool,
    pub continue_after_replay: bool,
    pub replay_submitted: Option<String>,
    pub captured: bool,
    pub bind_from_start: bool,
    pub failure_started_ms: Option<u64>,
    pub reported_block: bool,
    /// Transport may reconnect, but delivery waits for an owner submission.
    pub paused: bool,
    pub interrupt_pending: bool,
    pub draft_files: Vec<String>,
    pub draft_submit_requested: bool,
    pub draft_restored: bool,
    pub draft_submitted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    Closing,
    Starting,
    Replaying,
}

impl ClientRecovery {
    /// PTY timers and replay writes belong to the dead client. Keep only owned
    /// input, pause intent and transcript receipt boundaries on a cold start.
    pub(super) fn cold(saved: Option<Self>, paused: bool) -> Self {
        let mut state = saved.unwrap_or_default();
        state.paused |= paused;
        state.phase = None;
        state.phase_at_ms = 0;
        state.ready_at_ms = None;
        state.replay.clear();
        state.replay_submitted = None;
        state.draft_restored = false;
        state.draft_submitted = false;
        state.reported_block = false;
        state
    }

    fn size(&self) -> usize {
        self.draft.as_ref().map_or(0, String::len)
            + self.pending.iter().map(String::len).sum::<usize>()
            + self.draft_files.iter().map(String::len).sum::<usize>()
    }

    pub(super) fn can_queue(&self, text: &str) -> bool {
        self.size().saturating_add(text.len()) <= INPUT_LIMIT
            && self.pending.len() + self.draft_files.len() < MESSAGE_LIMIT
    }

    /// Original paste contents survive Codex's collapsed-paste rendering. Other
    /// editing keys invalidate the captured draft; a visible snapshot can restore
    /// it, but an incomplete hidden paste can never authorize killing the client.
    pub(super) fn typed(&mut self, data: &str, shown: Option<String>) {
        if data.is_empty() {
            return;
        }
        if self.size().saturating_add(data.len()) > INPUT_LIMIT || self.pending.len() >= MESSAGE_LIMIT {
            self.blocked =
                Some("pending input exceeds the automatic recovery limit; the original client was preserved".into());
            self.draft = None;
            return;
        }
        if let Some(prefix) =
            data.strip_suffix('\r').filter(|prefix| !prefix.is_empty() && !prefix.chars().any(char::is_control))
        {
            self.typed(prefix, shown);
            self.typed("\r", None);
            return;
        }
        let base = self.draft.clone().or(shown);
        if data == "\r" {
            if let Some(draft) = base.filter(|d| !d.trim().is_empty() && !codex_slash_command(d)) {
                if self.phase == Some(Phase::Replaying) {
                    self.replay.push(draft.clone());
                    self.continue_after_replay = false;
                }
                self.pending.push(draft);
            }
            self.draft = Some(String::new());
        } else if data == "\x03" {
            self.draft = Some(String::new());
        } else if let Some(paste) = data.strip_prefix("\x1b[200~").and_then(|p| p.strip_suffix("\x1b[201~")) {
            self.draft = base.map(|mut draft| {
                draft.push_str(paste);
                draft
            });
        } else if !data.is_empty() && data.chars().all(|c| !c.is_control()) {
            self.draft = base.map(|mut draft| {
                draft.push_str(data);
                draft
            });
        } else {
            self.draft = None;
        }
        if self.size() > INPUT_LIMIT || self.pending.len() > MESSAGE_LIMIT {
            self.blocked =
                Some("pending input exceeds the automatic recovery limit; the original client was preserved".into());
        }
    }

    pub(super) fn reconcile(&mut self) -> Result<()> {
        let Some(path) = &self.rollout else { return Ok(()) };
        let mut file = fs::File::open(path)?;
        let length = file.metadata()?.len();
        anyhow::ensure!(length >= self.offset, "conversation log changed; input needs review");
        if length == self.offset {
            return Ok(());
        }
        let mut head = Vec::new();
        std::io::BufReader::new((&mut file).take(8 * 1024 * 1024)).read_until(b'\n', &mut head)?;
        anyhow::ensure!(
            head.last() == Some(&b'\n'),
            "conversation metadata is incomplete or too large; input was preserved"
        );
        let items = crate::remote::chat::codex_mode(&head) == crate::remote::chat::CodexMode::Items;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = std::io::BufReader::new(file.take(8 * 1024 * 1024));
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line)? > 0 {
            if line.last() != Some(&b'\n') {
                break;
            }
            self.offset += line.len() as u64;
            if let Some(receipt) = serde_json::from_slice(&line).ok().and_then(|r| logged_user(&r, items)) {
                // Upload paths are unique and captured before entering the
                // native composer. Its accepted image record acknowledges the
                // complete native caption, including edits we cannot replay.
                // Never require stale captured text to unlock a healthy client.
                let accepted_files = |paths: &[&str]| {
                    let mut remaining = paths.iter().copied();
                    !self.draft_files.is_empty()
                        && self.draft_files.iter().all(|file| remaining.any(|path| path == file))
                };
                let matches_draft = self.draft_submit_requested
                    && match &receipt {
                        UserReceipt::Attached { paths, text } => {
                            !text.is_empty() && accepted_files(&paths.iter().map(String::as_str).collect::<Vec<_>>())
                        }
                        UserReceipt::Images(Some(paths)) => accepted_files(&paths.split('\n').collect::<Vec<_>>()),
                        _ => false,
                    };
                if matches_draft {
                    self.draft_files.clear();
                    self.draft = Some(String::new());
                    self.draft_submit_requested = false;
                    self.draft_restored = false;
                    self.draft_submitted = false;
                    if !self.paused
                        && self.phase == Some(Phase::Replaying)
                        && self.pending.is_empty()
                        && self.replay.is_empty()
                    {
                        self.phase = None;
                        self.ready_at_ms = None;
                        self.failure_started_ms = None;
                        self.interrupted = false;
                        self.continue_after_replay = false;
                        self.awaiting_continue = false;
                    }
                    line.clear();
                    continue;
                }
                let (text, images) = match receipt {
                    UserReceipt::Text(text) => (Some(text), false),
                    UserReceipt::Images(text) => (text, true),
                    UserReceipt::Attached { .. } => (None, true),
                };
                let at = text.as_deref().and_then(|text| {
                    self.pending.iter().position(|message| {
                        if images {
                            message == text
                        } else {
                            normalized(message) == normalized(text)
                        }
                    })
                });
                if let Some(at) = at {
                    self.pending.remove(at);
                } else if images && !self.pending.is_empty() {
                    let reason = "an image receipt cannot be matched exactly; the client and input are preserved";
                    self.blocked = Some(reason.into());
                    anyhow::bail!(reason);
                }
            }
            line.clear();
        }
        anyhow::ensure!(self.offset == length, "conversation log is still changing; input was preserved");
        Ok(())
    }
}

impl Service {
    /// Stop is control, even when the input buffer is full. Keep every owned
    /// payload, including a paste whose delayed Enter has not fired yet.
    pub(super) fn pause_recovery_input(&mut self, id: u64, data: &str) -> bool {
        if !matches!(data, "\x1b" | "\x03") {
            return false;
        }
        let continuing = self.pending_continues.iter().any(|p| p.id == id);
        let retrying = self.sessions.get(&id).is_some_and(|s| {
            s.retry.seen.is_some()
                && s.hosted.as_ref().is_some_and(|h| {
                    let guard = h.lock();
                    status::stopped_on_error(
                        &screen::screen_text(&guard.term),
                        screen::text_cursor(&guard.term),
                        provider_automation::patterns(s.tool, &self.opts.config),
                    )
                    .is_some()
                })
        });
        let Some(s) = self.sessions.get_mut(&id).filter(|s| {
            s.tool == Tool::Codex
                && (s.recovery.phase.is_some()
                    || s.recovery.paused
                    || s.recovery.awaiting_continue
                    || s.recovery.replay_submitted.is_some()
                    || !s.recovery.draft_files.is_empty()
                    || retrying
                    || continuing)
        }) else {
            return false;
        };
        s.recovery.paused = true;
        s.recovery.interrupt_pending = true;
        s.recovery.interrupted = false;
        s.recovery.continue_after_replay = false;
        s.recovery.awaiting_continue = false;
        if s.recovery.phase.is_none() {
            s.recovery.phase = Some(Phase::Replaying);
        }
        s.retry.seen = None;
        s.tracker.goal_active = false;
        s.tracker.background = false;
        // Stop revokes already admitted automatic work too. Preserve its
        // input custody while advancing the irreversible control revision.
        s.input_changed();
        self.pending_submits.retain(|p| p.id != id);
        self.pending_continues.retain(|p| p.id != id);
        self.pending_keys.retain(|(_, pending_id, _)| *pending_id != id);
        self.pending_reviews.retain(|p| p.id != id);
        Self::interrupt_paused_client(s);
        true
    }

    fn interrupt_paused_client(s: &mut Sess) {
        if !s.recovery.paused || !s.recovery.interrupt_pending || s.recovery.phase == Some(Phase::Closing) {
            return;
        }
        let Some(h) = &s.hosted else { return };
        let guard = h.lock();
        let lines = screen::screen_text(&guard.term);
        let ready = !codex_reconnect_failed(&lines)
            && guard.term.mode().contains(TermMode::BRACKETED_PASTE)
            && crate::providers::recovery::codex_draft_nonempty(&lines, screen::text_cursor(&guard.term)).is_some();
        let working = crate::providers::screen_signals(s.tool, &lines, s.hosted.as_ref().and_then(|h| h.pid)).working;
        drop(guard);
        if ready && working {
            // Esc interrupts a native turn without Ctrl+C clearing its draft
            // or quitting an empty client. Retry only after a client is ready.
            h.write(vec![0x1b]);
            s.recovery.interrupt_pending = false;
        }
    }

    pub(super) fn resume_recovery_input(&mut self, id: u64) {
        let Some(s) = self.sessions.get_mut(&id).filter(|s| s.recovery.paused) else { return };
        s.recovery.paused = false;
        s.recovery.blocked = None;
        s.recovery.reported_block = false;
        s.recovery.interrupt_pending = false;
        s.recovery.phase_at_ms = util::now_ms();
        s.tracker.screen_prompt = None;
        if s.recovery.phase.is_none() {
            s.recovery.phase = Some(Phase::Starting);
        }
        // An already pasted message stays in the native composer. Resumption
        // schedules its Enter, never another paste of that same payload.
        if s.recovery.phase == Some(Phase::Replaying) && s.recovery.replay_submitted.is_some() {
            self.pending_submits.push(PendingSubmit::new(Instant::now(), id, 0, s.inputs));
        }
        self.checkpoint_recovery(true);
    }

    /// Fresh Codex threads are learned after their first prompt. Attach their
    /// receipt stream from its start, including input already accepted before
    /// discovery, rather than advancing directly past that first user item.
    pub(super) fn bind_client_rollouts(&mut self) {
        let now = Instant::now();
        for s in self
            .sessions
            .values_mut()
            .filter(|s| s.alive && s.tool == Tool::Codex && s.recovery.captured && s.recovery.rollout.is_none())
        {
            let Some(conversation) = s.conversation.clone() else { continue };
            if !s.recovery.bind_from_start && s.recovery.blocked.is_some() {
                // Finding the log could only block it again.
                continue;
            }
            // The first look covers the whole history; a log not written yet
            // is looked for again after a pause, in the newest days only.
            let first = s.chat.rollout_probe.as_ref().is_none_or(|(seen, _)| *seen != conversation);
            if !first && s.chat.rollout_probe.as_ref().is_some_and(|(_, at)| now.duration_since(*at) < ROLLOUT_PROBE) {
                continue;
            }
            s.chat.rollout_probe = Some((conversation.clone(), now));
            let codex_dir = s.account_root.as_deref().unwrap_or(&self.opts.codex_dir);
            let found = if first {
                codex::find_rollout(codex_dir, &conversation)
            } else {
                codex::find_recent_rollout(codex_dir, &conversation, 2)
            };
            let Some(path) = found else { continue };
            if !s.recovery.bind_from_start {
                // An existing log missing at first input has no trustworthy byte
                // boundary; retaining the client avoids replaying an old send.
                s.recovery.blocked =
                    Some("the input receipt boundary is unavailable; the original client is preserved".into());
                continue;
            }
            s.recovery.rollout = Some(path);
            s.recovery.offset = 0;
            let _ = s.recovery.reconcile();
        }
    }

    /// Capture before writing, at the same boundary for phone, desktop and
    /// broadcast input. Read-only rollout reconciliation never invents messages.
    pub(super) fn capture_client_input(&mut self, id: u64, data: &str) {
        let Some(s) = self.sessions.get_mut(&id).filter(|s| s.tool == Tool::Codex && s.alive) else { return };
        if !s.recovery.captured {
            s.recovery.captured = true;
            s.recovery.bind_from_start = s.conversation.is_none();
            if let Some(path) = s
                .conversation
                .as_deref()
                .and_then(|c| codex::find_rollout(s.account_root.as_deref().unwrap_or(&self.opts.codex_dir), c))
            {
                s.recovery.offset = fs::metadata(&path).map_or(0, |m| m.len());
                s.recovery.rollout = Some(path);
            }
        }
        let submitting = super::submits(data) || data.starts_with("\x1b[200~");
        // Remove acknowledged input before it grows or meets the bounded
        // custody limit. An ordinary draft key does neither, and reading the
        // conversation log for it would delay the echo of every key.
        if submitting || !s.recovery.can_queue(data) {
            let _ = s.recovery.reconcile();
        }
        // The screen is read only when no captured draft exists to extend.
        let shown = if s.recovery.draft.is_none() {
            s.hosted.as_ref().and_then(|h| {
                let guard = h.lock();
                codex_recovery_draft(&guard.term)
            })
        } else {
            None
        };
        s.recovery.typed(data, shown);
        self.checkpoint_recovery(submitting);
    }

    pub(super) fn recovering_exit(&mut self, id: u64) -> bool {
        if self.shutdown_at.is_some() {
            return false;
        }
        let Some(s) =
            self.sessions.get_mut(&id).filter(|s| s.recovery.phase == Some(Phase::Closing) && !s.remove_when_ended)
        else {
            return false;
        };
        s.alive = false;
        s.closing = None;
        if let Some(mut old) = s.hosted.take() {
            let _ = old.pause();
        }
        let (tool, conversation) = (s.tool, s.conversation.clone());
        let place = Placement {
            cwd: s.cwd.clone(),
            command: s.command.clone(),
            account: s.account.clone(),
            account_root: s.account_root.clone(),
            ..Placement::default()
        };
        let now_ms = util::now_ms();
        let result = self.spawn_hosted(id, tool, conversation.as_deref(), &StartModel::default(), &place, now_ms);
        match result {
            Ok((hosted, launch)) => {
                let tracker = self.new_tracker(tool, launch.attached, now_ms);
                let s = self.sessions.get_mut(&id).unwrap();
                s.hosted = Some(hosted);
                s.retry.input_revision = Some(0);
                s.alive = true;
                s.exit_code = None;
                s.spawned_ms = now_ms;
                s.tracker = tracker;
                s.facts.busy = None;
                s.facts.goal_active = None;
                s.chat = ChatWatch::default();
                s.recovery.phase = Some(Phase::Starting);
                s.recovery.phase_at_ms = now_ms;
                s.recovery.ready_at_ms = None;
                s.recovery.replay_submitted = None;
                s.recovery.draft_restored = false;
                s.recovery.draft_submitted = false;
                if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == id) {
                    entry.ended_at = None;
                }
                self.registry_dirty = true;
                self.save_registry();
                self.update_refs();
            }
            Err(error) => {
                let reason = format!(
                    "{} could not relaunch its failed client: {error:#}; pending input is preserved",
                    self.label_of(id)
                );
                let s = self.sessions.get_mut(&id).unwrap();
                s.recovery.phase = None;
                s.recovery.blocked = Some(reason.clone());
                self.announce_error(reason);
            }
        }
        true
    }

    pub(super) fn recover_clients(&mut self, now_ms: u64) {
        if !self.opts.config.recovery.recover_clients && !self.sessions.values().any(|s| s.recovery.phase.is_some()) {
            return;
        }
        if self.shutdown_at.is_some() {
            return;
        }
        let mut restart = Vec::new();
        let mut restore = Vec::new();
        let mut errors = Vec::new();
        let submitting: HashSet<_> = self.pending_submits.iter().map(|p| p.id).collect();
        for s in self.sessions.values_mut().filter(|s| s.tool == Tool::Codex && s.alive && !s.remove_when_ended) {
            if !self.opts.config.recovery.recover_clients && s.recovery.phase.is_none() {
                continue;
            }
            Self::interrupt_paused_client(s);
            if s.recovery.phase == Some(Phase::Closing) {
                s.tracker.status = Status::Starting;
                continue;
            }
            let was_replaying = s.recovery.phase == Some(Phase::Replaying);
            let replay_failed = was_replaying
                && s.hosted.as_ref().is_some_and(|h| codex_reconnect_failed(&screen::screen_text(&h.lock().term)));
            if was_replaying && !replay_failed {
                if s.recovery.paused {
                    continue;
                }
                if now_ms.saturating_sub(s.recovery.phase_at_ms)
                    > self.opts.config.recovery.ready_timeout_secs.clamp(1, 3600) * 1000
                {
                    s.recovery.phase = None;
                    s.recovery.blocked =
                        Some("the relaunched client did not finish restoring input; pending input is preserved".into());
                    errors.push((s.id, s.recovery.blocked.clone().unwrap()));
                    continue;
                }
                restore.push(s.id);
                continue;
            }
            if s.recovery.paused && s.recovery.phase == Some(Phase::Starting) {
                continue;
            }
            let Some(h) = &s.hosted else { continue };
            let (lines, cursor, bracketed) = {
                let guard = h.lock();
                (
                    screen::screen_text(&guard.term),
                    screen::text_cursor(&guard.term),
                    guard.term.mode().contains(TermMode::BRACKETED_PASTE),
                )
            };
            let failed = codex_reconnect_failed(&lines);
            if !failed {
                if s.recovery.phase != Some(Phase::Starting) {
                    continue;
                }
                if crate::providers::screen_signals(s.tool, &lines, s.hosted.as_ref().and_then(|h| h.pid)).working {
                    // A resumed turn is running: the client is healthy and its
                    // composer is restored once the turn ends.
                    s.recovery.phase_at_ms = now_ms;
                    s.recovery.ready_at_ms = None;
                    continue;
                }
                if now_ms.saturating_sub(s.recovery.phase_at_ms)
                    > self.opts.config.recovery.ready_timeout_secs.clamp(1, 3600) * 1000
                {
                    s.recovery.blocked =
                        Some("the relaunched client did not restore its composer; pending input is preserved".into());
                    s.recovery.phase = None;
                    errors.push((s.id, s.recovery.blocked.clone().unwrap()));
                    continue;
                }
                let ready =
                    bracketed && codex_draft(&lines, cursor).is_some_and(|d| d.is_empty()) && !s.asking_now(now_ms);
                if !ready {
                    s.recovery.ready_at_ms = None;
                    continue;
                }
                let since = *s.recovery.ready_at_ms.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) >= self.opts.config.recovery.client_settle_ms.min(60000) {
                    restore.push(s.id);
                }
                continue;
            }
            // A dead transport wins over a stale active-goal or turn-busy log.
            s.tracker.background = false;
            s.tracker.goal_active = false;
            s.tracker.tool_busy = Some(false);
            s.tracker.screen_working_until_ms = 0;
            s.tracker.status = Status::NeedsYou;
            s.tracker.screen_prompt = Some("Codex connection failed; recovering".into());
            let first_failed = *s.recovery.failure_started_ms.get_or_insert(now_ms);
            if let Some(reason) = s.recovery.blocked.clone() {
                s.recovery.phase = None;
                if !s.recovery.reported_block {
                    errors.push((s.id, reason));
                }
                continue;
            }
            s.recovery.attempts_ms.retain(|at| {
                now_ms.saturating_sub(*at) < self.opts.config.recovery.client_attempt_window_secs.clamp(1, 86400) * 1000
            });
            if s.recovery.attempts_ms.len() >= self.opts.config.recovery.client_attempts.clamp(1, 100) {
                s.recovery.phase = None;
                s.recovery.blocked = Some(
                    "Codex reconnection failed repeatedly; automatic relaunch stopped and input is preserved".into(),
                );
                errors.push((s.id, s.recovery.blocked.clone().unwrap()));
                continue;
            }
            let wait = 1000u64 << s.recovery.attempts_ms.len().min(5);
            if s.recovery.attempts_ms.last().is_some_and(|at| now_ms < at.saturating_add(wait)) {
                continue;
            }
            let preserving_original = s.recovery.phase.is_some();
            let shown = if preserving_original || !s.recovery.draft_files.is_empty() {
                s.recovery.draft.clone()
            } else {
                s.hosted.as_ref().and_then(|h| codex_recovery_draft(&h.lock().term))
            };
            if let Some(shown) = shown {
                if s.recovery.draft.as_ref().is_none_or(|draft| normalized(draft) != normalized(&shown)) {
                    s.recovery.draft = Some(shown);
                }
            } else if s.recovery.draft.as_deref() == Some("") {
                s.recovery.draft = None;
            }
            if s.recovery.draft.is_none() {
                s.recovery.phase = None;
                s.recovery.blocked = Some(INCOMPLETE_DRAFT.into());
                errors.push((s.id, s.recovery.blocked.clone().unwrap()));
                continue;
            }
            if (s.recovery.draft_files.is_empty()
                && !codex_image_draft_recoverable(&lines, cursor, s.recovery.draft.as_deref().unwrap()))
                || s.recovery.draft_files.iter().any(|path| !Path::new(path).is_file())
            {
                s.recovery.phase = None;
                s.recovery.blocked = Some(IMAGE_DRAFT.into());
                errors.push((s.id, s.recovery.blocked.clone().unwrap()));
                continue;
            }
            if submitting.contains(&s.id) && !preserving_original {
                s.recovery.typed("\r", None);
            }
            if s.recovery.pending.is_empty() && lines.iter().any(|l| l.trim() == "Queued follow-up inputs") {
                s.recovery.phase = None;
                s.recovery.blocked = Some(
                    "the failed client has older queued input whose original text is unavailable; it was preserved"
                        .into(),
                );
                errors.push((s.id, s.recovery.blocked.clone().unwrap()));
                continue;
            }
            let receipts = if (s.conversation.is_none() || (s.recovery.captured && s.recovery.rollout.is_none()))
                && s.recovery.pending.iter().any(|text| !codex_slash_command(text))
            {
                Err(anyhow::anyhow!("waiting for the conversation identity"))
            } else {
                s.recovery.reconcile()
            };
            if let Err(error) = receipts {
                if now_ms.saturating_sub(first_failed) < 10_000 {
                    continue;
                }
                s.recovery.phase = None;
                s.recovery.blocked =
                    Some(format!("could not verify pending messages: {error:#}; the original client was preserved"));
                errors.push((s.id, s.recovery.blocked.clone().unwrap()));
                continue;
            }
            s.recovery.interrupted |=
                !s.recovery.paused && (s.facts.busy == Some(true) || s.facts.goal_active == Some(true));
            s.recovery.attempts_ms.push(now_ms);
            s.recovery.phase = Some(Phase::Closing);
            s.recovery.phase_at_ms = now_ms;
            s.recovery.ready_at_ms = None;
            s.closing = Some((Instant::now(), 1));
            restart.push(s.id);
        }
        for id in restart {
            self.pending_submits.retain(|p| p.id != id);
            self.pending_keys.retain(|(_, pending_id, _)| *pending_id != id);
            self.pending_reviews.retain(|p| p.id != id);
            if let Some(h) = self.sessions.get(&id).and_then(|s| s.hosted.as_ref()) {
                h.signal(libc::SIGHUP);
            }
            self.broadcast(&ServerMsg::Notice {
                text: format!("Reconnecting {} automatically; its input is preserved", self.label_of(id)),
                error: false,
            });
        }
        for id in restore {
            self.restore_recovered_input(id);
        }
        for (id, reason) in errors {
            if let Some(s) = self.sessions.get_mut(&id) {
                s.recovery.reported_block = true;
            }
            self.announce_error(format!("{}: {reason}", self.label_of(id)));
        }
    }

    fn restore_recovered_input(&mut self, id: u64) {
        if !self.checkpoint_recovery(true) {
            return;
        }
        if self.pending_submits.iter().any(|p| p.id == id) || self.pending_continues.iter().any(|p| p.id == id) {
            return;
        }
        let Some(s) = self.sessions.get_mut(&id) else { return };
        if s.recovery.paused {
            return;
        }
        if !s.recovery.draft_restored && s.recovery.draft_files.iter().any(|path| !Path::new(path).is_file()) {
            let reason = "A saved attachment is unavailable; recovery input is held for review";
            s.recovery.phase = None;
            s.recovery.blocked = Some(reason.into());
            self.announce_error(format!("{}: {reason}", self.label_of(id)));
            return;
        }
        if let Err(error) = s.recovery.reconcile() {
            let reason = format!("{}: could not restore pending messages: {error:#}", session_label(s.tool, &s.title));
            s.recovery.blocked = Some(reason.clone());
            s.recovery.phase = None;
            self.announce_error(reason);
            return;
        }
        if s.recovery.phase == Some(Phase::Starting) {
            s.recovery.replay = s.recovery.pending.clone();
            s.recovery.phase = Some(Phase::Replaying);
            s.recovery.continue_after_replay = s.recovery.interrupted
                && !s.recovery.draft_submit_requested
                && s.recovery.pending.iter().all(|p| codex_slash_command(p))
                && !s.recovery.pending.iter().any(|p| matches!(p.trim(), "/clear" | "/new"));
        }
        s.tracker.screen_prompt = None;
        if !s.recovery.draft_files.is_empty() && s.recovery.draft_restored {
            if s.recovery.draft_submit_requested && !s.recovery.draft_submitted {
                self.pending_submits.push(PendingSubmit::new(Instant::now(), id, 0, s.inputs));
            } else if !s.recovery.draft_submit_requested {
                s.recovery.phase = None;
            }
            return;
        }
        if let Some(message) = s.recovery.replay.first().cloned() {
            let Some(h) = &s.hosted else { return };
            let guard = h.lock();
            if !codex_draft(&screen::screen_text(&guard.term), screen::text_cursor(&guard.term))
                .is_some_and(|d| d.is_empty())
            {
                return;
            }
            drop(guard);
            s.recovery.replay.remove(0);
            s.recovery.replay_submitted = Some(message.clone());
            s.typed_input();
            let inputs = s.inputs;
            if !self.checkpoint_recovery(true) {
                let s = self.sessions.get_mut(&id).unwrap();
                s.recovery.replay.insert(0, message);
                s.recovery.replay_submitted = None;
                return;
            }
            if let Some(h) = self.sessions.get(&id).and_then(|s| s.hosted.as_ref()) {
                h.write(crate::keys::encode_message(&message).into_bytes());
            }
            self.pending_submits.push(PendingSubmit::new(Instant::now(), id, 0, inputs));
            return;
        }
        let draft = s.recovery.draft.clone().unwrap_or_default();
        if let Some(h) = &s.hosted {
            let guard = h.lock();
            if !codex_draft(&screen::screen_text(&guard.term), screen::text_cursor(&guard.term))
                .is_some_and(|d| d.is_empty())
            {
                return;
            }
        }
        if s.recovery.continue_after_replay
            && self.opts.config.recovery.resume(s.tool.key(), id)
            && !self.opts.config.recovery.message(s.tool.key(), id, &self.opts.config.resume_message).trim().is_empty()
        {
            s.recovery.continue_after_replay = false;
            s.recovery.awaiting_continue = true;
            s.tracker.tool_busy = Some(false);
            s.tracker.goal_active = false;
            s.tracker.background = false;
            s.tracker.status = Status::Idle;
            self.pending_continues.push(resume::PendingContinue { id, since: Instant::now(), ready_since: None });
            return;
        }
        if !s.recovery.draft_files.is_empty() {
            if let Some(h) = &s.hosted {
                for file in &s.recovery.draft_files {
                    h.write(crate::keys::encode_message(file).into_bytes());
                }
                if !draft.is_empty() {
                    h.write(crate::keys::encode_message(&draft).into_bytes());
                }
                s.recovery.draft_restored = true;
                if s.recovery.draft_submit_requested {
                    self.pending_submits.push(PendingSubmit::new(Instant::now(), id, 0, s.inputs));
                } else {
                    s.recovery.phase = None;
                }
            }
            return;
        }
        s.recovery.phase = None;
        s.recovery.ready_at_ms = None;
        s.recovery.interrupted = false;
        s.recovery.failure_started_ms = None;
        s.recovery.awaiting_continue = false;
        s.tracker.screen_prompt = None;
        s.tracker.tool_busy = Some(false);
        s.tracker.goal_active = false;
        s.tracker.background = false;
        // Every queued paste received its delayed Enter. The editable draft
        // follows them without being submitted.
        if let Some(h) = &s.hosted {
            if !draft.is_empty() {
                h.write(crate::keys::encode_message(&draft).into_bytes());
            }
        }
        self.broadcast(&ServerMsg::Notice {
            text: format!("Reconnected {}; its conversation and input are restored", self.label_of(id)),
            error: false,
        });
    }
}
