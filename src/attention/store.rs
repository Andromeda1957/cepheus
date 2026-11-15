//! Private durable attention state. Only metadata projections leave this module.
use super::*;
use crate::job_sessions::store as fs;
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Every published record must remain readable by the existing private-file reader.
pub const RECEIPT_LIMIT: usize = 524288;
fn bounded<T: serde::Serialize + ?Sized>(value: &T) -> Result<()> {
    anyhow::ensure!(serde_json::to_vec(value)?.len() <= RECEIPT_LIMIT, "attention receipt capacity exceeded");
    Ok(())
}
pub(crate) fn publish<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    bounded(value)?;
    fs::write(path, value)
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub session: u64,
    pub spawned_ms: u64,
    pub conversation: Option<String>,
    pub pid: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub key: String,
    pub info: MessageInfo,
    pub text: String,
    pub target: Target,
    #[serde(default)]
    pub attempted: bool,
    #[serde(default)]
    pub sequence: u64,
    #[serde(skip)]
    pub durable: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub created_at: u64,
    pub recipients: Vec<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub key: String,
    pub repository: PathBuf,
    pub request_repository: String,
    pub requested: Option<Vec<u64>>,
    pub text: String,
    pub info: Decision,
    pub targets: Vec<Target>,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub peers: Vec<DecisionPeer>,
    #[serde(skip)]
    pub durable: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionPeer {
    pub binding: Value,
    pub targets: Option<Vec<Target>>,
    #[serde(default)]
    pub accepted: std::collections::BTreeSet<u64>,
}
/// The fields of the removed stall, away and digest state in a state.json
/// written before 2026-11-08 are ignored and dropped on the next save.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub remote_off: BTreeMap<String, u64>,
    #[serde(default)]
    pub namespace: String,
    pub supervisor: SupervisorInfo,
    pub rules: Vec<Value>,
}
pub struct Store {
    pub root: PathBuf,
    pub saved: Saved,
    pub messages: BTreeMap<String, Message>,
    pub decisions: BTreeMap<String, DecisionRecord>,
    persisted: Saved,
}
impl Store {
    pub fn load(state: &Path) -> Result<Self> {
        let root = state.join("attention");
        fs::directory(&root)?;
        fs::directory(&root.join("messages"))?;
        fs::directory(&root.join("decisions"))?;
        fs::directory(&root.join("retired"))?;
        let mut saved = fs::optional::<Saved>(&root.join("state.json"))?.unwrap_or_default();
        if saved.namespace.is_empty() {
            saved.namespace = crate::util::uuid_v4()?;
            publish(&root.join("state.json"), &saved)?;
        }
        let mut messages = BTreeMap::new();
        for entry in std::fs::read_dir(root.join("messages"))? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let mut message: Message = fs::read(&path)?;
            anyhow::ensure!(
                valid_id(&message.info.id) && path.file_stem().and_then(|s| s.to_str()) == Some(&message.info.id),
                "invalid message identity"
            );
            if message.attempted
                && !matches!(
                    message.info.state,
                    DeliveryState::Submitted | DeliveryState::Unknown | DeliveryState::Canceled
                )
            {
                message.info.state = DeliveryState::Unknown;
                message.info.reason = "delivery interrupted; inspect preserved composer".into();
                publish(&path, &message)?;
            }
            message.durable = true;
            anyhow::ensure!(messages.len() < 10000, "attention message recovery capacity exceeded");
            anyhow::ensure!(messages.insert(message.info.id.clone(), message).is_none(), "duplicate attention message");
        }
        let mut decisions = BTreeMap::new();
        for entry in std::fs::read_dir(root.join("decisions"))? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let mut record: DecisionRecord = fs::read(&path)?;
            record.durable = true;
            anyhow::ensure!(valid_id(&record.info.id), "invalid decision identity");
            anyhow::ensure!(decisions.insert(record.key.clone(), record).is_none(), "duplicate decision key");
        }
        std::fs::File::open(root.join("messages"))?.sync_all()?;
        std::fs::File::open(root.join("decisions"))?.sync_all()?;
        std::fs::File::open(&root)?.sync_all()?;
        Ok(Self { root, saved: saved.clone(), persisted: saved, messages, decisions })
    }
    pub fn supervisor_durable(&self) -> bool {
        self.saved.supervisor == self.persisted.supervisor && self.saved.rules == self.persisted.rules
    }
    pub fn save(&mut self) -> Result<()> {
        if self.saved != self.persisted {
            publish(&self.root.join("state.json"), &self.saved)?;
            self.persisted = self.saved.clone();
        }
        Ok(())
    }
    pub fn add(&mut self, key: &str, text: &str, target: Target, now: u64) -> Result<MessageInfo> {
        anyhow::ensure!(
            !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control),
            "invalid message key"
        );
        anyhow::ensure!(!text.trim().is_empty() && text.len() <= 16000 && !text.contains('\0'), "invalid message text");
        if let Some(prior) = self.messages.values().find(|m| m.key == key).cloned() {
            anyhow::ensure!(
                prior.text == text && prior.target.session == target.session,
                "message key belongs to another payload"
            );
            if !prior.durable {
                publish(&self.root.join("messages").join(format!("{}.json", prior.info.id)), &prior)?;
                self.messages.get_mut(&prior.info.id).unwrap().durable = true;
            }
            return Ok(prior.info.clone());
        }
        anyhow::ensure!(
            !self.root.join("retired").join(crate::remote::auth::sha256_hex(key.as_bytes())).exists(),
            "message key expired; inspect prior delivery"
        );
        anyhow::ensure!(self.messages.len() < 10000, "attention queue full");
        let hash = crate::remote::auth::sha256_hex(format!("{}:{key}", self.saved.namespace).as_bytes());
        let id = format!("{}-{}-{}-{}-{}", &hash[..8], &hash[8..12], &hash[12..16], &hash[16..20], &hash[20..32]);
        let path = self.root.join("messages").join(format!("{id}.json"));
        if let Some(mut prior) = fs::optional::<Message>(&path)? {
            anyhow::ensure!(
                prior.key == key && prior.text == text && prior.target.session == target.session,
                "message key belongs to another payload"
            );
            publish(&path, &prior)?;
            prior.durable = true;
            let info = prior.info.clone();
            self.messages.insert(id, prior);
            return Ok(info);
        }
        let info = MessageInfo {
            host: None,
            id: id.clone(),
            session: target.session,
            state: DeliveryState::Queued,
            created_at: now,
            updated_at: now,
            reason: String::new(),
        };
        let sequence = self
            .messages
            .values()
            .map(|m| m.sequence)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .context("attention sequence exhausted")?;
        let message = Message {
            key: key.into(),
            text: text.into(),
            target,
            info: info.clone(),
            attempted: false,
            sequence,
            durable: true,
        };
        let path = self.root.join("messages").join(format!("{id}.json"));
        match publish(&path, &message) {
            Ok(()) => {
                self.messages.insert(id, message);
            }
            Err(error) => {
                // A pre-rename error leaves no runnable RAM intent. An ambiguous
                // publication is retained but held until explicit same-key repair.
                if let Ok(Some(mut published)) = fs::optional::<Message>(&path) {
                    published.durable = false;
                    self.messages.insert(id, published);
                }
                return Err(error);
            }
        }
        Ok(info)
    }
    pub fn update(&mut self, id: &str, state: DeliveryState, reason: &str, now: u64) -> Result<()> {
        let m = self.messages.get_mut(id).context("unknown attention message")?;
        if m.info.state != state || m.info.reason != reason {
            let mut next = m.clone();
            next.info.state = state;
            next.info.reason = reason.into();
            next.info.updated_at = now;
            next.attempted |= matches!(
                state,
                DeliveryState::Pasted | DeliveryState::Submitted | DeliveryState::Unknown | DeliveryState::Canceled
            );
            publish(&self.root.join("messages").join(format!("{id}.json")), &next)?;
            *m = next;
        }
        Ok(())
    }
    pub fn begin(&mut self, id: &str, now: u64) -> Result<()> {
        let m = self.messages.get(id).context("unknown attention message")?;
        anyhow::ensure!(m.durable, "message publication is not durably confirmed");
        let mut next = m.clone();
        next.attempted = true;
        next.info.updated_at = now;
        next.info.reason = "delivery prepared; admission pending".into();
        publish(&self.root.join("messages").join(format!("{id}.json")), &next)?;
        self.messages.insert(id.into(), next);
        Ok(())
    }
    pub fn gc(&mut self, now: u64, days: u64) -> Result<()> {
        let expired: Vec<_> = self
            .messages
            .values()
            .filter(|m| {
                matches!(m.info.state, DeliveryState::Submitted | DeliveryState::Canceled)
                    && now.saturating_sub(m.info.updated_at) > days.saturating_mul(86400)
            })
            .map(|m| (m.info.id.clone(), crate::remote::auth::sha256_hex(m.key.as_bytes())))
            .collect();
        for (id, key) in expired {
            // Keep a compact retry fence before removing private payload.
            publish(&self.root.join("retired").join(key), &now)?;
            std::fs::remove_file(self.root.join("messages").join(format!("{id}.json")))?;
            self.messages.remove(&id);
        }
        std::fs::File::open(self.root.join("messages"))?.sync_all()?;
        let expired: Vec<_> = self
            .decisions
            .values()
            .filter(|d| d.completed && now.saturating_sub(d.info.created_at) > days.saturating_mul(86400))
            .take(100)
            .map(|d| d.key.clone())
            .collect();
        for key in expired {
            let hash = crate::remote::auth::sha256_hex(key.as_bytes());
            publish(&self.root.join("retired").join(format!("decision-{hash}")), &now)?;
            std::fs::remove_file(self.root.join("decisions").join(format!("{hash}.json")))?;
            self.decisions.remove(&key);
        }
        std::fs::File::open(self.root.join("decisions"))?.sync_all()?;
        Ok(())
    }
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id
            .bytes()
            .enumerate()
            .all(|(i, b)| if [8, 13, 18, 23].contains(&i) { b == b'-' } else { b.is_ascii_hexdigit() })
}

#[cfg(test)]
#[path = "../tests/attention/store.rs"]
mod tests;
