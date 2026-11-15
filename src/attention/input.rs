//! One durable admission journal for work-queue idle chores and queued messages.
use super::*;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct Receipt {
    pub key: String,
    pub fingerprint: String,
    pub session: u64,
    pub turn_anchor: u64,
    pub delivered: bool,
    #[serde(default)]
    pub inputs: Option<u64>,
    #[serde(default)]
    pub spawned_ms: Option<u64>,
    #[serde(default)]
    pub conversation: Option<String>,
    #[serde(default)]
    pub attention_id: Option<String>,
    #[serde(default)]
    pub expected_text: Option<String>,
}
impl Service {
    pub(super) fn guarded_receipt_root(&self) -> Result<PathBuf> {
        let root = self.opts.state_dir.join("guarded-input");
        crate::job_sessions::store::directory(&root)?;
        Ok(root)
    }
    pub(super) fn guarded_receipt_path(&self, key: &str) -> Result<PathBuf> {
        // Preserve exact historical queue receipts through migration/handover.
        if let Some(q) = &self.work_queue {
            let old = q.root.join(format!("idle-{:010x}.json", crate::work_queue::store::identity(key)));
            if old.exists() {
                return Ok(old);
            }
        }
        Ok(self.guarded_receipt_root()?.join(format!("{}.json", crate::remote::auth::sha256_hex(key.as_bytes()))))
    }
    /// Tracked state only: the message goes through the provider's own
    /// interface, so the screen neither authorizes nor receives it.
    pub(super) fn guarded_idle_ready(&self, id: u64) -> bool {
        !self.automatic_input_pending(id) && self.sessions.get(&id).is_some_and(|s| s.native_idle(util::now_ms()))
    }
    pub(super) fn guarded_idle_start(&mut self, mut receipt: Receipt, text: &str) -> Result<()> {
        anyhow::ensure!(self.guarded_idle_ready(receipt.session), "session is not safely idle");
        let s = self.sessions.get(&receipt.session).context("session unavailable")?;
        anyhow::ensure!(
            receipt.spawned_ms == Some(s.spawned_ms) && receipt.conversation == s.conversation,
            "target conversation or process changed"
        );
        let wrapped = crate::keys::non_owner_message(text);
        receipt.expected_text = Some(wrapped.clone());
        crate::attention::store::publish(&self.guarded_receipt_path(&receipt.key)?, &receipt)?;
        self.begin_messages(0, &[receipt.session], &wrapped, false, None, Some(receipt));
        Ok(())
    }
    pub(super) fn guarded_idle_admitted(&mut self, intent: &Receipt, inputs: u64) -> Result<()> {
        let path = self.guarded_receipt_path(&intent.key)?;
        let mut receipt: Receipt = crate::job_sessions::store::read(&path)?;
        anyhow::ensure!(
            receipt.key == intent.key
                && receipt.fingerprint == intent.fingerprint
                && receipt.session == intent.session
                && receipt.turn_anchor == intent.turn_anchor
                && receipt.spawned_ms == intent.spawned_ms
                && receipt.conversation == intent.conversation,
            "idle receipt identity changed"
        );
        anyhow::ensure!(
            self.sessions.get(&receipt.session).is_some_and(|s| s.inputs == inputs
                && receipt.spawned_ms == Some(s.spawned_ms)
                && receipt.conversation == s.conversation),
            "idle admission custody changed; outcome unknown"
        );
        receipt.inputs = Some(inputs);
        receipt.delivered = true;
        crate::attention::store::publish(&path, &receipt)?;
        if let Some(id) = &receipt.attention_id {
            if let Some(attention) = &mut self.attention {
                attention.store.update(id, crate::attention::DeliveryState::Submitted, "", util::now_secs())?;
            }
        }
        Ok(())
    }
    pub(super) fn guarded_idle_failed(&mut self, intent: &Receipt) {
        if let Some(id) = &intent.attention_id {
            if let Some(a) = &mut self.attention {
                let _ = a.store.update(
                    id,
                    crate::attention::DeliveryState::Unknown,
                    "admission uncertain; inspect original target",
                    util::now_secs(),
                );
            }
        }
    }
}
