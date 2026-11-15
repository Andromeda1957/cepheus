//! Cold-start restoration runs in the service, before any TUI attaches.
//! Signal shutdown preserves recovery intent; public Stop refuses live or recovering sessions.
//! Explicit Close ends a chosen session.
use super::{PendingSubmit, Service};
use crate::protocol::{session_label, ServerMsg, Status, Tool};
use alacritty_terminal::term::TermMode;
use std::time::{Duration, Instant};

pub struct PendingContinue {
    pub id: u64,
    pub since: Instant,
    pub ready_since: Option<Instant>,
}

pub struct PendingRestore {
    pub id: u64,
    pub due: Instant,
    pub attempts: usize,
}

impl Service {
    /// Save captured input even without a status transition. Enter/paste and
    /// owner stops are barriers; ordinary draft keystrokes use the configured
    /// checkpoint interval.
    pub(super) fn snapshot_recovery(&mut self) {
        for s in self.sessions.values() {
            let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == s.id) else { continue };
            let saved = (s.recovery != super::recovery::ClientRecovery::default()).then(|| s.recovery.clone());
            if entry.recovery != saved {
                entry.recovery = saved;
                self.registry_dirty = true;
            }
        }
    }

    /// `force` is a barrier: durable before returning, so the caller may
    /// hand the input to the agent. Otherwise a snapshot is handed to the
    /// registry writer once per checkpoint interval and the loop goes on.
    pub(super) fn checkpoint_recovery(&mut self, force: bool) -> bool {
        if force {
            return self.save_registry();
        }
        let interval = Duration::from_millis(self.opts.config.recovery.checkpoint_interval_ms.min(60_000));
        if self.recovery_snapshot_at.elapsed() >= interval {
            self.queue_registry_save();
        }
        true
    }

    pub(super) fn note_boot_input(&mut self, id: u64, data: &str, submission: bool) {
        let stop = matches!(data, "\x1b" | "\x03");
        if !stop && !submission {
            return;
        }
        let starts_work = submission
            && self.sessions.get(&id).is_some_and(|s| {
                let inline = data.strip_suffix('\r').unwrap_or(data);
                let draft = if !inline.is_empty() {
                    Some(inline.to_owned())
                } else {
                    s.recovery.draft.clone().or_else(|| {
                        s.hosted.as_ref().and_then(|h| {
                            let guard = h.lock();
                            crate::status::claude_composer(&crate::screen::screen_text(&guard.term))
                        })
                    })
                };
                draft.is_some_and(|d| !d.trim().is_empty() && !d.trim_start().starts_with('/'))
            });
        if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == id && e.tool.is_agent()) {
            entry.owner_paused = stop;
            entry.boot_pending = starts_work;
            if starts_work {
                entry.working = true;
            }
            if stop {
                entry.working = false;
            }
            self.registry_dirty = true;
        }
        if !submission {
            // A stop gates no delivery: save it now, without waiting on the disk.
            self.queue_registry_save();
        }
    }

    pub(super) fn note_for_resume(&mut self, owner: bool) {
        let resume = self.opts.config.resume_sessions && !owner;
        let mut changed = false;
        for s in self.sessions.values() {
            let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == s.id) else { continue };
            let was = (entry.resume, entry.working, entry.boot_pending);
            let again = resume && s.alive && entry.recoverable();
            entry.resume = again || (!owner && entry.resume);
            if owner || s.alive {
                entry.working = again
                    && !entry.owner_paused
                    && !s.recovery.paused
                    && (entry.boot_pending || s.tracker.status == Status::Working);
            }
            if owner {
                entry.boot_pending = false;
            }
            changed |= (entry.resume, entry.working, entry.boot_pending) != was;
        }
        // Write only what shutdown changed. A load-time fill-in, such as an
        // older list's start directory, waits for a real change, so an
        // accepted Stop that changes nothing leaves the saved list as it was.
        // The save still persists earlier unsaved changes and waits out
        // queued writes.
        self.registry_dirty |= changed;
        self.save_registry();
    }

    pub(super) fn track_working(&mut self) {
        if self.shutdown_at.is_some() {
            return;
        }
        let mut changed = false;
        for s in self.sessions.values() {
            let pending = self.pending_continues.iter().any(|p| p.id == s.id)
                || self.pending_submits.iter().any(|p| p.id == s.id)
                || s.recovery.phase.is_some()
                || s.tracker.status == Status::Starting;
            let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == s.id) else { continue };
            // The terminal is locked only for a session still waiting to be
            // seen working again after a boot.
            let fresh = || {
                s.hosted
                    .as_ref()
                    .is_some_and(|h| s.retry.input_revision.is_none_or(|rev| h.lock().output_revision > rev))
            };
            if entry.boot_pending
                && !pending
                && matches!(s.tracker.status, Status::Working | Status::Finished)
                && fresh()
            {
                entry.boot_pending = false;
                changed = true;
            }
            let working = s.alive
                && s.tool.is_agent()
                && !entry.owner_paused
                && !s.recovery.paused
                && (entry.boot_pending || s.tracker.status == Status::Working);
            if entry.working != working {
                entry.working = working;
                changed = true;
            }
        }
        if changed {
            self.registry_dirty = true;
            // Turn transitions must not wait for the ordinary draft interval;
            // the writer thread makes them durable without holding the loop.
            self.queue_registry_save();
        }
    }

    pub(super) fn resume_after_start(&mut self) {
        if !self.opts.config.resume_sessions {
            return;
        }
        let policy = &self.opts.config.recovery;
        let mut due = Instant::now() + Duration::from_secs(policy.startup_delay_secs.min(3600));
        let stagger = Duration::from_millis(policy.launch_interval_ms.min(60_000));
        for entry in &self.registry.sessions {
            if entry.resume
                && policy.restore(entry.tool.key(), entry.id)
                && self.sessions.get(&entry.id).is_some_and(|s| !s.alive)
            {
                self.pending_restores.push(PendingRestore { id: entry.id, due, attempts: 0 });
                due += stagger;
            }
        }
        self.restores_due(Instant::now());
    }

    pub(super) fn restores_due(&mut self, now: Instant) {
        if self.shutdown_at.is_some() {
            return;
        }
        let mut keep = Vec::new();
        for mut pending in std::mem::take(&mut self.pending_restores) {
            if now < pending.due {
                keep.push(pending);
                continue;
            }
            let id = pending.id;
            let Some(entry) = self.registry.sessions.iter().find(|e| e.id == id) else { continue };
            if !self.opts.config.resume_sessions || !self.opts.config.recovery.restore(entry.tool.key(), id) {
                continue;
            }
            if self.sessions.get(&id).is_some_and(|s| s.alive) {
                continue;
            }
            if entry.tool.is_agent() && !self.scheduler.settings.probe.read().admits() {
                // Short headroom is common while many agents restart together.
                // It holds the restore without spending a launch attempt.
                pending.due = now + Duration::from_secs(self.opts.config.recovery.retry_delay_secs.clamp(1, 3600));
                keep.push(pending);
                continue;
            }
            let working = (entry.working || entry.boot_pending) && !entry.owner_paused;
            let message =
                self.opts.config.recovery.message(entry.tool.key(), id, &self.opts.config.resume_message).to_owned();
            let resume =
                working && self.opts.config.recovery.resume(entry.tool.key(), id) && !message.trim().is_empty();
            let Some(s) = self.sessions.get(&id) else { continue };
            let owned_input = !s.recovery.pending.is_empty()
                || !s.recovery.draft_files.is_empty()
                || s.recovery.draft.as_ref().is_some_and(|d| !d.is_empty());
            // Interrupted Claude and Codex turns carry on through the
            // provider's own resume command, so the turn is running before
            // any client attaches. Codex messages accepted but never recorded
            // are the work itself and are replayed instead; held input waits.
            let native = resume
                && !s.recovery.paused
                && s.conversation.is_some()
                && match s.tool {
                    Tool::Claude => true,
                    Tool::Codex => {
                        s.recovery.pending.is_empty() && (self.opts.config.recovery.restore_drafts || !owned_input)
                    }
                    _ => false,
                };
            pending.attempts += 1;
            match self.reopen_with(id, native.then_some(message)) {
                Ok(()) => {
                    let s = self.sessions.get_mut(&id).unwrap();
                    if s.tool == Tool::Codex && (owned_input || (resume && !native) || s.recovery.paused) {
                        s.recovery.interrupted = resume && !native && !s.recovery.paused;
                        if self.opts.config.recovery.restore_drafts || !owned_input {
                            s.recovery.phase = Some(super::recovery::Phase::Starting);
                            s.recovery.phase_at_ms = crate::util::now_ms();
                            s.recovery.draft.get_or_insert_with(String::new);
                        } else {
                            s.recovery.phase = Some(super::recovery::Phase::Starting);
                            s.recovery.phase_at_ms = crate::util::now_ms();
                            s.recovery.paused = true;
                            s.recovery.blocked = Some(
                                "Saved input is held; recovery.restore_drafts is disabled; submit to release it".into(),
                            );
                        }
                    } else if resume && !native && !s.recovery.paused {
                        self.pending_continues.push(PendingContinue { id, since: now, ready_since: None });
                    }
                    if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == id) {
                        entry.resume = false;
                        entry.boot_pending = resume;
                        entry.working = resume;
                    }
                }
                Err(error) => {
                    let label = self.label_of(id);
                    if pending.attempts < self.opts.config.recovery.launch_attempts.clamp(1, 100) {
                        pending.due =
                            now + Duration::from_secs(self.opts.config.recovery.retry_delay_secs.clamp(1, 3600));
                        keep.push(pending);
                        self.log_error(&format!("Recovery of {label} will retry: {error:#}"));
                    } else {
                        self.announce_error(format!("Recovery of {label} could not launch: {error:#}. Its recovery request is saved for the next service start."));
                    }
                }
            }
            self.registry_dirty = true;
            self.save_registry();
        }
        self.pending_restores = keep;
    }

    /// A complete empty native composer and no question are required. A paste
    /// is only scheduled here; the normal guarded submit path owns its Enter.
    pub(super) fn continues_due(&mut self, now: Instant) {
        let mut keep = Vec::new();
        let mut gave_up = Vec::new();
        for mut p in std::mem::take(&mut self.pending_continues) {
            if self.messages.active(p.id) {
                p.ready_since = None;
                keep.push(p);
                continue;
            }
            let Some(s) = self.sessions.get_mut(&p.id).filter(|s| s.alive) else { continue };
            if s.recovery.paused || self.registry.sessions.iter().any(|e| e.id == p.id && e.owner_paused) {
                continue;
            }
            let message =
                self.opts.config.recovery.message(s.tool.key(), p.id, &self.opts.config.resume_message).to_owned();
            if !self.opts.config.recovery.resume(s.tool.key(), p.id) || message.trim().is_empty() {
                continue;
            }
            let label = session_label(s.tool, &s.title);
            if now.duration_since(p.since)
                > Duration::from_secs(self.opts.config.recovery.ready_timeout_secs.clamp(1, 3600))
            {
                gave_up.push((p.id, label));
                continue;
            }
            let ready = !s.asking_now(crate::util::now_ms())
                && matches!(s.tracker.status, Status::Idle | Status::Finished)
                && s.hosted.as_ref().is_some_and(|h| {
                    let guard = h.lock();
                    if matches!(s.tool, Tool::Claude | Tool::Codex) {
                        crate::status::empty_automatic_composer(s.tool, &guard.term)
                    } else {
                        guard.term.mode().contains(TermMode::BRACKETED_PASTE)
                    }
                });
            if !ready {
                p.ready_since = None;
                keep.push(p);
                continue;
            }
            let since = *p.ready_since.get_or_insert(now);
            if now.duration_since(since) < Duration::from_millis(self.opts.config.recovery.settle_ms.min(60_000)) {
                keep.push(p);
                continue;
            }
            let encoded = crate::keys::encode_message(&message);
            // Bind receipts without merging generated text into an owner's
            // saved draft or typed()'s replay queue.
            self.capture_client_input(p.id, "");
            let s = self.sessions.get_mut(&p.id).unwrap();
            if s.tool == Tool::Codex {
                if !s.recovery.can_queue(&message) {
                    s.recovery.blocked = Some("Continuation is waiting for input custody space".into());
                    continue;
                }
                if s.recovery.replay_submitted.as_deref() != Some(&message) {
                    s.recovery.pending.push(message.clone());
                }
                s.recovery.replay_submitted = Some(message);
            }
            s.typed_input();
            s.recovery.awaiting_continue = false;
            if !self.checkpoint_recovery(true) {
                keep.push(p);
                continue;
            }
            let s = self.sessions.get(&p.id).unwrap();
            if let Some(h) = &s.hosted {
                h.follow_output();
                h.write(encoded.into_bytes());
            }
            self.pending_submits.retain(|submit| submit.id != p.id);
            self.pending_submits.push(PendingSubmit::new(now, p.id, 0, s.inputs));
            self.broadcast(&ServerMsg::Notice {
                text: format!("Queued continuation for {label}; waiting for the agent to accept it"),
                error: false,
            });
        }
        self.pending_continues = keep;
        for (id, label) in gave_up {
            if let Some(s) = self.sessions.get_mut(&id) {
                s.recovery.blocked =
                    Some("Restored session is waiting for a usable composer; interrupted work remains pending".into());
            }
            self.announce_error(format!("{label} was restored but did not become ready for continuation; its interrupted work is still recorded"));
        }
        self.checkpoint_recovery(false);
    }
}
