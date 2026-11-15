//! Changing a selected agent's directory is session lifecycle, so it belongs
//! in core. Resolve paths on the owning host before touching the old process.
use super::*;

pub(super) struct PendingDirectory {
    client: u64,
    cwd: PathBuf,
}

fn resolve_directory(path: &str, base: &Path, home: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        !path.is_empty() && path.len() <= 4096 && !path.chars().any(char::is_control),
        "give a directory without control characters (at most 4096 bytes)"
    );
    let path = if path == "~" {
        home.to_path_buf()
    } else if let Some(tail) = path.strip_prefix("~/") {
        home.join(tail)
    } else {
        anyhow::ensure!(!path.starts_with('~'), "use ~ or ~/path for your home directory");
        base.join(path)
    };
    let path = fs::canonicalize(&path).with_context(|| format!("cannot open {}", path.display()))?;
    anyhow::ensure!(path.is_dir(), "{} is not a directory", path.display());
    fs::read_dir(&path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(path)
}

impl Service {
    pub(super) fn directory_reply(&self, client: u64, id: u64, cwd: Option<String>, error: Option<String>) {
        self.send(client, &ServerMsg::DirectoryChanged { id, cwd, error });
    }

    pub(super) fn change_directory(&mut self, client: u64, id: u64, path: &str) -> Result<()> {
        anyhow::ensure!(self.shutdown_at.is_none(), "the service is stopping");
        let s = self.sessions.get(&id).context("no such session")?;
        anyhow::ensure!(s.tool.is_agent(), "use cd inside the shell terminal");
        anyhow::ensure!(
            !self.opts.config.providers.iter().any(|p| p.id == s.tool.key() && p.cloud),
            "cloud agents keep their existing conversation and cannot change project directory"
        );
        anyhow::ensure!(s.closing.is_none(), "this session is already stopping");
        let base = s.cwd.as_deref().unwrap_or(&self.opts.start_dir);
        let cwd = resolve_directory(path, base, &paths::home())?;
        if s.alive && fs::canonicalize(base).ok().as_ref() == Some(&cwd) {
            self.directory_reply(client, id, Some(cwd.to_string_lossy().into_owned()), None);
            return Ok(());
        }
        let alive = s.alive;
        // Validate provider configuration before ending the current process.
        self.launch_for(s.tool, None, &Placement { cwd: Some(cwd.clone()), ..Placement::default() })?;
        self.pending_submits.retain(|p| p.id != id);
        self.pending_keys.retain(|(_, session, _)| *session != id);
        self.pending_reviews.retain(|p| p.id != id);
        self.pending_continues.retain(|p| p.id != id);
        if let Some(s) = self.sessions.get_mut(&id) {
            s.inputs += 1; // Invalidate answers captured from the previous process.
        }
        self.pending_directories.insert(id, PendingDirectory { client, cwd });
        if alive {
            // Events carry only the session id: the old Exited must be handled
            // before a replacement can safely occupy that same slot.
            self.close(id);
        } else {
            self.finish_directory_change(id);
        }
        Ok(())
    }

    pub(super) fn cancel_directory_change(&mut self, id: u64, reason: &str) {
        if let Some(pending) = self.pending_directories.remove(&id) {
            self.directory_reply(pending.client, id, None, Some(reason.into()));
        }
    }

    pub(super) fn finish_directory_change(&mut self, id: u64) {
        let Some(pending) = self.pending_directories.remove(&id) else { return };
        let result = self.start_in_directory(id, pending.cwd.clone(), None);
        match result {
            Ok(()) => self.directory_reply(pending.client, id, Some(pending.cwd.to_string_lossy().into_owned()), None),
            Err(err) => {
                // A folder can disappear between validation and spawn. Try the
                // previous location again; its conversation was never deleted.
                let recovery = self.sessions.get(&id).map(|s| {
                    let conversation = has_conversation(s, &self.opts).then(|| s.conversation.clone()).flatten();
                    (s.cwd.clone().unwrap_or_else(|| self.opts.start_dir.clone()), conversation)
                });
                let restored =
                    recovery.is_some_and(|(cwd, conversation)| self.start_in_directory(id, cwd, conversation).is_ok());
                let message = if restored {
                    format!("{err:#}; restored the previous directory")
                } else {
                    format!("{err:#}; previous conversations remain available through Resume")
                };
                self.directory_reply(pending.client, id, None, Some(message));
            }
        }
    }

    fn start_in_directory(&mut self, id: u64, cwd: PathBuf, conversation: Option<String>) -> Result<()> {
        anyhow::ensure!(cwd.is_dir(), "{} is no longer a directory", cwd.display());
        let tool = self.sessions.get(&id).context("the session was closed")?.tool;
        let place = Placement { cwd: Some(cwd.clone()), ..Placement::default() };
        let now_ms = util::now_ms();
        let (hosted, launch) =
            self.spawn_hosted(id, tool, conversation.as_deref(), &StartModel::default(), &place, now_ms)?;
        let tracker = self.new_tracker(tool, launch.attached, now_ms);
        if conversation.is_none() {
            self.archive_directory_conversation(id);
        }
        self.pending_submits.retain(|p| p.id != id);
        self.pending_keys.retain(|(_, session, _)| *session != id);
        self.pending_reviews.retain(|p| p.id != id);
        self.pending_continues.retain(|p| p.id != id);
        let s = self.sessions.get_mut(&id).unwrap();
        s.enter_conversation(launch.conversation.clone());
        if conversation.is_none() {
            s.forget_chat();
        }
        s.facts = SessionFacts::default();
        s.term_title = None;
        s.hosted = Some(hosted);
        s.alive = true;
        s.exit_code = None;
        s.closing = None;
        s.tracker = tracker;
        s.spawned_ms = now_ms;
        s.retry = AutoContinue::default();
        s.chat = ChatWatch::default();
        s.cwd = Some(cwd.clone());
        s.inputs += 1;
        if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == id) {
            entry.cwd = Some(cwd.to_string_lossy().into_owned());
            entry.conversation = s.conversation.clone();
            entry.name = s.name.clone();
            entry.title = s.title.clone();
            entry.ended_at = None;
            entry.working = false;
            entry.resume = false;
        }
        for c in self.clients.values_mut().filter(|c| c.attached == Some(id)) {
            c.sent_gen = 0;
            c.pending_frame = None;
        }
        self.registry_dirty = true;
        self.save_registry();
        self.update_refs();
        self.last_status = Instant::now() - STATUS_INTERVAL;
        Ok(())
    }

    fn archive_directory_conversation(&mut self, id: u64) {
        let Some(s) = self.sessions.get(&id) else { return };
        let Some(conversation) = s.conversation.as_ref().filter(|_| has_conversation(s, &self.opts)) else { return };
        if self
            .registry
            .sessions
            .iter()
            .any(|e| e.id != id && e.tool == s.tool && e.conversation.as_ref() == Some(conversation))
        {
            return;
        }
        let Some(mut entry) = self.registry.sessions.iter().find(|e| e.id == id).cloned() else { return };
        entry.id = self.registry.next_id;
        self.registry.next_id += 1;
        entry.conversation = Some(conversation.clone());
        entry.cwd = Some(s.cwd.as_deref().unwrap_or(&self.opts.start_dir).to_string_lossy().into_owned());
        entry.title = s.title.clone();
        entry.name = s.name.clone();
        entry.ended_at = Some(util::now_secs());
        entry.resume = false;
        entry.working = false;
        self.registry.sessions.push(entry);
    }
}
