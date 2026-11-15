//! Service-owned attention and private durable message queue.
use super::*;
use crate::attention::{self as a, DeliveryState};
#[derive(Clone)]
pub(super) struct DecisionRpc {
    pub key: String,
    pub peer: usize,
    pub session: Option<u64>,
    pub at: Instant,
}
pub(super) struct State {
    pub native_pending: HashSet<u64>,
    pub decision_rpc: HashMap<u64, DecisionRpc>,
    pub store: a::store::Store,
    pub rpc_last: Option<Instant>,
    pub rpc_serial: u64,
    pub pending_rpc: HashMap<u64, String>,
    pub authority: Arc<Mutex<u64>>,
    pub last_tick: Option<Instant>,
    pub last_gc: u64,
    pub delivery_cursor: usize,
    pub decision_cursor: usize,
}
impl State {
    pub fn load(path: &Path) -> Result<Self> {
        let store = a::store::Store::load(path)?;
        let authority = Arc::new(Mutex::new(store.saved.supervisor.revision));
        Ok(Self {
            native_pending: Default::default(),
            decision_rpc: Default::default(),
            authority,
            rpc_last: None,
            rpc_serial: 0,
            pending_rpc: Default::default(),
            last_tick: None,
            last_gc: 0,
            delivery_cursor: 0,
            decision_cursor: 0,
            store,
        })
    }
}
impl Service {
    pub(super) fn attention_snapshot(&self) -> a::Snapshot {
        let Some(a) = &self.attention else { return Default::default() };
        let mut queued = BTreeMap::new();
        for m in a
            .store
            .messages
            .values()
            .filter(|m| matches!(m.info.state, DeliveryState::Queued | DeliveryState::Held | DeliveryState::Pasted))
        {
            *queued.entry(m.info.session).or_insert(0) += 1;
        }
        let mut supervisor = a.store.saved.supervisor.clone();
        if !a.store.supervisor_durable() {
            supervisor.enabled = false;
            if !supervisor.pending.iter().any(|p| p == "local persistence") {
                supervisor.pending.push("local persistence".into());
            }
        }
        a::Snapshot { queued, messages: a.store.messages.values().map(|m| m.info.clone()).collect(), supervisor }
    }
    pub(super) fn attention_command(&mut self, action: &str, args: &Value) -> Result<Value> {
        anyhow::ensure!(self.attention.is_some(), "attention recovery unavailable");
        match action {
            "attention-status" | "attention-list" => {
                Ok(serde_json::json!({"attention":self.attention_fleet_snapshot()}))
            }
            "attention-send" => {
                let id = args["session"].as_u64().context("target session required")?;
                let key = args["key"].as_str().context("stable message key required")?;
                let text = args["text"].as_str().context("message text required")?;
                // Exact-key lookup precedes current target lookup: a reopen cannot
                // redirect a retry or turn unknown admission into another send.
                if let Some(prior) = self.attention.as_ref().unwrap().store.messages.values().find(|m| m.key == key) {
                    anyhow::ensure!(
                        prior.info.session == id && prior.text == text,
                        "message key belongs to another payload"
                    );
                    let target = prior.target.clone();
                    let info = self.attention.as_mut().unwrap().store.add(key, text, target, util::now_secs())?;
                    return Ok(serde_json::json!({"message":info}));
                }
                let s = self
                    .sessions
                    .get(&id)
                    .filter(|s| s.alive && s.tool.is_agent())
                    .context("live agent target required")?;
                let target = a::store::Target {
                    session: id,
                    spawned_ms: s.spawned_ms,
                    conversation: s.conversation.clone(),
                    pid: s.hosted.as_ref().and_then(|h| h.pid),
                };
                let expected = args
                    .get("expected_target")
                    .context("Update needed: captured target identity required (protocol 17)")?;
                anyhow::ensure!(
                    expected.as_object().is_some_and(|v| v.contains_key("spawned_ms")
                        && v.contains_key("pid")
                        && v.contains_key("conversation")),
                    "complete captured target identity required"
                );
                let expected: a::ExpectedTarget = serde_json::from_value(expected.clone())
                    .map_err(|_| anyhow::anyhow!("captured target identity invalid"))?;
                anyhow::ensure!(expected.matches(&target), "captured target process or conversation changed");
                if let Some(expected) = args.get("target") {
                    let expected: a::store::Target = serde_json::from_value(expected.clone())?;
                    anyhow::ensure!(expected == target, "target process or conversation changed");
                    let repository = args["repository"].as_str().context("registered repository required")?;
                    let root = self
                        .opts
                        .config
                        .scheduler
                        .repositories
                        .get(repository)
                        .context("repository is not registered")?;
                    anyhow::ensure!(
                        self.attention_repository_targets(root)?.contains(&target),
                        "target repository changed"
                    );
                }
                let info = self.attention.as_mut().unwrap().store.add(key, text, target, util::now_secs())?;
                self.state_changed();
                Ok(serde_json::json!({"message":info}))
            }
            "attention-recipients" => {
                let repository = args["repository"].as_str().context("registered repository required")?;
                let root = self
                    .opts
                    .config
                    .scheduler
                    .repositories
                    .get(repository)
                    .context("repository is not registered")?
                    .clone();
                let targets = self.attention_repository_targets(&root)?;
                Ok(serde_json::json!({"repository":repository,"targets":targets}))
            }
            "attention-message" => {
                let id = args["id"].as_str().context("message id required")?;
                let m = self.attention.as_ref().unwrap().store.messages.get(id).context("unknown attention message")?;
                Ok(serde_json::json!({"message":m.info,"text":m.text}))
            }
            "attention-cancel" => {
                let id = args["id"].as_str().context("message id required")?;
                let m = self.attention.as_ref().unwrap().store.messages.get(id).context("unknown attention message")?;
                anyhow::ensure!(
                    m.durable && !m.attempted && matches!(m.info.state, DeliveryState::Queued | DeliveryState::Held),
                    "delivery already attempted; inspect original target"
                );
                self.attention.as_mut().unwrap().store.update(
                    id,
                    DeliveryState::Canceled,
                    "canceled before delivery",
                    util::now_secs(),
                )?;
                self.state_changed();
                Ok(serde_json::json!({"message":self.attention.as_ref().unwrap().store.messages[id].info}))
            }
            "attention-decide" => self.attention_decide(args),
            "attention-supervisor-status" => Ok(serde_json::json!({"supervisor":self.attention_snapshot().supervisor})),
            "attention-supervisor-set" => {
                let enabled = args["enabled"].as_bool().context("enabled boolean required")?;
                if let Some(origin) = args["origin"].as_str() {
                    anyhow::ensure!(
                        !enabled && !origin.is_empty() && origin.len() <= 128 && !origin.chars().any(char::is_control),
                        "invalid supervisor off authority"
                    );
                    let generation = args["generation"].as_u64().context("off generation required")?;
                    let previous = self.attention.as_ref().unwrap().store.saved.remote_off.get(origin).copied();
                    anyhow::ensure!(previous.is_none_or(|n| generation >= n), "stale supervisor off generation");
                    if previous != Some(generation) {
                        anyhow::ensure!(
                            previous.is_some() || self.attention.as_ref().unwrap().store.saved.remote_off.len() < 128,
                            "supervisor authority capacity reached"
                        );
                        self.attention_set_supervisor(false)?;
                        let store = &mut self.attention.as_mut().unwrap().store;
                        store.saved.remote_off.insert(origin.into(), generation);
                        store.save()?;
                    }
                    return Ok(
                        serde_json::json!({"supervisor":self.attention_snapshot().supervisor,"off":{"origin":origin,"generation":generation}}),
                    );
                }
                self.attention_set_supervisor(enabled)?;
                Ok(serde_json::json!({"supervisor":self.attention_snapshot().supervisor}))
            }
            "attention-supervisor-rules" => {
                if let Some(rules) = args.get("rules") {
                    let rules = rules.as_array().context("rules array required")?;
                    anyhow::ensure!(rules.len() <= 32, "too many supervisor rules");
                    self.attention_validate_rules(rules)?;
                    let authority = self.attention.as_ref().unwrap().authority.clone();
                    let store = &mut self.attention.as_mut().unwrap().store;
                    store.saved.rules = rules.clone();
                    store.saved.supervisor.rules = rules.len();
                    store.saved.supervisor.revision =
                        store.saved.supervisor.revision.checked_add(1).context("supervisor generation exhausted")?;
                    *authority.lock().unwrap_or_else(|e| e.into_inner()) = store.saved.supervisor.revision;
                    store.save()?;
                    self.attention_revoke_prepared();
                    self.state_changed();
                }
                Ok(
                    serde_json::json!({"rules":self.attention.as_ref().unwrap().store.saved.rules,"supervisor":self.attention_snapshot().supervisor}),
                )
            }
            "attention-supervisor-off" => {
                self.attention_set_supervisor(false)?;
                self.attention_fleet_off()?;
                Ok(serde_json::json!({"supervisor":self.attention_snapshot().supervisor}))
            }
            _ => anyhow::bail!("unknown attention operation"),
        }
    }
    fn attention_set_supervisor(&mut self, enabled: bool) -> Result<()> {
        let authority = self.attention.as_ref().context("attention unavailable")?.authority.clone();
        let store = &mut self.attention.as_mut().context("attention unavailable")?.store;
        store.saved.supervisor.enabled = enabled;
        store.saved.supervisor.revision =
            store.saved.supervisor.revision.checked_add(1).context("supervisor generation exhausted")?;
        *authority.lock().unwrap_or_else(|e| e.into_inner()) = store.saved.supervisor.revision;
        store.save()?;
        self.attention_revoke_prepared();
        self.state_changed();
        Ok(())
    }
    pub(super) fn attention_tick(&mut self) {
        let Some(a) = &mut self.attention else { return };
        let now = util::now_secs();
        if a.last_tick.is_some_and(|t| t.elapsed() < Duration::from_millis(200)) {
            return;
        }
        a.last_tick = Some(Instant::now());
        if now.saturating_sub(a.last_gc) > 3600 {
            if a.store.gc(now, self.opts.config.attention.retention_days).is_ok() {
                a.last_gc = now;
            }
        }
        let _ = a.store.save();
        let pending: Vec<_> = a.store.decisions.values().filter(|d| d.durable && !d.completed).cloned().collect();
        let len = pending.len();
        let offset = if len == 0 { 0 } else { a.decision_cursor % len };
        let decisions: Vec<_> = pending.into_iter().cycle().skip(offset).take(len.min(10)).collect();
        a.decision_cursor = if len == 0 { 0 } else { (offset + 10) % len };
        for d in decisions {
            let _ = self.attention_complete_decision(&d);
        }
        self.attention_deliver_queued();
        self.attention_sync_fleet_off();
    }
    fn attention_deliver_queued(&mut self) {
        if self.shutdown_at.is_some() || self.updating.is_some() {
            return;
        }
        let a = self.attention.as_mut().unwrap();
        let mut pending: Vec<_> = a
            .store
            .messages
            .values()
            .filter(|m| {
                m.durable && !m.attempted && matches!(m.info.state, DeliveryState::Queued | DeliveryState::Held)
            })
            .cloned()
            .collect();
        pending.sort_by_key(|m| (m.sequence, m.info.created_at, m.info.id.clone()));
        // Rotate the bounded scan, while selecting only the oldest outstanding
        // message of each session. Held sessions cannot starve another target.
        let mut first = std::collections::HashSet::new();
        pending.retain(|m| first.insert(m.info.session));
        let len = pending.len();
        let offset = if len == 0 { 0 } else { a.delivery_cursor % len };
        let candidates: Vec<_> = pending.iter().cycle().skip(offset).take(len.min(100)).cloned().collect();
        a.delivery_cursor = offset.saturating_add(candidates.len());
        for m in candidates {
            let now = util::now_secs();
            let same = self.sessions.get(&m.target.session).is_some_and(|s| {
                s.alive
                    && s.spawned_ms == m.target.spawned_ms
                    && s.conversation == m.target.conversation
                    && s.hosted.as_ref().and_then(|h| h.pid) == m.target.pid
            });
            if !same {
                let _ = self.attention.as_mut().unwrap().store.update(
                    &m.info.id,
                    DeliveryState::Unknown,
                    "target process or conversation changed",
                    now,
                );
                continue;
            }
            if !self.guarded_idle_ready(m.target.session) {
                let _ = self.attention.as_mut().unwrap().store.update(
                    &m.info.id,
                    DeliveryState::Held,
                    "waiting for the session to be idle without a question or unsent draft",
                    now,
                );
                continue;
            }
            let s = &self.sessions[&m.target.session];
            let receipt = super::guarded_input::Receipt {
                key: format!("attention-{}", m.info.id),
                fingerprint: crate::remote::auth::sha256_hex(m.text.as_bytes()),
                session: m.target.session,
                turn_anchor: s.tracker.completed_turns,
                delivered: false,
                inputs: None,
                spawned_ms: Some(s.spawned_ms),
                conversation: s.conversation.clone(),
                attention_id: Some(m.info.id.clone()),
                expected_text: None,
            };
            if self.attention.as_mut().unwrap().store.begin(&m.info.id, now).is_err() {
                continue;
            }
            if self.guarded_idle_start(receipt, &m.text).is_err() {
                let _ = self.attention.as_mut().unwrap().store.update(
                    &m.info.id,
                    DeliveryState::Unknown,
                    "guarded admission uncertain",
                    now,
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/attention/service.rs"]
mod tests;
