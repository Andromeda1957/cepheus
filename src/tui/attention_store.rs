//! Private desktop recovery, partitioned by coordinator identity. No operation is replayed on load.
use super::*;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
};

const MAX_BYTES: u64 = 524288;
#[derive(Serialize, Deserialize)]
struct Recovery {
    version: u32,
    origin: String,
    drafts: BTreeMap<String, attention::Draft>,
    decisions: BTreeMap<String, attention::Decision>,
}
fn checked(file: File) -> Result<File> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0 && meta.nlink() == 1,
        "invalid private attention file"
    );
    Ok(file)
}
fn load(path: &Path, origin: &str) -> Result<Recovery> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => checked(file)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Recovery {
                version: 1,
                origin: origin.into(),
                drafts: BTreeMap::new(),
                decisions: BTreeMap::new(),
            })
        }
        Err(e) => return Err(e.into()),
    };
    ensure!(file.metadata()?.len() <= MAX_BYTES, "oversized attention recovery");
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_BYTES, "oversized attention recovery");
    let value: Recovery = serde_json::from_slice(&bytes)?;
    ensure!(
        value.version == 1 && value.origin == origin && value.drafts.len() + value.decisions.len() <= 128,
        "invalid attention recovery"
    );
    ensure!(
        value.drafts.iter().all(|(key, d)| key.len() <= 128 && d.text.len() <= 16000)
            && value
                .decisions
                .iter()
                .all(|(key, d)| key.len() <= 128 && serde_json::to_vec(&d.args).is_ok_and(|v| v.len() <= 20000)),
        "invalid attention operation"
    );
    Ok(value)
}
impl App {
    pub(super) fn use_attention_store(&mut self, root: PathBuf) {
        self.attention.store_root = Some(root);
        self.attention.origin = match &self.coordinator_target {
            Ok(Some(host)) => format!("peer:{}:{}", host.name, host.fingerprint),
            Ok(None) => format!("local:{}:{}", paths::socket_path().display(), paths::state_dir().display()),
            Err(_) => {
                self.attention.store_failed = true;
                return;
            }
        };
        let prior_namespace = self.attention.origin_namespaces.get(&self.attention.origin).copied();
        if let Err(_error) = self.restore_attention_store() {
            self.attention.store_failed = true;
            self.set_notice("Private attention recovery is unavailable; idle/decision submission is held. Existing state was preserved.",true);
        }
        // A failed disk save must not orphan a never-submitted composer in this process.
        // Rebind only the exact origin's retired namespace, without opening or sending anything.
        if let Some(prior) = prior_namespace {
            let namespace = self.attention.namespace;
            for draft in self.attention.drafts.values_mut().filter(|d| d.namespace == prior && !d.accepted) {
                draft.namespace = namespace;
                draft.request = None;
            }
            for decision in self.attention.decisions.values_mut().filter(|d| d.namespace == prior && !d.known) {
                decision.namespace = namespace;
            }
            let candidates: Vec<_> = self
                .attention
                .drafts
                .iter()
                .filter(|(_, d)| d.namespace == namespace && !d.accepted)
                .map(|(key, d)| (key.clone(), d.session, d.attempted))
                .collect();
            for (key, session, attempted) in candidates {
                let has_attempt = self
                    .attention
                    .draft_for
                    .get(&session)
                    .and_then(|key| self.attention.drafts.get(key))
                    .is_some_and(|d| d.attempted);
                if attempted || !has_attempt {
                    self.attention.draft_for.insert(session, key);
                }
            }
        }
    }
    fn attention_store_path(&self) -> Option<PathBuf> {
        self.attention.store_root.as_ref().map(|root| {
            root.join(format!("{}.json", crate::remote::auth::sha256_hex(self.attention.origin.as_bytes())))
        })
    }
    pub(super) fn restore_attention_store(&mut self) -> Result<()> {
        let Some(path) = self.attention_store_path() else { return Ok(()) };
        crate::job_sessions::store::directory(path.parent().unwrap())?;
        let state = load(&path, &self.attention.origin)?;
        for (key, mut draft) in state.drafts.into_iter().filter(|(_, d)| !d.accepted) {
            draft.namespace = self.attention.namespace;
            draft.request = None;
            if draft.attempted {
                draft.error =
                    Some("Recovered unresolved operation; retry the same immutable request to reconcile".into());
            }
            let previous_attempt = self
                .attention
                .draft_for
                .get(&draft.session)
                .and_then(|key| self.attention.drafts.get(key))
                .is_some_and(|d| d.attempted && !d.accepted);
            if draft.attempted || !previous_attempt {
                self.attention.draft_for.insert(draft.session, key.clone());
            }
            if let Some(old) = self.attention.drafts.get_mut(&key) {
                ensure!(
                    old.session == draft.session
                        && ((!old.attempted && !draft.attempted) || old.text == draft.text)
                        && old.conversation == draft.conversation
                        && old.pid == draft.pid
                        && old.spawned_ms == draft.spawned_ms
                        && old.host == draft.host,
                    "recovered idle identity changed"
                );
                old.attempted |= draft.attempted;
                if old.namespace != self.attention.namespace {
                    old.namespace = self.attention.namespace;
                    old.request = None;
                }
            } else {
                self.attention.drafts.insert(key, draft);
            }
        }
        for (key, mut decision) in state.decisions.into_iter().filter(|(_, d)| !d.known) {
            decision.namespace = self.attention.namespace;
            if let Some(old) = self.attention.decisions.get_mut(&key) {
                ensure!(old.args == decision.args, "recovered decision identity changed");
                old.namespace = self.attention.namespace;
            } else {
                self.attention.decisions.insert(key, decision);
            }
        }
        self.attention.store_failed = false;
        Ok(())
    }
    pub(super) fn persist_attention(&mut self) -> bool {
        if self.attention.store_failed {
            self.set_notice("Private attention recovery is unavailable; submission remains held.", true);
            return false;
        }
        let Some(path) = self.attention_store_path() else { return true };
        let save = (|| -> Result<()> {
            let root = path.parent().unwrap();
            crate::job_sessions::store::directory(root)?;
            let lock = checked(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                    .open(root.join("recovery.lock"))?,
            )?;
            ensure!(
                unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
                "attention recovery busy"
            );
            let mut state = load(&path, &self.attention.origin)?;
            // Merge definitive knowledge before checking stale unknown owners, independent of map iteration order.
            for (key, draft) in
                self.attention.drafts.iter().filter(|(_, d)| d.namespace == self.attention.namespace && d.accepted)
            {
                if let Some(old) = state.drafts.get_mut(key) {
                    ensure!(
                        old.session == draft.session
                            && old.text == draft.text
                            && old.conversation == draft.conversation
                            && old.pid == draft.pid
                            && old.spawned_ms == draft.spawned_ms
                            && old.host == draft.host,
                        "immutable receipt changed"
                    );
                    old.accepted = true;
                }
            }
            for (key, decision) in
                self.attention.decisions.iter().filter(|(_, d)| d.namespace == self.attention.namespace && d.known)
            {
                if let Some(old) = state.decisions.get_mut(key) {
                    ensure!(old.args == decision.args, "immutable decision receipt changed");
                    old.known = true;
                }
            }
            for (key, draft) in
                self.attention.drafts.iter().filter(|(_, d)| d.namespace == self.attention.namespace && !d.local_only)
            {
                if !draft.accepted && !state.drafts.contains_key(key) {
                    ensure!(
                        !state
                            .drafts
                            .iter()
                            .any(|(other, d)| other != key && d.session == draft.session && d.attempted && !d.accepted),
                        "another unresolved idle operation owns this target"
                    );
                }
                if let Some(old) = state.drafts.get(key) {
                    ensure!(
                        !old.attempted
                            || (old.session == draft.session
                                && old.text == draft.text
                                && old.conversation == draft.conversation
                                && old.pid == draft.pid
                                && old.spawned_ms == draft.spawned_ms
                                && old.host == draft.host),
                        "immutable attention message changed"
                    );
                }
                let mut value = draft.clone();
                if state.drafts.get(key).is_some_and(|d| d.accepted) {
                    value.accepted = true;
                }
                state.drafts.insert(key.clone(), value);
            }
            for (key, decision) in self
                .attention
                .decisions
                .iter()
                .filter(|(_, d)| d.namespace == self.attention.namespace && !d.local_only)
            {
                if !decision.known && !state.decisions.contains_key(key) {
                    ensure!(
                        !state.decisions.iter().any(|(other, d)| other != key
                            && !d.known
                            && d.args["repository"] == decision.args["repository"]
                            && d.args.get("host").and_then(serde_json::Value::as_str).unwrap_or("@here")
                                == decision.args.get("host").and_then(serde_json::Value::as_str).unwrap_or("@here")),
                        "another unresolved decision owns this repository"
                    );
                }
                if let Some(old) = state.decisions.get(key) {
                    ensure!(old.args == decision.args, "immutable decision changed");
                }
                let mut value = decision.clone();
                if state.decisions.get(key).is_some_and(|d| d.known) {
                    value.known = true;
                }
                state.decisions.insert(key.clone(), value);
            }
            state.drafts.retain(|_, d| !d.accepted);
            state.decisions.retain(|_, d| !d.known);
            ensure!(
                state.drafts.len() + state.decisions.len() <= 128
                    && serde_json::to_vec(&state)?.len() as u64 <= MAX_BYTES,
                "attention recovery capacity reached"
            );
            crate::job_sessions::store::write(&path, &state)?;
            Ok(())
        })();
        if save.is_err() {
            self.set_notice(
                "Could not save immutable attention recovery; submission held and existing recovery preserved.",
                true,
            );
            false
        } else {
            true
        }
    }
}
