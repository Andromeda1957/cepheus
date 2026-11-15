//! Private monotonic execution policy for protocol-14 queue intents.
//! A pause acknowledgement takes the same lock as the independent supervisor.
use super::*;
use std::os::fd::AsRawFd;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    pub nonce: String,
    pub binding: String,
    pub revision: u64,
    pub enabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Ack {
    #[serde(default)]
    pub executor: Option<String>,
    pub policy: Policy,
    pub outcome: String,
}
pub fn permitted(dir: &Path, intent: &Intent) -> Result<bool> {
    let Some(binding) = &intent.gate_binding else { return Ok(true) };
    Ok(store::optional::<Policy>(&dir.join("policy.json"))?
        .is_some_and(|p| p.nonce == intent.nonce && &p.binding == binding && p.enabled))
}
impl Jobs {
    pub(crate) fn policy(&mut self, key: &str, binding: &str, revision: u64, enabled: bool) -> Result<Ack> {
        let record = self.records.values().find(|r| r.intent.key == key).context("job intent not accepted yet")?;
        anyhow::ensure!(record.intent.gate_binding.as_deref() == Some(binding), "job policy binding changed");
        let dir = self.dir(record.info.id);
        let request = Policy { nonce: record.intent.nonce.clone(), binding: binding.into(), revision, enabled };
        let old = store::optional::<Policy>(&dir.join("policy.json"))?;
        if let Some(old) = &old {
            anyhow::ensure!(
                old.nonce == request.nonce && old.binding == request.binding,
                "job policy identity changed"
            );
            if old.revision > revision {
                return Ok(Ack { executor: None, policy: old.clone(), outcome: "superseded".into() });
            }
            anyhow::ensure!(old.revision != revision || old == &request, "job policy revision changed payload");
        }
        if old.as_ref() != Some(&request) {
            store::write(&dir.join("policy.json"), &request)?;
        }
        // An exact retry may follow rename success plus a failed parent fsync.
        // Reestablish publication before acknowledging; never rewrite unchanged JSON.
        #[cfg(test)]
        if self.fail_policy_barrier {
            self.fail_policy_barrier = false;
            anyhow::bail!("injected policy publication barrier failure");
        }
        std::fs::File::open(&dir)?.sync_all()?;
        // Persist the requested revision before trying the gate. If the body
        // won this race, it continues; a caller cannot claim an effective pause.
        let lock = store::append_log(&dir.join("run.lock"))?;
        let acquired = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !acquired {
            let error = std::io::Error::last_os_error();
            anyhow::ensure!(error.kind() == std::io::ErrorKind::WouldBlock, "job policy gate unavailable: {error}");
        }
        let launched = store::optional::<Launch>(&dir.join("launch.json"))?.is_some_and(|l| l.nonce == request.nonce);
        let outcome = if launched {
            "running"
        } else if acquired {
            if enabled {
                "enabled"
            } else {
                "paused"
            }
        } else {
            "pending"
        };
        Ok(Ack { executor: None, policy: request, outcome: outcome.into() })
    }
    pub(crate) fn policy_ack(&self, id: u64) -> Result<Option<Ack>> {
        let ack = store::optional::<Ack>(&self.dir(id).join("policy-acked.json"))?;
        Ok(ack.filter(|a| {
            a.policy.nonce == self.records[&id].intent.nonce
                && a.executor == self.records[&id].executor.as_ref().and_then(|e| e.host.clone())
        }))
    }
    pub(crate) fn save_policy_ack(&self, id: u64, mut ack: Ack) -> Result<()> {
        let record = &self.records[&id];
        anyhow::ensure!(
            record.intent.gate_binding.as_ref() == Some(&ack.policy.binding),
            "foreign job policy acknowledgement"
        );
        if let Some(old) = self.policy_ack(id)? {
            if old.policy.revision > ack.policy.revision {
                return Ok(());
            }
        }
        ack.policy.nonce = record.intent.nonce.clone();
        ack.executor = record.executor.as_ref().and_then(|e| e.host.clone());
        if self.policy_ack(id)?.as_ref() != Some(&ack) {
            store::write(&self.dir(id).join("policy-acked.json"), &ack)?;
        }
        Ok(())
    }
}
