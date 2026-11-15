use super::*;
impl Service {
    pub(crate) fn refresh_scan_list(&mut self, now: Instant) {
        if self.opts.config.plugin_enabled("security-scans")
            && self.native.scans_read.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(30))
        {
            self.native.scans_read = Some(now);
            self.native.scans = jobs::list(&self.opts.config.scans_dir()).iter().map(|j| j.info(None)).collect();
        }
    }

    /// Begin a Codex Security scan of the start directory (`jobs.rs`): its
    /// scratch worktree is made on a thread, then Codex starts in it.
    pub(crate) fn start_scan(&mut self, cid: u64, scope: Option<String>) {
        if self.shutdown_at.is_some() || !self.opts.config.plugin_enabled("security-scans") {
            self.notice(cid, "Plugin security-scans is disabled", true);
            return;
        }
        if self.native.scan_preparing > 0 || self.native.scan_jobs.iter().any(jobs::ScanJob::running) {
            self.notice(cid, "A security scan is already running here.", true);
            return;
        }
        let start_dir = self.opts.start_dir.clone();
        let scans_dir = self.opts.config.scans_dir();
        let tx = self.tx.clone();
        let cancel = self.extensions.cancel.clone();
        let started = thread::Builder::new().name("scan".into()).spawn(move || {
            crate::run::with_cancellation(cancel, || {
                let (scan, error) = match jobs::prepare(&start_dir, &scans_dir, scope.as_deref(), util::now_secs()) {
                    Ok(job) => (job, None),
                    Err(err) => (jobs::ScanJob::default(), Some(format!("{err:#}"))),
                };
                let _ = tx.send(Ev::ScanReady { client: cid, scan, error });
            })
        });
        match started {
            Ok(_) => {
                self.native.scan_preparing += 1;
                self.notice(cid, "Checking the repository out for the security scan…", false);
            }
            Err(err) => self.notice(cid, format!("Could not start the scan: {err}"), true),
        }
    }

    /// The scratch worktree is ready: start Codex in it.
    pub(crate) fn scan_ready(&mut self, cid: u64, mut job: jobs::ScanJob, error: Option<String>) {
        self.native.scan_preparing = self.native.scan_preparing.saturating_sub(1);
        if self.shutdown_at.is_some() && error.is_none() {
            self.end_scan(job, true);
            return;
        }
        if let Some(err) = error {
            self.notice(cid, format!("The security scan could not start: {err}"), true);
            return;
        }
        let place = Placement {
            cwd: Some(job.worktree()),
            name: Some(format!("Security scan {}", job.head.as_deref().unwrap_or(""))),
            job: Some(job.tag()),
            prompt: Some(jobs::prompt(&job)),
            args: jobs::codex_args(&job),
            ..Placement::default()
        };
        match self.create_with(Tool::Codex, None, &StartModel::default(), &place) {
            Ok(id) => {
                job.session = Some(id);
                let _ = job.save();
                self.native.scan_jobs.push(job);
                self.native.scans_read = None;
                self.send(cid, &ServerMsg::Created { id, request: None });
            }
            Err(err) => {
                self.notice(cid, format!("Could not start Codex for the scan: {err:#}"), true);
                self.end_scan(job, true);
            }
        }
    }

    /// Remove a scan's worktree on a thread and report how it ended.
    pub(crate) fn end_scan(&mut self, job: jobs::ScanJob, stopped: bool) {
        let tx = self.tx.clone();
        let work = crate::extensions::WorkGuard::new(self.extensions.workers.clone());
        let _ = thread::Builder::new().name("scan-end".into()).spawn(move || {
            let _work = work;
            let job = jobs::finish(job, stopped);
            let error = job.state.starts_with("failed");
            let text = match job.state.as_str() {
                "saved" => format!("Security scan saved: {}", job.report().display()),
                "stopped" => format!("Security scan stopped; its folder: {}", job.dir.display()),
                state => format!("Security scan {state}"),
            };
            let _ = tx.send(Ev::Notice { client: None, text, error });
        });
        self.native.scans_read = None;
    }

    /// Follow the running scans: once the report is written and Codex's turn
    /// is over, or its session ended, close it and remove the worktree.
    pub(crate) fn watch_scans(&mut self) {
        let mut ended = Vec::new();
        for job in &self.native.scan_jobs {
            let session = job.session.and_then(|id| self.sessions.get(&id));
            let alive = session.is_some_and(|s| s.alive);
            let idle = session.is_some_and(|s| matches!(s.tracker.status, Status::Finished | Status::Idle));
            if !alive || (idle && job.report_written()) {
                ended.push((job.id.clone(), !alive && !job.report_written()));
            }
        }
        for (id, stopped) in ended {
            let Some(pos) = self.native.scan_jobs.iter().position(|j| j.id == id) else { continue };
            let job = self.native.scan_jobs.remove(pos);
            if let Some(sid) = job.session {
                self.close_and_remove(sid);
            }
            self.end_scan(job, stopped);
        }
    }

    /// The scans a previous service left running: follow each whose session
    /// is still here, and end the rest.
    pub(crate) fn load_scan_jobs(&mut self) {
        if !self.opts.config.plugin_enabled("security-scans") {
            return;
        }
        if !self.opts.monitor {
            return;
        }
        for job in jobs::list(&self.opts.config.scans_dir()).into_iter().filter(jobs::ScanJob::running) {
            let tag = job.tag();
            let session = self.sessions.values().find(|s| s.job.as_deref() == Some(tag.as_str())).map(|s| s.id);
            match session {
                Some(id) => {
                    let mut job = job;
                    job.session = Some(id);
                    self.native.scan_jobs.push(job);
                }
                None => self.end_scan(job, true),
            }
        }
    }

    pub(crate) fn scan_infos(&self) -> Vec<ScanInfo> {
        let mut out: Vec<ScanInfo> = self.native.scan_jobs.iter().map(|j| j.info(None)).collect();
        let saved: Vec<ScanInfo> =
            self.native.scans.iter().filter(|s| !out.iter().any(|o| o.id == s.id)).cloned().collect();
        out.extend(saved);
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
        out.truncate(jobs::MAX_LISTED);
        out
    }
}
