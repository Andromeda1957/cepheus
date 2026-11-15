//! Idle chores use the normal native message delivery, with a durable receipt.
pub(super) use super::guarded_input::Receipt;
use super::*;
use crate::work_queue as w;
impl Service {
    pub(super) fn work_idle_bindings(&self, id: u64) -> Result<Vec<(String, String)>> {
        let Some(q) = &self.work_queue else {
            return Ok(Vec::new());
        };
        let mut bindings = Vec::new();
        for entry in std::fs::read_dir(&q.root)?.chain(std::fs::read_dir(self.guarded_receipt_root()?)?) {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("idle-")
                || entry.path().parent() == Some(self.guarded_receipt_root()?.as_path())
            {
                let receipt: Receipt = crate::job_sessions::store::read(&entry.path())?;
                if receipt.delivered
                    && receipt.session == id
                    && self.sessions.get(&id).is_some_and(|s| {
                        receipt.inputs == Some(s.inputs)
                            && receipt.spawned_ms == Some(s.spawned_ms)
                            && receipt.conversation == s.conversation
                    })
                {
                    bindings.push((receipt.key, receipt.fingerprint));
                }
            }
        }
        Ok(bindings)
    }
    pub(super) fn work_idle_session(&self, key: &str) -> Result<Option<u64>> {
        let Some(_) = &self.work_queue else {
            return Ok(None);
        };
        let path = self.guarded_receipt_path(key)?;
        let receipt = crate::job_sessions::store::optional::<Receipt>(&path)?;
        anyhow::ensure!(receipt.as_ref().is_none_or(|r| r.key == key), "idle receipt identity changed");
        let Some(receipt) = receipt.filter(|r| r.delivered) else {
            return Ok(None);
        };
        anyhow::ensure!(
            self.sessions.get(&receipt.session).is_some_and(|s| {
                receipt.inputs == Some(s.inputs)
                    && receipt.spawned_ms == Some(s.spawned_ms)
                    && receipt.conversation == s.conversation
            }),
            "idle turn custody changed; completion outcome unknown"
        );
        Ok(Some(receipt.session))
    }
    pub(super) fn work_idle_start(&mut self, spec: &w::Spec, queue_id: Option<u64>) -> Result<Value> {
        spec.validate()?;
        anyhow::ensure!(
            spec.kind == "session" && spec.guard.is_empty(),
            "idle chore requires an unguarded interactive prompt"
        );
        self.work_queue.as_ref().context("queue unavailable")?;
        let path = self.guarded_receipt_path(&spec.key)?;
        let fingerprint = crate::remote::auth::sha256_hex(&serde_json::to_vec(spec)?);
        if let Some(prior) = crate::job_sessions::store::optional::<Receipt>(&path)? {
            anyhow::ensure!(
                prior.key == spec.key && prior.fingerprint == fingerprint,
                "idle delivery key belongs to another payload"
            );
            anyhow::ensure!(prior.delivered, "idle delivery outcome unknown; inspect the session");
            return Ok(serde_json::json!({"session":prior.session,"turn_anchor":prior.turn_anchor}));
        }
        self.work_validate_new_key(&spec.key)?;
        let now = util::now_ms();
        let cwd = spec.cwd.canonicalize()?;
        let pending: std::collections::BTreeSet<_> =
            self.sessions.keys().filter(|id| self.automatic_input_pending(**id)).copied().collect();
        let start = self.opts.start_dir.clone();
        let (id, turn_anchor, spawned_ms, conversation) = self
            .sessions
            .values()
            .find_map(|s| {
                (!pending.contains(&s.id)
                    && Some(s.tool) == spec.provider
                    && s.native_idle(now)
                    && s.cwd.as_deref().unwrap_or(&start).canonicalize().is_ok_and(|path| path == cwd))
                .then_some((s.id, s.tracker.completed_turns, s.spawned_ms, s.conversation.clone()))
            })
            .context("no safely idle session in the requested directory")?;
        if let Some(queue_id) = queue_id {
            self.work_queue.as_mut().unwrap().begin_attempt(queue_id)?;
        }
        let receipt = Receipt {
            key: spec.key.clone(),
            fingerprint,
            session: id,
            turn_anchor,
            delivered: false,
            inputs: None,
            spawned_ms: Some(spawned_ms),
            conversation,
            attention_id: None,
            expected_text: None,
        };
        self.guarded_idle_start(receipt, spec.prompt.as_deref().context("idle prompt required")?)?;
        // Native delivery is asynchronous. Reconcile this same key after the
        // provider's receipt; a restart or uncertain result must never
        // trigger a second send.
        bail!("idle delivery awaits its admission receipt; outcome unknown until confirmed")
    }
}
