//! Private immutable batches plus small changed-only progress receipts.
use super::*;
use crate::job_sessions::store as private;
use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    #[serde(default)]
    pub dispatch_key: String,
    #[serde(default)]
    pub enqueue_order: u64,
    #[serde(default)]
    pub turn_anchor: Option<u64>,
    pub info: Info,
    pub attempted: bool,
    pub executor: Option<String>,
    #[serde(default)]
    pub executor_cwd: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub spec: Spec,
    pub progress: Progress,
    #[serde(skip)]
    pub origin: PathBuf,
}
impl std::ops::Deref for Record {
    type Target = Progress;
    fn deref(&self) -> &Progress {
        &self.progress
    }
}
impl std::ops::DerefMut for Record {
    fn deref_mut(&mut self) -> &mut Progress {
        &mut self.progress
    }
}
pub struct Queue {
    pub root: PathBuf,
    pub records: BTreeMap<u64, Record>,
    pub enabled: bool,
    pub policy_revision: u64,
    pub error: Option<String>,
    #[cfg(test)]
    pub fail_batch_sync: bool,
    pub sources: BTreeMap<String, super::sources::Source>,
    pub chores: BTreeMap<String, super::chores::Chore>,
    pub last_poll: u64,
    pub refresh_jobs: crate::job_sessions::Jobs,
}
pub fn identity(key: &str) -> u64 {
    let hash = ring::digest::digest(&ring::digest::SHA256, key.as_bytes());
    u64::from_be_bytes(hash.as_ref()[..8].try_into().unwrap()) & ((1u64 << 40) - 1)
}
pub fn valid_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}
impl Spec {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(valid_text(&self.key, 128) && valid_text(&self.title, 200), "invalid queue identity or title");
        anyhow::ensure!(
            self.cwd.is_absolute() && self.cwd.is_dir(),
            "queue cwd must be an existing absolute directory"
        );
        anyhow::ensure!(valid_text(&self.placement, 128), "invalid queue placement");
        anyhow::ensure!(
            self.budget.is_none_or(|n| n.is_finite() && n > 0.0),
            "budget must be a finite positive USD amount"
        );
        anyhow::ensure!(self.model.as_ref().is_none_or(|s| valid_text(s, 256)), "invalid model");
        anyhow::ensure!(self.account.as_ref().is_none_or(|s| valid_text(s, 64)), "invalid account");
        anyhow::ensure!(self.dependencies.len() <= 100, "too many dependencies");
        match self.kind.as_str() {
            "job" => crate::job_sessions::Spec {
                argv: self.argv.clone(),
                cwd: self.cwd.clone(),
                provider: self.provider,
                account: self.account.clone(),
                stdin: self.stdin.clone().unwrap_or_default(),
                env_clear: self.env_clear,
                env_allowlist: self.env_allowlist.clone(),
                env: self.env.clone(),
            }
            .validate()?,
            "session" => {
                anyhow::ensure!(
                    self.provider.is_some_and(Tool::is_agent),
                    "interactive queue item requires an agent provider"
                );
                anyhow::ensure!(
                    self.argv.is_empty()
                        && self.stdin.is_none()
                        && self.env.is_empty()
                        && self.env_allowlist.is_empty()
                        && !self.env_clear,
                    "interactive queue payload must be a prompt"
                );
                anyhow::ensure!(
                    self.prompt.as_ref().is_some_and(|s| !s.is_empty() && s.len() <= 16000 && !s.contains('\0')),
                    "invalid finite interactive prompt"
                );
            }
            _ => bail!("queue kind must be job or session"),
        }
        for d in &self.dependencies {
            anyhow::ensure!(
                matches!(d.condition.as_str(), "finished" | "ready" | "merged"),
                "invalid dependency condition"
            );
            anyhow::ensure!(
                match (&d.queue, &d.source, &d.task) {
                    (Some(_), None, None) => d.condition == "finished",
                    (None, Some(s), Some(t)) => valid_text(s, 256) && valid_text(t, 256) && d.condition != "finished",
                    _ => false,
                },
                "dependency requires queue/finished or source/task/ready|merged"
            );
        }
        anyhow::ensure!(serde_json::to_vec(self)?.len() <= 262144, "queue payload too large");
        Ok(())
    }
}
impl Queue {
    pub fn load(state: &Path) -> Result<Self> {
        let root = state.join("work-queue");
        private::directory(&root)?;
        let legacy = private::optional::<bool>(&root.join("enabled.json"))?.unwrap_or(false);
        let policy = private::optional::<(u64, bool)>(&root.join("policy.json"))?.unwrap_or((1, legacy));
        let enabled = policy.1;
        let runtime = root.join("source-runtime");
        private::directory(&runtime)?;
        let mut q = Self {
            root: root.clone(),
            records: BTreeMap::new(),
            enabled,
            policy_revision: policy.0,
            error: None,
            #[cfg(test)]
            fail_batch_sync: false,
            sources: BTreeMap::new(),
            chores: BTreeMap::new(),
            last_poll: 0,
            refresh_jobs: crate::job_sessions::Jobs::load_with_retention(&runtime, 0)?,
        };
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("batch-") {
                private::check_directory(&entry.path())?;
                for file in fs::read_dir(entry.path())? {
                    let path = file?.path();
                    let mut r: Record = private::read(&path)?;
                    anyhow::ensure!(r.info.id == identity(&r.spec.key), "invalid queue receipt identity");
                    r.origin = path;
                    if let Some(progress) = private::optional::<Progress>(&q.progress_path(r.info.id))? {
                        r.progress = progress;
                    }
                    anyhow::ensure!(q.records.insert(r.info.id, r).is_none(), "duplicate queue receipt");
                }
            } else if name.starts_with("source-") && name.ends_with(".json") {
                let source: super::sources::Source = private::read(&entry.path())?;
                q.sources.insert(source.spec.id.clone(), source);
            } else if name.starts_with("chore-") && name.ends_with(".json") {
                let chore: super::chores::Chore = private::read(&entry.path())?;
                q.chores.insert(chore.spec.id.clone(), chore);
            }
        }
        anyhow::ensure!(
            q.records.len() <= 10000 && q.sources.len() <= 100 && q.chores.len() <= 100,
            "queue storage limit exceeded"
        );
        if let Some(ids) = private::optional::<Vec<u64>>(&root.join("order.json"))? {
            for (n, id) in ids.iter().enumerate() {
                if let Some(r) = q.records.get_mut(id) {
                    r.info.priority = (ids.len() - n) as i32;
                }
            }
        }
        Ok(q)
    }
    pub fn progress_path(&self, id: u64) -> PathBuf {
        self.root.join(format!("state-{id:010x}.json"))
    }
    pub fn save(&self, id: u64) -> Result<()> {
        private::write(&self.progress_path(id), &self.records.get(&id).context("no such queue item")?.progress)
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_where(|_| true)
    }
    /// A snapshot with only the task rows `keep` accepts; each source counts the rest.
    pub fn snapshot_where(&self, keep: impl Fn(&Task) -> bool) -> Snapshot {
        let now = crate::util::now_secs();
        let mut tasks = Vec::new();
        let mut sources = Vec::with_capacity(self.sources.len());
        for source in self.sources.values() {
            let before = tasks.len();
            tasks.extend(source.tasks.iter().filter(|t| keep(t)).cloned());
            let mut info = source.info(now);
            info.omitted = source.tasks.len() - (tasks.len() - before);
            sources.push(info);
        }
        Snapshot {
            enabled: self.enabled,
            pause_pending: Vec::new(),
            pause_effective: !self.enabled,
            items: self.ordered().into_iter().map(|id| self.records[&id].info.clone()).collect(),
            sources,
            tasks,
            chores: self.chores.values().map(|c| c.info(self.enabled, crate::util::now_secs())).collect(),
        }
    }
    pub fn ordered(&self) -> Vec<u64> {
        let mut ids: Vec<_> = self.records.keys().copied().collect();
        ids.sort_by_key(|id| {
            let r = &self.records[id];
            (std::cmp::Reverse(r.info.priority), r.enqueue_order, r.info.created_at, *id)
        });
        ids
    }
    pub fn set_enabled(&mut self, enabled: bool) -> Result<()> {
        if self.enabled != enabled {
            let revision = self.policy_revision.checked_add(1).context("queue policy revision exhausted")?;
            let path = self.root.join("policy.json");
            if let Err(error) = private::write(&path, &(revision, enabled)) {
                // A post-rename barrier failure has already accepted the new
                // policy. Keep live state aligned and fence further publication.
                if private::optional::<(u64, bool)>(&path).ok().flatten() == Some((revision, enabled)) {
                    self.policy_revision = revision;
                    self.enabled = enabled;
                    self.error = Some("queue policy durability unconfirmed".into());
                }
                return Err(error);
            }
            self.policy_revision = revision;
            self.enabled = enabled;
        }
        Ok(())
    }
    pub fn add_batch(&mut self, specs: Vec<Spec>, now: u64) -> Result<Vec<Info>> {
        anyhow::ensure!(!specs.is_empty() && specs.len() <= 100, "batch must contain 1–100 items");
        let mut staged = BTreeMap::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut order = Vec::new();
        let sequence = self.records.values().map(|r| r.enqueue_order).max().unwrap_or(0);
        for spec in specs {
            spec.validate()?;
            let id = identity(&spec.key);
            anyhow::ensure!(seen.insert(id), "duplicate key in batch");
            anyhow::ensure!(id != 0, "invalid queue key identity");
            anyhow::ensure!(!staged.contains_key(&id), "duplicate key in batch");
            if let Some(old) = self.records.get(&id) {
                anyhow::ensure!(old.spec == spec, "queue key already belongs to a different payload");
                order.push(old.info.clone());
                continue;
            }
            let info = Info {
                kind: spec.kind.clone(),
                id,
                title: spec.title.clone(),
                state: "queued".into(),
                priority: spec.priority,
                created_at: now,
                updated_at: now,
                reason: "queued".into(),
                dispatch: None,
                dependencies: spec.dependencies.clone(),
                source: spec.source.clone(),
                budget: spec.budget,
                cancelable: true,
                provider: spec.provider,
                model: spec.model.clone(),
                placement: spec.placement.clone(),
            };
            order.push(info.clone());
            staged.insert(
                id,
                Record {
                    spec,
                    progress: Progress {
                        enqueue_order: sequence + order.len() as u64,
                        info,
                        attempted: false,
                        executor: None,
                        executor_cwd: None,
                        dispatch_key: format!("queue-{now}-{}", crate::util::uuid_v4()?),
                        turn_anchor: None,
                    },
                    origin: PathBuf::new(),
                },
            );
        }
        anyhow::ensure!(
            self.records.len() + staged.len() <= 10000,
            "queue capacity reached; terminal records expire after retention"
        );
        for (&id, r) in &staged {
            for d in &r.spec.dependencies {
                if let Some(target) = d.queue {
                    anyhow::ensure!(
                        target != id && (self.records.contains_key(&target) || staged.contains_key(&target)),
                        "missing or self dependency"
                    );
                }
                if let (Some(source), Some(task)) = (&d.source, &d.task) {
                    anyhow::ensure!(
                        self.sources.get(source).is_some_and(|s| s.tasks.iter().any(|t| &t.id == task)),
                        "missing source task"
                    );
                }
            }
        }
        fn visit(id: u64, all: &BTreeMap<u64, Record>, new: &BTreeMap<u64, Record>, path: &mut BTreeSet<u64>) -> bool {
            if path.len() >= 128 || !path.insert(id) {
                return false;
            }
            let Some(r) = new.get(&id).or_else(|| all.get(&id)) else { return false };
            let valid = r.spec.dependencies.iter().filter_map(|d| d.queue).all(|n| visit(n, all, new, path));
            path.remove(&id);
            valid
        }
        for id in staged.keys() {
            anyhow::ensure!(visit(*id, &self.records, &staged, &mut BTreeSet::new()), "cyclic queue dependency");
        }
        if staged.is_empty() {
            if self.error.is_some() {
                self.publication_barrier()?;
            }
            return Ok(order);
        }
        let nonce = crate::util::uuid_v4()?;
        let temp = self.root.join(format!(".batch-{nonce}"));
        private::directory(&temp)?;
        for (id, r) in &staged {
            private::write(&temp.join(format!("{id:010x}.json")), r)?;
        }
        let target = self.root.join(format!("batch-{nonce}"));
        fs::rename(&temp, &target)?;
        // Rename has already published these immutable keys. Keep them in
        // memory even when the directory barrier reports an ambiguous error.
        for (id, mut r) in staged {
            r.origin = target.join(format!("{id:010x}.json"));
            self.records.insert(id, r);
        }
        self.publication_barrier()?;
        Ok(order)
    }
    pub fn publication_barrier(&mut self) -> Result<()> {
        let result = (|| -> Result<()> {
            #[cfg(test)]
            if self.fail_batch_sync {
                anyhow::bail!("injected post-rename directory barrier failure");
            }
            fs::File::open(&self.root)?.sync_all()?;
            Ok(())
        })();
        self.error = result.as_ref().err().map(|_| "queue publication recovery required".into());
        result
    }
    pub fn transition(
        &mut self,
        id: u64,
        state: &str,
        reason: &str,
        now: u64,
        dispatch: Option<Dispatch>,
    ) -> Result<()> {
        let r = self.records.get_mut(&id).context("no such queue item")?;
        if r.info.state == state && r.info.reason == reason && r.info.dispatch == dispatch {
            return Ok(());
        }
        let old = r.progress.clone();
        r.info.state = state.into();
        r.info.reason = reason.into();
        r.info.dispatch = dispatch;
        r.info.updated_at = now;
        r.info.cancelable = !r.attempted && r.info.dispatch.is_none() && matches!(state, "queued" | "held");
        if let Err(e) = self.save(id) {
            self.records.get_mut(&id).unwrap().progress = old;
            return Err(e);
        }
        Ok(())
    }
    pub fn begin_attempt(&mut self, id: u64) -> Result<()> {
        let r = self.records.get_mut(&id).context("no such queue item")?;
        if r.attempted {
            return Ok(());
        }
        r.attempted = true;
        r.info.cancelable = false;
        if let Err(e) = self.save(id) {
            let r = self.records.get_mut(&id).unwrap();
            r.attempted = false;
            r.info.cancelable = true;
            return Err(e);
        }
        Ok(())
    }
    pub fn cancel(&mut self, id: u64, now: u64) -> Result<Info> {
        let r = self.records.get(&id).context("no such queue item")?;
        anyhow::ensure!(
            r.info.cancelable && !r.attempted && r.info.dispatch.is_none(),
            "item ownership was already submitted; stop its dispatched handle explicitly"
        );
        self.transition(id, "failed", "cancelled", now, None)?;
        Ok(self.records[&id].info.clone())
    }
    pub fn reorder(&mut self, ids: &[u64], now: u64) -> Result<Vec<Info>> {
        let eligible: BTreeSet<_> = self.records.values().filter(|r| r.info.cancelable).map(|r| r.info.id).collect();
        let given: BTreeSet<_> = ids.iter().copied().collect();
        anyhow::ensure!(
            ids.len() == given.len() && given == eligible,
            "reorder requires the exact current cancelable item set"
        );
        private::write(&self.root.join("order.json"), &ids)?;
        for (n, id) in ids.iter().enumerate() {
            let r = self.records.get_mut(id).unwrap();
            r.info.priority = (ids.len() - n) as i32;
            r.info.updated_at = now;
        }
        Ok(self.snapshot().items)
    }
    pub fn dependencies_ready(&self, id: u64, now: u64) -> bool {
        self.records[&id].spec.dependencies.iter().all(|d| {
            if let Some(q) = d.queue {
                return self.records.get(&q).is_some_and(|r| r.info.state == "done");
            }
            let Some(source) = d.source.as_ref().and_then(|id| self.sources.get(id)) else { return false };
            source.info(now).fresh
                && source.tasks.iter().any(|t| Some(&t.id) == d.task.as_ref() && t.state == d.condition)
        })
    }
    pub fn gc(&mut self, now: u64) -> Result<()> {
        let needed: BTreeSet<_> = self
            .records
            .values()
            .filter(|r| !matches!(r.info.state.as_str(), "done" | "failed"))
            .flat_map(|r| r.spec.dependencies.iter().filter_map(|d| d.queue))
            .collect();
        let expired: Vec<_> = self
            .records
            .values()
            .filter(|r| {
                matches!(r.info.state.as_str(), "done" | "failed")
                    && now.saturating_sub(r.info.updated_at) > 30 * 86400
                    && !needed.contains(&r.info.id)
            })
            .map(|r| r.info.id)
            .collect();
        for id in expired {
            let r = &self.records[&id];
            fs::remove_file(&r.origin)?;
            fs::File::open(r.origin.parent().context("missing batch parent")?)?.sync_all()?;
            let progress = self.progress_path(id);
            if progress.exists() {
                fs::remove_file(progress)?;
                fs::File::open(&self.root)?.sync_all()?;
            }
            let idle = self.root.join(format!("idle-{:010x}.json", identity(&r.dispatch_key)));
            if idle.exists() {
                fs::remove_file(idle)?;
                fs::File::open(&self.root)?.sync_all()?;
            }
            self.records.remove(&id);
        }
        Ok(())
    }
}
#[cfg(test)]
#[path = "../tests/work_queue/store.rs"]
mod tests;
