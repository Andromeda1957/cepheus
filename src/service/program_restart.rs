//! Sessions running the deck's own program, such as a Dot bridge, may continue using a replaced executable.
//! Restart those sessions on the installed program in the same slot, with the same command and state directory,
//! once they are idle and have no queued or unsettled owner messages. Agent and terminal sessions are not
//! restarted.
use super::*;
use std::os::unix::fs::MetadataExt;

#[derive(Default)]
pub(super) struct State {
    checked: Option<Instant>,
    /// Sessions running a replaced program: since when, and their input
    /// count then. Input typed since starts the wait again.
    replaced: HashMap<u64, (Instant, u64)>,
    /// Sessions stopped to start again once their program has exited.
    stopping: HashSet<u64>,
    /// The process whose restart could not start, reported once.
    refused: HashMap<u64, u32>,
}

/// The file identity of `path`.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    fs::metadata(path).ok().map(|meta| (meta.dev(), meta.ino()))
}

/// Whether process `pid` runs an earlier file of the program now installed
/// at `installed`: one that was started from that path and replaced there.
fn runs_replaced(pid: u32, installed: &Path) -> bool {
    let exe = PathBuf::from(format!("/proc/{pid}/exe"));
    let Some(link) = fs::read_link(&exe).ok().and_then(|link| link.to_str().map(str::to_owned)) else {
        return false;
    };
    let ran = Path::new(link.strip_suffix(" (deleted)").unwrap_or(&link));
    let same_path = matches!((fs::canonicalize(ran), fs::canonicalize(installed)), (Ok(a), Ok(b)) if a == b);
    same_path && file_id(&exe).zip(file_id(installed)).is_some_and(|(running, now)| running != now)
}

impl Service {
    /// The registered provider behind `tool` when its command runs the
    /// deck's own program.
    fn deck_program_provider(&self, tool: Tool) -> Option<&crate::providers::Provider> {
        let Tool::Dynamic(key) = tool else { return None };
        let provider = self.opts.config.providers.iter().find(|p| p.id == key)?;
        let program = file_id(Path::new(provider.command.first()?))?;
        (Some(program) == file_id(&self.opts.exe)).then_some(provider)
    }

    /// Whether session `id` can stop now without losing anything: it is
    /// idle, asks nothing, and no owner message to it is queued, on its way
    /// or unsettled in its provider's delivery journal.
    fn quiet_for_restart(&self, s: &Sess, provider: &crate::providers::Provider, now_ms: u64) -> bool {
        let id = s.id;
        let pid = s.hosted.as_ref().and_then(|h| h.pid);
        s.alive
            && s.closing.is_none()
            && !s.remove_when_ended
            && !s.recovery.paused
            && s.recovery.phase.is_none()
            && s.recovery.pending.is_empty()
            && matches!(s.tracker.status, Status::Idle | Status::Finished)
            && !s.tracker.is_working(now_ms)
            && s.tracker.needs_reason().is_none()
            && !self.messages.active(id)
            && s.chat.controls.iter().all(|control| control.strong_count() == 0)
            && self.extensions.workers.load(Ordering::SeqCst) == 0
            && !self.pending_submits.iter().any(|p| p.id == id)
            && !self.pending_keys.iter().any(|(_, session, _)| *session == id)
            && !self.pending_reviews.iter().any(|p| p.id == id)
            && !self.pending_continues.iter().any(|p| p.id == id)
            && !self.pending_restores.iter().any(|p| p.id == id)
            && !self.pending_directories.contains_key(&id)
            && provider.chat_snapshot.as_deref().is_none_or(|transcript| {
                !crate::providers::snapshot::working(transcript, pid, now_ms)
                    && !crate::providers::snapshot::delivery_pending(transcript)
            })
    }

    /// Stop each quiet session whose deck program was replaced, to start it
    /// again on the installed one when it has exited (`program_exited`).
    pub(super) fn move_replaced_programs(&mut self, now: Instant) {
        if self.shutdown_at.is_some() || self.updating.is_some() {
            return;
        }
        let check = Duration::from_secs(self.opts.config.program_restart_check_secs);
        if self.program_moves.checked.is_some_and(|at| now.saturating_duration_since(at) < check) {
            return;
        }
        self.program_moves.checked = Some(now);
        let grace = Duration::from_secs(self.opts.config.program_restart_grace_secs);
        let now_ms = util::now_ms();
        let mut replaced = HashMap::new();
        let mut due = Vec::new();
        for s in self.sessions.values() {
            let Some(pid) = s.hosted.as_ref().and_then(|h| h.pid).filter(|_| s.alive) else { continue };
            let Some(provider) = self.deck_program_provider(s.tool) else { continue };
            if !runs_replaced(pid, &self.opts.exe) {
                continue;
            }
            let seen = self.program_moves.replaced.get(&s.id).copied().filter(|(_, inputs)| *inputs == s.inputs);
            let (since, inputs) = seen.unwrap_or((now, s.inputs));
            replaced.insert(s.id, (since, inputs));
            if now.saturating_duration_since(since) >= grace && self.quiet_for_restart(s, provider, now_ms) {
                due.push((s.id, pid));
            }
        }
        self.program_moves.replaced = replaced;
        for (id, pid) in due {
            self.stop_to_restart(id, pid);
        }
    }

    fn stop_to_restart(&mut self, id: u64, pid: u32) {
        let Some(s) = self.sessions.get(&id) else { return };
        // The same session, as Reopen starts it: its conversation, if it has
        // one, else a fresh start of its registered command.
        let recoverable = s.conversation.is_some()
            || self.registry.sessions.iter().any(|entry| entry.id == id && entry.recoverable());
        if !recoverable || !self.scheduler.settings.probe.read().admits() {
            return;
        }
        let place = Placement {
            account_root: s.account_root.clone(),
            account: s.account.clone(),
            cwd: s.cwd.clone(),
            command: s.command.clone(),
            ..Placement::default()
        };
        // The installed program must be able to start before the running one stops.
        if let Err(error) = self.launch_for(s.tool, s.conversation.as_deref(), &place) {
            let label = session_label(s.tool, &s.title);
            if self.program_moves.refused.insert(id, pid) != Some(pid) {
                self.log_error(&format!("{label} could not start again on the installed program: {error:#}"));
            }
            return;
        }
        self.program_moves.refused.remove(&id);
        self.program_moves.stopping.insert(id);
        self.close(id);
    }

    /// Session `id` has exited: if it was stopped to move, start it again on
    /// the installed program. A failure is that session's error.
    pub(super) fn program_exited(&mut self, id: u64) {
        if !self.program_moves.stopping.remove(&id) || self.shutdown_at.is_some() {
            return;
        }
        let Some(s) = self.sessions.get(&id).filter(|s| !s.alive && !s.remove_when_ended) else { return };
        let (label, owner_draft) = (session_label(s.tool, &s.title), s.owner_draft);
        match self.reopen_with(id, None) {
            // Starting again changes nothing the owner left unsent.
            Ok(()) => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.owner_draft = owner_draft;
                }
            }
            Err(error) => {
                self.log_error(&format!("{label} could not start again on the installed program: {error:#}"));
            }
        }
    }
}
