//! Durable headless execution. Only display metadata crosses the state wire.
use crate::{protocol::Tool, util};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
pub mod policy;
pub mod store;
pub mod stream;
pub mod supervisor;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub provider: Option<Tool>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub stdin: String,
    #[serde(default)]
    pub env_clear: bool,
    #[serde(default)]
    pub env_allowlist: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}
impl Spec {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.argv.is_empty()
                && self.argv.len() <= 128
                && self.argv.iter().all(|s| s.len() <= 16000 && !s.contains('\0'))
                && self.argv.iter().map(String::len).sum::<usize>() <= 192000,
            "invalid bounded job argument vector"
        );
        anyhow::ensure!(
            !self.argv[0].is_empty() && self.cwd.is_absolute(),
            "job requires an executable and absolute cwd"
        );
        anyhow::ensure!(self.provider.is_none_or(Tool::is_agent), "invalid job provider");
        anyhow::ensure!(
            self.stdin.len() <= 128000 && !self.stdin.contains('\0'),
            "job stdin exceeds finite input limit"
        );
        let name = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                && !s.as_bytes()[0].is_ascii_digit()
        };
        anyhow::ensure!(
            self.env.len() <= 128
                && self.env_allowlist.len() <= 128
                && self.env.keys().chain(&self.env_allowlist).all(|s| name(s))
                && self.env.values().all(|s| !s.contains('\0'))
                && self.env.values().map(String::len).sum::<usize>() <= 16000,
            "invalid job environment"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Info {
    pub id: u64,
    pub title: String,
    pub provider: Option<Tool>,
    pub status: String,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: String,
    #[serde(default)]
    pub host: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Intent {
    #[serde(default)]
    pub gate_binding: Option<String>,
    pub info: Info,
    pub key: String,
    pub nonce: String,
    pub spec: Spec,
    pub placement: String,
    #[serde(default)]
    pub repository: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Executor {
    pub host: Option<String>,
    pub cwd: PathBuf,
    pub repository: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub pid: u32,
    pub start: u64,
    pub boot: String,
    pub uid: u32,
}
impl Identity {
    pub fn read(pid: u32) -> Result<Self> {
        let stat = crate::procs::stat(pid).context("job process identity unavailable")?;
        Ok(Self {
            pid,
            start: stat.start_ticks,
            boot: fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().into(),
            uid: unsafe { libc::geteuid() },
        })
    }
    fn reap_if_child(&self) {
        let Some(stat) = crate::procs::stat(self.pid) else { return };
        if stat.ppid == std::process::id()
            && stat.state == 'Z'
            && stat.start_ticks == self.start
            && self.uid == unsafe { libc::geteuid() }
            && fs::read_to_string("/proc/sys/kernel/random/boot_id").is_ok_and(|b| b.trim() == self.boot)
        {
            // Reaping supervisor custody never supplies a workload status.
            let mut ignored = 0;
            unsafe {
                libc::waitpid(self.pid as i32, &mut ignored, libc::WNOHANG);
            }
        }
    }
    pub fn alive(&self) -> bool {
        self.pid > 1
            && self.uid == unsafe { libc::geteuid() }
            && fs::read_to_string("/proc/sys/kernel/random/boot_id").is_ok_and(|b| b.trim() == self.boot)
            && crate::procs::alive_with_start(self.pid, Some(self.start))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Launch {
    pub nonce: String,
    pub supervisor: Identity,
    pub child: Identity,
    pub pgid: u32,
    pub started_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Exit {
    pub nonce: String,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub finished_at: u64,
    pub stopped: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Record {
    pub intent: Intent,
    pub executor: Option<Executor>,
    pub info: Info,
    pub dismissed: bool,
    pub attempted_at: u64,
}
pub struct Jobs {
    #[cfg(test)]
    pub fail_policy_barrier: bool,
    pub root: PathBuf,
    pub records: BTreeMap<u64, Record>,
    pub last_poll: u64,
    children: Vec<std::process::Child>,
    retention_days: u64,
    persisted: BTreeMap<u64, Info>,
}
impl Jobs {
    #[cfg(test)]
    pub fn load(state: &Path) -> Result<Self> {
        Self::load_with_retention(state, 7)
    }
    pub fn load_with_retention(state: &Path, retention_days: u64) -> Result<Self> {
        let root = state.join("jobs");
        store::directory(&root)?;
        let mut jobs = Self {
            #[cfg(test)]
            fail_policy_barrier: false,
            root,
            records: BTreeMap::new(),
            last_poll: 0,
            children: Vec::new(),
            retention_days,
            persisted: BTreeMap::new(),
        };
        for entry in fs::read_dir(&jobs.root)? {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().and_then(|s| u64::from_str_radix(s, 16).ok()) else { continue };
            let dir = entry.path();
            store::check_directory(&dir)?;
            let intent: Intent = store::read(&dir.join("intent.json"))?;
            anyhow::ensure!(id == intent.info.id, "job identity changed");
            intent.spec.validate()?;
            let executor = store::optional(&dir.join("executor.json"))?;
            let saved: Option<Info> = store::optional(&dir.join("public.json"))?;
            if let Some(saved) = &saved {
                jobs.persisted.insert(id, saved.clone());
            }
            let info = saved.unwrap_or_else(|| intent.info.clone());
            jobs.records.insert(
                id,
                Record { intent, executor, info, dismissed: dir.join("dismissed").exists(), attempted_at: 0 },
            );
        }
        jobs.refresh()?;
        Ok(jobs)
    }
    pub fn dir(&self, id: u64) -> PathBuf {
        self.root.join(format!("{id:010x}"))
    }
    pub fn add(
        &mut self,
        key: String,
        title: String,
        spec: Spec,
        placement: String,
        repository: Option<String>,
    ) -> Result<Info> {
        self.add_gated(key, title, spec, placement, repository, None)
    }
    pub(crate) fn add_gated(
        &mut self,
        key: String,
        title: String,
        spec: Spec,
        placement: String,
        repository: Option<String>,
        gate_binding: Option<String>,
    ) -> Result<Info> {
        anyhow::ensure!(
            gate_binding.as_ref().is_none_or(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)),
            "invalid job gate binding"
        );
        spec.validate()?;
        anyhow::ensure!(
            !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control),
            "invalid job request key"
        );
        if let Some(record) = self.records.values().find(|r| r.intent.key == key) {
            anyhow::ensure!(
                record.intent.spec == spec
                    && record.intent.gate_binding == gate_binding
                    && record.intent.info.title == title
                    && record.intent.placement == placement
                    && record.intent.repository == repository,
                "job request key already owns a different specification"
            );
            return Ok(record.info.clone());
        }
        anyhow::ensure!(
            self.records.values().filter(|r| !r.dismissed).count() < 10000,
            "retained job capacity reached"
        );
        let id = Self::key_id(&key)?;
        anyhow::ensure!(!self.records.contains_key(&id), "job identifier collision; choose another request key");
        let info = Info {
            id,
            title,
            provider: spec.provider,
            status: "starting".into(),
            created_at: util::now_secs(),
            reason: "queued".into(),
            ..Info::default()
        };
        let intent =
            Intent { info: info.clone(), key, nonce: util::uuid_v4()?, spec, placement, repository, gate_binding };
        let dir = self.dir(id);
        store::publish_intent(&self.root, &dir, &intent)?;
        self.records
            .insert(id, Record { intent, executor: None, info: info.clone(), dismissed: false, attempted_at: 0 });
        if let Some(cancelled) = store::optional::<String>(&self.root.join("cancellations").join(format!("{id:010x}")))?
        {
            anyhow::ensure!(self.records[&id].intent.key == cancelled, "job cancellation key collision");
            return self.stop(id);
        }
        Ok(info)
    }
    fn key_id(key: &str) -> Result<u64> {
        anyhow::ensure!(
            !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control),
            "invalid job request key"
        );
        let digest = crate::remote::auth::sha256_hex(key.as_bytes());
        Ok(u64::from_str_radix(&digest[..10], 16)?.max(1))
    }
    pub fn cancel_key(&mut self, key: &str) -> Result<Option<Info>> {
        if let Some(id) = self.records.values().find(|r| r.intent.key == key).map(|r| r.info.id) {
            return self.stop(id).map(Some);
        }
        let id = Self::key_id(key)?;
        let dir = self.root.join("cancellations");
        store::directory(&dir)?;
        let path = dir.join(format!("{id:010x}"));
        if let Some(existing) = store::optional::<String>(&path)? {
            anyhow::ensure!(existing == key, "job cancellation key collision");
        } else {
            store::write(&path, &key)?;
        }
        Ok(None)
    }
    pub fn save_public(&mut self, id: u64) -> Result<()> {
        let info = &self.records[&id].info;
        if self.persisted.get(&id) == Some(info) {
            return Ok(());
        }
        // Acknowledged metadata is durable, but an unchanged poll is read-only.
        // Fleet100 exposed gigabytes of redundant journal writes and stale
        // placement snapshots when every RPC fsynced every retained row.
        store::write(&self.dir(id).join("public.json"), info)?;
        self.persisted.insert(id, info.clone());
        Ok(())
    }
    pub fn snapshot(&self) -> Vec<Info> {
        self.records.values().filter(|r| !r.dismissed).map(|r| r.info.clone()).collect()
    }
    /// Atomically cancel before the independent supervisor crosses its run gate.
    /// A locked gate is already owned even before owner.json becomes visible.
    pub(crate) fn cancel_unstarted(&mut self, key: &str) -> Result<(Option<Info>, bool)> {
        use std::os::fd::AsRawFd;
        let Some(id) = self.records.values().find(|r| r.intent.key == key).map(|r| r.info.id) else {
            return Ok((self.cancel_key(key)?, true));
        };
        let lock = store::append_log(&self.dir(id).join("run.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            anyhow::ensure!(error.kind() == std::io::ErrorKind::WouldBlock, "job run gate unavailable: {error}");
            return Ok((Some(self.records[&id].info.clone()), false));
        }
        if self.records[&id].executor.as_ref().is_some_and(|e| e.host.is_some()) || self.spawned(id)? {
            return Ok((Some(self.records[&id].info.clone()), false));
        }
        let info = self.stop(id)?;
        drop(lock);
        Ok((Some(info), true))
    }
    pub fn stop(&mut self, id: u64) -> Result<Info> {
        let record = self.records.get(&id).context("no such job")?;
        let stop = self.dir(id).join("stop.json");
        if let Some(nonce) = store::optional::<String>(&stop)? {
            anyhow::ensure!(nonce == record.intent.nonce, "job stop identity changed");
        } else {
            store::write(&stop, &record.intent.nonce)?;
        }
        if record.info.status == "starting"
            && record.executor.as_ref().is_none_or(|e| e.host.is_none())
            && !self.spawned(id)?
        {
            let exit = Exit {
                nonce: record.intent.nonce.clone(),
                code: None,
                signal: None,
                finished_at: util::now_secs(),
                stopped: true,
                error: Some("cancelled before dispatch".into()),
            };
            store::write(&self.dir(id).join("exit.json"), &exit)?;
        }
        self.refresh()?;
        Ok(self.records[&id].info.clone())
    }
    pub fn workload_pending(&self, id: u64) -> Result<bool> {
        let record = self.records.get(&id).context("no such job")?;
        if matches!(record.info.status.as_str(), "starting" | "working") {
            return Ok(true);
        }
        Ok(store::optional::<Launch>(&self.dir(id).join("launch.json"))?
            .is_some_and(|launch| launch.nonce == record.intent.nonce && launch.child.alive()))
    }
    pub fn remote_accepted(&self, id: u64) -> Result<bool> {
        let nonce: Option<String> = store::optional(&self.dir(id).join("accepted.json"))?;
        anyhow::ensure!(
            nonce.as_ref().is_none_or(|n| n == &self.records[&id].intent.nonce),
            "remote acceptance identity changed"
        );
        Ok(nonce.is_some())
    }
    pub fn mark_remote_accepted(&self, id: u64) -> Result<()> {
        if !self.remote_accepted(id)? {
            store::write(&self.dir(id).join("accepted.json"), &self.records[&id].intent.nonce)?;
        }
        Ok(())
    }
    pub fn stop_requested(&self, id: u64) -> Result<bool> {
        Ok(store::optional::<String>(&self.dir(id).join("stop.json"))?
            .is_some_and(|n| n == self.records[&id].intent.nonce))
    }
    pub fn stop_acked(&self, id: u64) -> Result<bool> {
        Ok(store::optional::<String>(&self.dir(id).join("stop-acked.json"))?
            .is_some_and(|n| n == self.records[&id].intent.nonce))
    }
    /// No job is retained and no child of this service is left to reap.
    pub fn idle(&self) -> bool {
        self.records.is_empty() && self.children.is_empty()
    }
    pub fn busy(&self) -> bool {
        self.records.values().any(|r| {
            self.workload_pending(r.info.id).unwrap_or(true)
                || (r.executor.as_ref().is_some_and(|e| e.host.is_some())
                    && self.stop_requested(r.info.id).unwrap_or(true)
                    && !self.stop_acked(r.info.id).unwrap_or(false))
        })
    }
    pub fn dismiss(&mut self, id: u64) -> Result<Info> {
        let record = self.records.get(&id).context("no such job")?;
        anyhow::ensure!(
            matches!(record.info.status.as_str(), "finished" | "failed"),
            "stop the running job before dismissing it"
        );
        anyhow::ensure!(!self.workload_pending(id)?, "stop the remaining workload before dismissing this result");
        anyhow::ensure!(
            !(record.executor.as_ref().is_some_and(|e| e.host.is_some())
                && record.info.reason.contains("unknown")
                && !self.stop_acked(id)?),
            "stop the unknown remote workload before dismissing this result"
        );
        store::write(&self.dir(id).join("dismissed"), &true)?;
        let r = self.records.get_mut(&id).unwrap();
        r.dismissed = true;
        Ok(r.info.clone())
    }
    pub fn refresh(&mut self) -> Result<()> {
        // Reap only children created by this daemon. Cold recovery uses durable
        // receipts; ECHILD never becomes an invented workload exit status.
        self.children.retain_mut(|child| match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(error) => error.raw_os_error() != Some(libc::ECHILD),
        });
        let ids: Vec<_> = self.records.keys().copied().collect();
        for &id in &ids {
            let dir = self.dir(id);
            if let Some(owner) = store::optional::<Identity>(&dir.join("owner.json"))? {
                owner.reap_if_child();
            }
            let record = self.records.get_mut(&id).unwrap();
            if !record.executor.as_ref().is_some_and(|e| e.host.is_some()) {
                if let Some(exit) = store::optional::<Exit>(&dir.join("exit.json"))? {
                    anyhow::ensure!(exit.nonce == record.intent.nonce, "job exit identity changed");
                    record.info.status =
                        if exit.code == Some(0) && exit.error.is_none() { "finished" } else { "failed" }.into();
                    record.info.finished_at = Some(exit.finished_at);
                    record.info.exit_code = exit.code;
                    record.info.signal = exit.signal;
                    record.info.reason =
                        exit.error.unwrap_or_else(|| if exit.stopped { "stopped".into() } else { String::new() });
                } else if let Some(launch) = store::optional::<Launch>(&dir.join("launch.json"))? {
                    anyhow::ensure!(launch.nonce == record.intent.nonce, "job launch identity changed");
                    record.info.started_at = Some(launch.started_at);
                    record.info.status = if launch.supervisor.alive() { "working" } else { "failed" }.into();
                    record.info.reason = if launch.supervisor.alive() {
                        String::new()
                    } else {
                        "supervisor lost; outcome unknown; automatic retry disabled".into()
                    };
                    if !launch.supervisor.alive() {
                        record.info.finished_at.get_or_insert(util::now_secs());
                        // Explicit stop still owns this workload after supervisor loss.
                        // Never target a reused process/group identity.
                        if store::optional::<String>(&dir.join("stop.json"))?.is_some_and(|n| n == record.intent.nonce)
                            && launch.child.alive()
                            && launch.pgid == launch.child.pid
                        {
                            let age = fs::metadata(dir.join("stop.json"))?.modified()?.elapsed().unwrap_or_default();
                            let signal = if age.as_secs() >= 5 { libc::SIGKILL } else { libc::SIGTERM };
                            unsafe {
                                libc::kill(-(launch.pgid as i32), signal);
                            }
                        }
                    }
                }
            }
            let remaining_workload = store::optional::<Launch>(&dir.join("launch.json"))?
                .is_some_and(|launch| launch.nonce == record.intent.nonce && launch.child.alive());
            let unresolved_remote = record.executor.as_ref().is_some_and(|e| e.host.is_some())
                && record.info.reason.contains("unknown")
                && !store::optional::<String>(&dir.join("stop-acked.json"))?
                    .is_some_and(|nonce| nonce == record.intent.nonce);
            // Expiry obeys the same custody boundary as explicit dismiss: a
            // failed/unknown row can still own a running workload.
            if !record.dismissed
                && self.retention_days > 0
                && !remaining_workload
                && !unresolved_remote
                && matches!(record.info.status.as_str(), "finished" | "failed")
                && record
                    .info
                    .finished_at
                    .is_some_and(|at| util::now_secs().saturating_sub(at) > self.retention_days.saturating_mul(86400))
            {
                record.dismissed = true;
                store::write(&dir.join("dismissed"), &true)?;
                // Keep the private idempotency intent/exit receipt, expire output.
                // Never race a live supervisor that still owns its descriptors.
                if store::optional::<Exit>(&dir.join("exit.json"))?.is_some()
                    && !store::optional::<Identity>(&dir.join("owner.json"))?.is_some_and(|p| p.alive())
                {
                    for name in ["output.log", "input", "execution.json", "supervisor.log"] {
                        match fs::remove_file(dir.join(name)) {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Err(e.into()),
                        }
                    }
                    fs::File::open(&dir)?.sync_all()?;
                }
            }
        }
        for id in ids {
            self.save_public(id)?;
        }
        Ok(())
    }
    pub fn spawned(&self, id: u64) -> Result<bool> {
        let dir = self.dir(id);
        if dir.join("launch.json").exists() || dir.join("exit.json").exists() {
            return Ok(true);
        }
        Ok(store::optional::<Identity>(&dir.join("owner.json"))?.is_some_and(|owner| owner.alive()))
    }
    pub fn start(&mut self, id: u64, exe: &Path, spec: Spec) -> Result<()> {
        let dir = self.dir(id);
        if self.spawned(id)? {
            return Ok(());
        }
        anyhow::ensure!(policy::permitted(&dir, &self.records[&id].intent)?, "work queue execution gate paused");
        if let Some(existing) = store::optional::<Spec>(&dir.join("execution.json"))? {
            anyhow::ensure!(existing == spec, "job execution plan is already pinned");
        } else {
            store::write(&dir.join("execution.json"), &spec)?;
        }
        let diagnostics = store::append_log(&dir.join("supervisor.log"))?;
        let child = Command::new(exe)
            .arg("__job-supervise")
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(diagnostics)
            .spawn()
            .context("cannot start job supervisor")?;
        self.children.push(child);
        self.records.get_mut(&id).unwrap().attempted_at = util::now_secs();
        Ok(())
    }
    /// Internal consumers may retire fully consumed, reaped known-exit receipts.
    /// Public jobs retain their existing idempotency and retention behavior.
    pub(crate) fn purge_completed(&mut self, id: u64) -> Result<()> {
        let record = self.records.get(&id).context("no such private job")?;
        let dir = self.dir(id);
        let exit: Exit = store::read(&dir.join("exit.json"))?;
        anyhow::ensure!(
            exit.nonce == record.intent.nonce
                && !self.workload_pending(id)?
                && !store::optional::<Identity>(&dir.join("owner.json"))?.is_some_and(|owner| owner.alive()),
            "private job custody is not quiescent"
        );
        store::check_directory(&dir)?;
        fs::remove_dir_all(&dir)?;
        fs::File::open(&self.root)?.sync_all()?;
        self.records.remove(&id);
        self.persisted.remove(&id);
        Ok(())
    }
    pub fn log(&self, id: u64, offset: u64, limit: u64) -> Result<serde_json::Value> {
        use std::io::{Read, Seek, SeekFrom};
        let record = self.records.get(&id).context("no such job")?;
        let path = self.dir(id).join("output.log");
        let mut bytes = Vec::new();
        let mut snapshot = Vec::new();
        let mut length = 0;
        let mut truncated = false;
        if path.exists() {
            let mut file = store::open_log(&path)?;
            length = file.metadata()?.len();
            anyhow::ensure!(offset <= length, "job log cursor exceeds current log");
            file.seek(SeekFrom::Start(offset))?;
            (&mut file).take(limit.clamp(4, 262144)).read_to_end(&mut bytes)?;
            let terminal = matches!(record.info.status.as_str(), "finished" | "failed");
            if !bytes.is_empty() && (!terminal || offset + (bytes.len() as u64) < length) {
                // Preserve valid multibyte text when a page cuts a character.
                let mut start = bytes.len() - 1;
                while start > bytes.len().saturating_sub(4) && bytes[start] & 0xc0 == 0x80 {
                    start -= 1;
                }
                let expected = match bytes[start] {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => 1,
                };
                if expected > bytes.len() - start {
                    bytes.truncate(start);
                }
            }
            const CHAT_WINDOW: u64 = 1048576;
            let from = length.saturating_sub(CHAT_WINDOW);
            truncated = from > 0;
            file.seek(SeekFrom::Start(from))?;
            file.take(CHAT_WINDOW).read_to_end(&mut snapshot)?;
            if truncated {
                // A tail may begin in a JSON record or UTF-8 character.
                let skip = snapshot.iter().position(|b| *b == b'\n').map_or(snapshot.len(), |n| n + 1);
                snapshot.drain(..skip);
            }
        } else {
            anyhow::ensure!(offset == 0, "job log cursor exceeds current log");
        }
        let next = offset + bytes.len() as u64;
        let text = String::from_utf8_lossy(&bytes);
        let mut items = stream::items(record.info.provider, &String::from_utf8_lossy(&snapshot));
        if items.len() > 600 {
            items.drain(..items.len() - 600);
            truncated = true;
        }
        let eof = matches!(record.info.status.as_str(), "finished" | "failed") && next >= length;
        Ok(serde_json::json!({"job":id,"offset":next,"text":text,"items":items,"items_truncated":truncated,"eof":eof}))
    }
}

#[cfg(test)]
#[path = "../tests/job_sessions/store.rs"]
mod tests;
