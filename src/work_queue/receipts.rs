//! Compact completion/launch tombstones survive owner removal of a terminal.
use super::*;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct Receipt {
    pub key: String,
    pub fingerprint: Option<String>,
    pub session: u64,
    pub completed_turns: u64,
    pub saved_at: u64,
}
// Legacy launch keys are queue-<UUID>. Only the complete new timestamped
// shape can expire: an all-numeric legacy UUID prefix is still an identity.
fn timestamped_key_time(key: &str) -> Option<u64> {
    let (time, uuid) = key.strip_prefix("queue-")?.split_once('-')?;
    let bytes = uuid.as_bytes();
    if bytes.len() != 36
        || !bytes.iter().enumerate().all(
            |(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    *b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            },
        )
    {
        return None;
    }
    time.parse().ok()
}
impl Service {
    fn work_receipt_path(&self, key: &str) -> Result<PathBuf> {
        let root = self.work_queue.as_ref().context("work receipt storage unavailable")?.root.join("turn-receipts");
        crate::job_sessions::store::directory(&root)?;
        Ok(root.join(format!("{}.json", crate::remote::auth::sha256_hex(key.as_bytes()))))
    }
    pub(super) fn work_retired_receipt(&self, key: &str) -> Result<Option<Receipt>> {
        let path = self.work_receipt_path(key)?;
        let receipt = crate::job_sessions::store::optional::<Receipt>(&path)?;
        anyhow::ensure!(receipt.as_ref().is_none_or(|r| r.key == key), "work receipt identity changed");
        Ok(receipt)
    }
    pub(super) fn work_validate_new_key(&self, key: &str) -> Result<()> {
        if let Some(time) = timestamped_key_time(key) {
            anyhow::ensure!(
                util::now_secs().saturating_sub(time) <= 30 * 86400,
                "work launch receipt expired; outcome unknown, submit a new item only after inspecting prior custody"
            );
        }
        Ok(())
    }
    pub(super) fn work_save_turn_receipts(&self, id: u64) -> Result<()> {
        let Some(session) = self.sessions.get(&id) else { return Ok(()) };
        let mut bindings: Vec<(String, Option<String>)> = self
            .registry
            .sessions
            .iter()
            .filter(|e| e.id == id)
            .filter_map(|e| e.launch_key.clone().map(|k| (k, e.launch_fingerprint.clone())))
            .collect();
        bindings.extend(self.work_idle_bindings(id)?.into_iter().map(|(k, f)| (k, Some(f))));
        for (key, fingerprint) in bindings {
            let path = self.work_receipt_path(&key)?;
            if !path.exists() {
                anyhow::ensure!(
                    std::fs::read_dir(path.parent().unwrap())?.count() < 100000,
                    "work receipt safety capacity reached; retain session custody for inspection"
                );
            }
            let old = self.work_retired_receipt(&key)?;
            let completed_turns =
                session.tracker.completed_turns.max(old.as_ref().map(|r| r.completed_turns).unwrap_or(0));
            let receipt = Receipt { key, fingerprint, session: id, completed_turns, saved_at: util::now_secs() };
            crate::job_sessions::store::write(&path, &receipt)?;
        }
        Ok(())
    }
    pub(super) fn work_turn_receipt(&self, key: &str) -> Result<Value> {
        if let Some(r) = self.work_retired_receipt(key)? {
            return Ok(serde_json::json!({"session":r.session,"completed_turns":r.completed_turns}));
        }
        let id = self
            .registry
            .sessions
            .iter()
            .find(|e| e.launch_key.as_deref() == Some(key))
            .map(|e| e.id)
            .or(self.work_idle_session(key)?)
            .context("work receipt unavailable; outcome unknown")?;
        let completed_turns = self
            .sessions
            .get(&id)
            .map(|s| s.tracker.completed_turns)
            .context("session custody unavailable; outcome unknown")?;
        Ok(serde_json::json!({"session":id,"completed_turns":completed_turns}))
    }
    pub(super) fn work_receipts_gc(&self, now: u64) -> Result<()> {
        let Some(q) = &self.work_queue else { return Ok(()) };
        let dir = q.root.join("turn-receipts");
        if !dir.exists() {
            return Ok(());
        }
        let mut changed = false;
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let r: Receipt = crate::job_sessions::store::read(&path)?;
            if timestamped_key_time(&r.key).is_some() && now.saturating_sub(r.saved_at) > 30 * 86400 {
                std::fs::remove_file(path)?;
                changed = true;
            }
        }
        if changed {
            std::fs::File::open(&dir)?.sync_all()?;
        }
        Ok(())
    }
}
