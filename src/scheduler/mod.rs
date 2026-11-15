//! Host-owned scheduling primitives; no model calls or repository authority.
#[path = "../../plugins/fleet_workspace/placement.rs"]
pub mod placement;
pub mod probe;
#[cfg(test)]
use crate::protocol::Usage;
use crate::{procs, protocol::Tool, util};
use anyhow::{Context, Result};
pub use probe::Headroom;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File},
    os::fd::{AsRawFd, FromRawFd, RawFd},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub pools: BTreeMap<String, usize>,
    pub probe: probe::Probe,
    pub max_load_per_cpu: f64,
    pub max_load_wait: u64,
    pub stagger_seconds: u64,
    pub quota_percent: f64,
    pub repositories: BTreeMap<String, PathBuf>,
    /// The identity this service discovered for its own checkout, which a
    /// later discovery may replace; explicit mappings are never replaced.
    #[serde(skip)]
    pub discovered: Option<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            pools: BTreeMap::new(),
            probe: probe::Probe::default(),
            max_load_per_cpu: 1.5,
            max_load_wait: 600,
            stagger_seconds: 5,
            quota_percent: 90.0,
            repositories: BTreeMap::new(),
            discovered: None,
        }
    }
}
impl Settings {
    /// Explicit host mappings take precedence over shallow-clone root hashes.
    pub fn add_discovered_repository(&mut self, id: String, root: PathBuf) {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        let common = git_location(&root).map(|(_, common)| common);
        if self.repositories.values().any(|configured| {
            configured.canonicalize().unwrap_or_else(|_| configured.clone()) == canonical
                || common
                    .as_ref()
                    .is_some_and(|common| git_location(configured).is_some_and(|(_, configured)| configured == *common))
        }) {
            return;
        }
        if !self.repositories.contains_key(&id) {
            self.repositories.insert(id.clone(), root);
            self.discovered = Some(id);
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Owner {
    pub pid: u32,
    pub start_ticks: u64,
    pub boot: String,
    pub session: Option<u64>,
}
impl Owner {
    pub fn read(pid: u32, session: Option<u64>) -> Result<Self> {
        let stat = procs::stat(pid).context("holder process no longer exists")?;
        anyhow::ensure!(pid > 1 && !matches!(stat.state, 'Z' | 'X'), "invalid holder process");
        Ok(Self {
            pid,
            start_ticks: stat.start_ticks,
            boot: fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().into(),
            session,
        })
    }
    pub fn alive(&self) -> bool {
        !self.boot.is_empty()
            && fs::read_to_string("/proc/sys/kernel/random/boot_id").is_ok_and(|boot| self.boot == boot.trim())
            && procs::alive_with_start(self.pid, Some(self.start_ticks))
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    #[serde(default)]
    pub load_waited: u64,
    #[serde(default)]
    pub load_blocked_at: Option<u64>,
    #[serde(default)]
    pub key: String,
    pub id: u64,
    pub pool: String,
    pub owner: Owner,
    pub label: String,
    pub queued_at: u64,
    pub granted_at: Option<u64>,
    pub number: Option<usize>,
    #[serde(default)]
    pub compatibility: Option<PathBuf>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub paused: Vec<Pause>,
    #[serde(default)]
    pub error: Option<String>,
    pub memory_free: bool,
    pub admitted: bool,
    pub headroom: Headroom,
    pub pools: BTreeMap<String, usize>,
    pub holders: Vec<Slot>,
    pub waiters: Vec<Slot>,
    pub starts: Vec<Start>,
    pub reruns: Vec<Rerun>,
    pub repositories: BTreeMap<String, String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pause {
    pub session: u64,
    pub provider: Tool,
    pub reset: Option<u64>,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Start {
    #[serde(default = "auto_host")]
    pub host: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub attempted_at: Option<u64>,
    #[serde(default)]
    pub account: Option<String>,
    pub id: u64,
    pub provider: Tool,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub prompt: Option<String>,
    pub fallback: Option<Tool>,
    pub reason: String,
    pub reset: Option<u64>,
    pub session: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rerun {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub conversation: Option<String>,
    pub id: u64,
    pub session: u64,
    pub command: String,
    pub at: u64,
    pub authoritative: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub memory_events: BTreeMap<String, u64>,
    #[serde(default)]
    pub memory_dismissed: BTreeMap<String, u64>,
    #[serde(default)]
    pub account_cursor: BTreeMap<String, usize>,
    #[serde(default)]
    /// Zero is a durable uncertain launch, never a successful session ID.
    pub launched: BTreeMap<String, u64>,
    #[serde(default)]
    pub cancelled: BTreeMap<String, u64>,
    pub next: u64,
    pub slots: Vec<Slot>,
    pub starts: Vec<Start>,
    pub reruns: Vec<Rerun>,
    pub last_started: BTreeMap<String, u64>,
}
fn auto_host() -> String {
    "auto".into()
}
pub struct Scheduler {
    pub memory_events: BTreeMap<String, u64>,
    pub memory_dismissed: BTreeMap<String, u64>,
    last_tick: std::time::Instant,
    pub account_cursor: BTreeMap<String, usize>,
    pub error: Option<String>,
    pub launched: BTreeMap<String, u64>,
    pub last_poll: u64,
    pub cancelled: BTreeMap<String, u64>,
    pub settings: Settings,
    repository_commons: BTreeMap<PathBuf, String>,
    pub headroom: Headroom,
    pub slots: VecDeque<Slot>,
    pub starts: Vec<Start>,
    pub reruns: Vec<Rerun>,
    pub last_started: BTreeMap<String, u64>,
    next: u64,
    locks: BTreeMap<u64, File>,
    state: PathBuf,
    /// The bytes the state file durably holds, so an unchanged state costs
    /// no disk read, write or fsync on the service loop.
    durable: std::cell::RefCell<Option<Vec<u8>>>,
}
impl Scheduler {
    #[cfg(test)]
    pub fn new(settings: Settings, state_dir: &Path) -> Self {
        Self::load(settings, state_dir, false)
    }
    pub fn load(mut settings: Settings, state_dir: &Path, deferred: bool) -> Self {
        let state = state_dir.join("scheduler.json");
        let loaded = (|| -> Result<(Saved, Option<Vec<u8>>)> {
            match fs::metadata(&state) {
                Ok(meta) => {
                    anyhow::ensure!(meta.len() <= 32 << 20, "scheduler state exceeds 32 MiB; preserved for recovery")
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Saved::default(), None)),
                Err(e) => return Err(e.into()),
            }
            let bytes = fs::read(&state)?;
            Ok((serde_json::from_slice(&bytes)?, Some(bytes)))
        })();
        let (saved, durable, mut error) = match loaded {
            Ok((saved, bytes)) => (saved, bytes, None),
            Err(e) => (Saved::default(), None, Some(format!("Scheduler recovery required: {e:#}"))),
        };
        let floor = saved
            .slots
            .iter()
            .map(|s| s.id)
            .chain(saved.starts.iter().map(|s| s.id))
            .chain(saved.reruns.iter().map(|r| r.id))
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        let mut repository_commons = BTreeMap::new();
        let mut repository_roots = BTreeMap::new();
        for (id, root) in &mut settings.repositories {
            let canonical = root.is_absolute().then(|| root.canonicalize().ok()).flatten().filter(|root| root.is_dir());
            let Some(canonical) = canonical else {
                error = Some("Repository mappings require existing absolute directories".into());
                continue;
            };
            *root = canonical.clone();
            let duplicate_root = repository_roots.insert(canonical, id.clone()).is_some();
            let duplicate_common =
                git_location(root).is_some_and(|(_, common)| repository_commons.insert(common, id.clone()).is_some());
            if duplicate_root || duplicate_common {
                error = Some(
                    "Conflicting repository mappings: each root and Git common directory needs one explicit identity"
                        .into(),
                );
            }
        }
        let mut s = Self {
            repository_commons,
            last_tick: std::time::Instant::now() - std::time::Duration::from_secs(1),
            memory_events: saved.memory_events,
            memory_dismissed: saved.memory_dismissed,
            account_cursor: saved.account_cursor,
            error,
            launched: saved.launched,
            last_poll: 0,
            cancelled: saved.cancelled,
            headroom: settings.probe.read(),
            settings,
            slots: saved.slots.into(),
            starts: saved.starts,
            reruns: saved.reruns,
            last_started: saved.last_started,
            next: saved.next.max(floor).max(1),
            locks: BTreeMap::new(),
            state,
            durable: std::cell::RefCell::new(durable),
        };
        if !deferred {
            s.recover(&[]);
        }
        s
    }
    /// A replica deepened after this service started gains the fleet's
    /// root-commit identity; WSL's shallow clone reported another until a
    /// hand-written mapping was added. Replace only the identity this service
    /// itself discovered for the same checkout, never an explicit mapping.
    /// True when the registered identity changed.
    pub fn rediscover_repository(&mut self, id: String, root: PathBuf) -> bool {
        if self.settings.discovered.as_deref() == Some(id.as_str()) {
            return false;
        }
        let Some(root) = root.canonicalize().ok().filter(|root| root.is_dir()) else { return false };
        let mut settings = self.settings.clone();
        let old = settings.discovered.take();
        if let Some(old) = &old {
            if settings.repositories.get(old) != Some(&root) {
                return false;
            }
            settings.repositories.remove(old);
        }
        settings.add_discovered_repository(id.clone(), root.clone());
        if settings.discovered.as_deref() != Some(id.as_str()) {
            return false;
        }
        if let Some(old) = &old {
            self.repository_commons.retain(|_, known| known != old);
        }
        if let Some((_, common)) = git_location(&root) {
            self.repository_commons.insert(common, id);
        }
        self.settings = settings;
        true
    }
    /// Registered identity is precomputed before the event loop. Sibling
    /// worktrees resolve their common Git directory using files only.
    pub fn repository_for(&self, cwd: &Path) -> Option<(String, PathBuf)> {
        if self.error.is_some() || !cwd.is_absolute() {
            return None;
        }
        let cwd = cwd.canonicalize().ok()?;
        if !cwd.is_dir() {
            return None;
        }
        if let Some((id, root)) = self
            .settings
            .repositories
            .iter()
            .filter(|(_, root)| cwd.starts_with(root))
            .max_by_key(|(_, root)| root.components().count())
        {
            if let Some((native, common)) = git_location(&cwd) {
                if native != *root && native.starts_with(root) {
                    // A nested Git checkout cannot borrow its containing
                    // repository's identity or silently switch task branches.
                    return (self.repository_commons.get(&common) == Some(id)).then(|| (id.clone(), native));
                }
            }
            return Some((id.clone(), root.clone()));
        }
        let (root, common) = git_location(&cwd)?;
        self.repository_commons.get(&common).map(|id| (id.clone(), root))
    }
    pub fn placed_directory(&self, repository: &str, cwd: &Path) -> Result<PathBuf> {
        anyhow::ensure!(self.error.is_none(), "scheduler recovery required");
        anyhow::ensure!(!repository.is_empty() && repository.len() <= 256, "invalid repository identity");
        let root = self.settings.repositories.get(repository).context("repository is not registered on this host")?;
        anyhow::ensure!(root.canonicalize().ok().as_ref() == Some(root), "registered repository root changed");
        anyhow::ensure!(cwd.is_absolute(), "placed cwd must be absolute");
        let cwd = cwd.canonicalize().context("placed working directory is unavailable")?;
        anyhow::ensure!(cwd.is_dir() && cwd.starts_with(root), "placed cwd escapes its registered repository");
        Ok(cwd)
    }
    pub fn capacity(&self, pool: &str) -> usize {
        self.settings
            .pools
            .get(pool)
            .copied()
            .unwrap_or_else(|| if pool == "heavy" { (self.headroom.cpus as usize / 4).clamp(1, 3) } else { 1 })
            .clamp(1, 100)
    }
    pub fn id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }
    pub fn hold(
        &mut self,
        pool: &str,
        owner: Owner,
        label: &str,
        compatibility: Option<PathBuf>,
        key: String,
    ) -> Result<u64> {
        anyhow::ensure!(self.error.is_none(), "scheduler recovery required");
        anyhow::ensure!(
            !key.is_empty()
                && key.len() <= 128
                && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "invalid request key"
        );
        anyhow::ensure!(!self.cancelled.contains_key(&key), "slot request was cancelled");
        if let Some(existing) = self.slots.iter().find(|s| s.key == key) {
            anyhow::ensure!(existing.owner == owner, "request key belongs to another process");
            return Ok(existing.id);
        }
        anyhow::ensure!(crate::extensions::valid_id(pool), "invalid pool name");
        anyhow::ensure!(label.len() <= 200 && !label.chars().any(char::is_control), "invalid slot label");
        anyhow::ensure!(self.slots.len() < 1000, "slot queue is full");
        if let Some(path) = &compatibility {
            let meta = fs::symlink_metadata(path).context("compatibility queue directory")?;
            anyhow::ensure!(
                meta.is_dir() && !meta.file_type().is_symlink() && meta.uid() == unsafe { libc::geteuid() },
                "compatibility directory must be owned by this user"
            );
        }
        self.last_tick = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let id = self.id();
        self.slots.push_back(Slot {
            load_waited: 0,
            load_blocked_at: None,
            key,
            id,
            pool: pool.into(),
            owner,
            label: label.into(),
            queued_at: util::now_secs(),
            granted_at: None,
            number: None,
            compatibility,
        });
        if let Err(error) = self.save() {
            self.slots.pop_back();
            return Err(error);
        }
        Ok(id)
    }
    pub fn cancel_key(&mut self, key: &str) -> Result<()> {
        anyhow::ensure!(!key.is_empty() && key.len() <= 128, "invalid request key");
        let now = util::now_secs();
        self.cancelled.retain(|_, at| now.saturating_sub(*at) < 86400);
        anyhow::ensure!(
            self.cancelled.len() < 10000 || self.cancelled.contains_key(key),
            "too many cancelled requests"
        );
        self.cancelled.insert(key.into(), now);
        let ids: Vec<_> = self.slots.iter().filter(|s| s.key == key).map(|s| s.id).collect();
        for id in ids {
            let _ = self.release(id);
        }
        self.save()
    }
    pub fn release(&mut self, id: u64) -> Result<()> {
        self.last_tick = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let before = self.slots.len();
        self.slots.retain(|s| s.id != id);
        self.locks.remove(&id);
        anyhow::ensure!(before != self.slots.len(), "no such slot request");
        self.save()
    }
    pub fn ended(&mut self, session: u64) {
        let ids: Vec<_> = self.slots.iter().filter(|s| s.owner.session == Some(session)).map(|s| s.id).collect();
        for id in ids {
            let _ = self.release(id);
        }
        self.reruns.retain(|r| r.session != session);
        self.memory_events.retain(|key, _| !key.starts_with(&format!("{session}:")));
        self.memory_dismissed.retain(|key, _| !key.starts_with(&format!("{session}:")));
    }
    pub fn tick(&mut self) {
        if self.last_tick.elapsed() < std::time::Duration::from_millis(100) {
            return;
        }
        self.last_tick = std::time::Instant::now();
        if self.error.is_some() {
            return;
        }
        self.headroom = self.settings.probe.read();
        let now = util::now_secs();
        let dead: Vec<_> = self.slots.iter().filter(|s| !s.owner.alive()).map(|s| s.id).collect();
        for id in dead {
            let _ = self.release(id);
        }
        let pools: std::collections::BTreeSet<_> = self.slots.iter().map(|s| s.pool.clone()).collect();
        for pool in pools {
            while let Some(index) = self.slots.iter().position(|s| s.pool == pool && s.granted_at.is_none()) {
                let held = self.slots.iter().filter(|s| s.pool == pool && s.granted_at.is_some()).count();
                if held >= self.capacity(&pool) || !self.headroom.admits() {
                    self.slots[index].load_blocked_at = None;
                    break;
                }
                let mut slot = self.slots[index].clone();
                // A compatibility holder still occupies a slot; that wait never spends the load allowance.
                if !self.lock(&mut slot) {
                    self.slots[index].load_blocked_at = None;
                    break;
                }
                if !self.headroom.calm(self.settings.max_load_per_cpu) {
                    if let Some(at) = slot.load_blocked_at {
                        slot.load_waited = slot.load_waited.saturating_add(now.saturating_sub(at));
                    }
                    slot.load_blocked_at = Some(now);
                    self.locks.remove(&slot.id);
                    slot.number = None;
                    self.slots[index] = slot;
                    break;
                }
                slot.load_blocked_at = None;
                slot.granted_at = Some(now);
                self.slots[index] = slot;
            }
        }
        let _ = self.save();
    }
    fn lock(&mut self, slot: &mut Slot) -> bool {
        if self.locks.contains_key(&slot.id) {
            return true;
        }
        let dir =
            slot.compatibility.clone().unwrap_or_else(|| self.state.parent().unwrap().join("slots").join(&slot.pool));
        if util::ensure_private_dir(&dir).is_err() {
            return false;
        }
        let queue = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("queue.lock"));
        let Ok(queue) = queue else { return false };
        if unsafe { libc::flock(queue.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return false;
        }
        let legacy = fs::read(dir.join("config.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["slots"].as_u64())
            .filter(|n| *n > 0)
            .map(|n| n.min(100) as usize);
        let capacity = legacy.map_or(self.capacity(&slot.pool), |n| n.min(self.capacity(&slot.pool)));
        for number in 0..capacity {
            if slot.number.is_some_and(|n| n != number) {
                continue;
            }
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(dir.join(format!("slot-{number}.lock")));
            if let Ok(file) = file {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    slot.number = Some(number);
                    self.locks.insert(slot.id, file);
                    return true;
                }
            }
        }
        false
    }
    pub fn saved(&self) -> Saved {
        Saved {
            memory_events: self.memory_events.clone(),
            memory_dismissed: self.memory_dismissed.clone(),
            account_cursor: self.account_cursor.clone(),
            launched: self.launched.clone(),
            cancelled: self.cancelled.clone(),
            next: self.next,
            slots: self.slots.iter().cloned().collect(),
            starts: self.starts.clone(),
            reruns: self.reruns.clone(),
            last_started: self.last_started.clone(),
        }
    }
    pub fn save(&self) -> Result<()> {
        anyhow::ensure!(self.error.is_none(), "scheduler state preserved for recovery");
        let bytes = serde_json::to_vec(&self.saved())?;
        anyhow::ensure!(bytes.len() <= 32 << 20, "scheduler state exceeds 32 MiB");
        if self.durable.borrow().as_deref() == Some(bytes.as_slice()) {
            return Ok(());
        }
        // A failed write leaves the file's contents unknown (the rename can
        // land before the directory sync fails), so nothing is cached until
        // this write succeeds and the next save after a failure writes again.
        *self.durable.borrow_mut() = None;
        // The renamed receipt must survive loss of the process or machine
        // before a network dispatch or native child can be admitted:
        // `write_atomic` syncs the file and then its directory.
        util::write_atomic(&self.state, &bytes)?;
        *self.durable.borrow_mut() = Some(bytes);
        Ok(())
    }
    pub fn descriptors(&self) -> Vec<(u64, RawFd)> {
        self.locks.iter().map(|(id, f)| (*id, f.as_raw_fd())).collect()
    }
    pub fn recover(&mut self, inherited: &[(u64, RawFd)]) {
        let mut adopted = std::collections::BTreeSet::new();
        for (id, fd) in inherited {
            let Some(slot) = self.slots.iter().find(|s| s.id == *id && s.granted_at.is_some()) else {
                continue;
            };
            let Some(number) = slot.number else { continue };
            let dir = slot
                .compatibility
                .clone()
                .unwrap_or_else(|| self.state.parent().unwrap().join("slots").join(&slot.pool));
            let expected = fs::symlink_metadata(dir.join(format!("slot-{number}.lock")));
            let actual = fs::metadata(format!("/proc/self/fd/{fd}"));
            let matches = match (expected, actual) {
                (Ok(a), Ok(b)) => {
                    a.is_file()
                        && !a.file_type().is_symlink()
                        && a.uid() == unsafe { libc::geteuid() }
                        && a.dev() == b.dev()
                        && a.ino() == b.ino()
                }
                _ => false,
            };
            if *fd > 2 && matches && adopted.insert(*fd) {
                if slot.owner.alive() && slot.granted_at.is_some() && !self.locks.contains_key(id) {
                    let _ = crate::session::set_cloexec(*fd, true);
                    self.locks.insert(*id, unsafe { File::from_raw_fd(*fd) });
                } else {
                    // A holder can die after serialization but before exec.
                    // This validated inherited descriptor still owns its flock.
                    unsafe {
                        libc::close(*fd);
                    }
                }
            }
        }
        let old = std::mem::take(&mut self.slots);
        for mut slot in old {
            slot.load_blocked_at = None;
            if !slot.owner.alive() {
                continue;
            }
            if slot.granted_at.is_some() && !self.lock(&mut slot) {
                slot.granted_at = None;
                slot.number = None;
            }
            self.slots.push_back(slot);
        }
        let _ = self.save();
    }
    pub fn restore(&mut self, saved: Saved, fds: &[(u64, RawFd)]) {
        self.locks.clear();
        self.slots = saved.slots.into();
        self.starts = saved.starts;
        self.reruns = saved.reruns;
        self.last_started = saved.last_started;
        self.cancelled = saved.cancelled;
        self.launched = saved.launched;
        self.account_cursor = saved.account_cursor;
        self.memory_events = saved.memory_events;
        self.memory_dismissed = saved.memory_dismissed;
        self.next = saved
            .next
            .max(
                self.slots
                    .iter()
                    .map(|s| s.id)
                    .chain(self.starts.iter().map(|s| s.id))
                    .chain(self.reruns.iter().map(|r| r.id))
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1),
            )
            .max(1);
        self.error = None;
        self.recover(fds);
    }
    pub fn snapshot(&self) -> Snapshot {
        let mut pools = self.settings.pools.clone();
        pools.entry("heavy".into()).or_insert_with(|| self.capacity("heavy"));
        Snapshot {
            paused: Vec::new(),
            error: self.error.clone(),
            memory_free: self.headroom.memory_free(),
            admitted: self.error.is_none() && self.headroom.admits(),
            headroom: self.headroom.clone(),
            pools,
            holders: self.slots.iter().filter(|s| s.granted_at.is_some()).cloned().collect(),
            waiters: self.slots.iter().filter(|s| s.granted_at.is_none()).cloned().collect(),
            starts: self
                .starts
                .iter()
                .cloned()
                .map(|mut item| {
                    item.prompt = None;
                    item
                })
                .collect(),
            reruns: self.reruns.clone(),
            repositories: self
                .settings
                .repositories
                .iter()
                .filter(|(_, p)| self.error.is_none() && p.is_dir())
                .map(|(k, p)| (k.clone(), p.to_string_lossy().into_owned()))
                .collect(),
        }
    }
    #[cfg(test)]
    pub fn dispatch_provider(&self, start: &Start, usage: &Usage, now: u64) -> (Tool, bool, Option<u64>) {
        let held =
            crate::service::provider_automation::quota_hold(start.provider, usage, self.settings.quota_percent, now);
        if held.is_some() {
            if let Some(other) = start.fallback {
                if crate::service::provider_automation::quota_hold(other, usage, self.settings.quota_percent, now)
                    .is_none()
                {
                    return (other, false, None);
                }
            }
        }
        (start.provider, held.is_some(), held.flatten())
    }
}
pub fn memory_key(session: u64, command: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, command.as_bytes());
    format!("{session}:{}", digest.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>())
}
/// Find a worktree and its canonical common directory without running Git.
pub(crate) fn git_location(path: &Path) -> Option<(PathBuf, PathBuf)> {
    for root in path.ancestors() {
        let marker = root.join(".git");
        let git = if marker.is_dir() {
            marker
        } else {
            if !fs::metadata(&marker).is_ok_and(|m| m.is_file() && m.len() <= 4096) {
                continue;
            }
            let text = fs::read_to_string(&marker).ok()?;
            let target = PathBuf::from(text.trim().strip_prefix("gitdir: ")?);
            if target.is_absolute() {
                target
            } else {
                root.join(target)
            }
        };
        let common = git.join("commondir");
        let common = if common.is_file() {
            if fs::metadata(&common).ok()?.len() > 4096 {
                return None;
            }
            let target = PathBuf::from(fs::read_to_string(common).ok()?.trim());
            if target.is_absolute() {
                target
            } else {
                git.join(target)
            }
        } else {
            git
        };
        return Some((root.to_path_buf(), common.canonicalize().ok()?));
    }
    None
}
/// Git's root commits identify the repository without retaining a remote URL.
pub fn repository(path: &Path) -> Option<(String, PathBuf)> {
    let output = crate::external::run_with_timeout(
        "git",
        &["-C", path.to_str()?, "rev-parse", "--show-toplevel"],
        std::time::Duration::from_secs(2),
    )?;
    let root = PathBuf::from(String::from_utf8(output).ok()?.trim());
    // A shallow clone's "roots" are its cut-off commits, which gave WSL an
    // identity no other host shared and kept it out of placement
    // (2026-11-06). Such a host is identified only by an explicit
    // `scheduler.repositories` mapping until its history is complete.
    let shallow = crate::external::run_with_timeout(
        "git",
        &["-C", root.to_str()?, "rev-parse", "--is-shallow-repository"],
        std::time::Duration::from_secs(2),
    )?;
    if String::from_utf8_lossy(&shallow).trim() != "false" {
        return None;
    }
    let output = crate::external::run_with_timeout(
        "git",
        &["-C", root.to_str()?, "rev-list", "--max-parents=0", "HEAD"],
        std::time::Duration::from_secs(2),
    )?;
    let mut roots: Vec<_> = String::from_utf8(output).ok()?.lines().map(str::to_string).collect();
    roots.sort();
    if roots.is_empty() {
        return None;
    }
    let hash = ring::digest::digest(&ring::digest::SHA256, roots.join("\n").as_bytes());
    let id = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>();
    Some((id, root))
}
#[cfg(test)]
#[path = "../tests/scheduler.rs"]
mod tests;
