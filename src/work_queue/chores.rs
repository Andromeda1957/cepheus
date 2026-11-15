//! Fenced scheduling leases. Repository plugins retain their own independent authority.
use super::*;
use anyhow::{Context as _, Result};
use std::collections::BTreeMap;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Context {
    pub id: String,
    pub day: String,
    pub key: String,
    pub generation: u64,
    pub token: String,
    pub session: String,
}
impl Context {
    pub fn env(&self) -> Vec<(String, String)> {
        [
            ("ID", self.id.clone()),
            ("DAY", self.day.clone()),
            ("KEY", self.key.clone()),
            ("GENERATION", self.generation.to_string()),
            ("TOKEN", self.token.clone()),
            ("SESSION", self.session.clone()),
        ]
        .into_iter()
        .flat_map(|(k, v)| crate::paths::env_names(&format!("CHORE_{k}")).map(|name| (name, v.clone())))
        .collect()
    }
    pub fn guard(&self, args: &[String]) -> Vec<String> {
        args.iter()
            .map(|s| match s.as_str() {
                "{chore.day}" => self.day.clone(),
                "{chore.key}" => self.key.clone(),
                "{chore.generation}" => self.generation.to_string(),
                "{chore.session}" => self.session.clone(),
                _ => s.clone(),
            })
            .collect()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChoreSpec {
    pub id: String,
    pub owner: String,
    pub repository: String,
    pub title: String,
    pub target: String,
    #[serde(default = "completion")]
    pub completion: String,
    pub schedule: Schedule,
    pub spec: Spec,
    #[serde(default)]
    pub guard: Vec<String>,
    #[serde(default)]
    pub enabled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claim {
    pub info: ClaimInfo,
    pub token: String,
    pub key: String,
    pub occurrence: String,
    pub item: Option<u64>,
    pub outcome: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chore {
    pub spec: ChoreSpec,
    pub registered: bool,
    pub claims: BTreeMap<String, Claim>,
    #[serde(default)]
    pub retired: BTreeMap<String, Claim>,
    pub next_run: Option<u64>,
}
fn completion() -> String {
    "dispatch".into()
}
impl ChoreSpec {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            store::valid_text(&self.id, 256)
                && store::valid_text(&self.owner, 64)
                && store::valid_text(&self.repository, 4096)
                && store::valid_text(&self.title, 200),
            "invalid chore identity"
        );
        anyhow::ensure!(matches!(self.target.as_str(), "job" | "session" | "idle"), "invalid chore target");
        anyhow::ensure!(
            (self.target == "job" && self.spec.kind == "job")
                || (matches!(self.target.as_str(), "session" | "idle") && self.spec.kind == "session"),
            "chore target and queue kind differ"
        );
        anyhow::ensure!(
            self.guard.len() <= 32 && self.guard.iter().all(|s| s.len() <= 4096 && !s.contains('\0')),
            "invalid chore guard"
        );
        anyhow::ensure!(matches!(self.completion.as_str(), "dispatch" | "worker"), "invalid chore completion policy");
        anyhow::ensure!(self.target != "idle" || self.guard.is_empty(), "idle chores cannot run a prelaunch guard");
        self.schedule.validate()?;
        self.spec.validate()
    }
}
impl Chore {
    /// Retry the current generation until strictly stale; transport retries of
    /// older generations remain read-only and can never acquire its replacement.
    pub fn scheduled_key(&self, occurrence: &str, now: u64) -> Option<String> {
        match self.claims.get(occurrence) {
            Some(c) if c.outcome.is_some() => None,
            Some(c) if now.saturating_sub(c.info.claimed_at) <= 8 * 3600 => Some(c.key.clone()),
            old => Some(format!(
                "scheduled-{:010x}-{:010x}-g{}",
                store::identity(&self.spec.id),
                store::identity(occurrence),
                old.map(|c| c.info.generation + 1).unwrap_or(1)
            )),
        }
    }
    fn prune(&mut self, keep: &str, now: u64) {
        // Bound the file and request replay window. Current occurrence is never
        // forgotten; older occurrences expire after 30 days or under pressure
        // once their eight-hour scheduling lease has elapsed.
        self.retired.retain(|_, c| now.saturating_sub(c.info.claimed_at) <= 30 * 86400);
        self.claims.retain(|key, c| {
            key == keep
                || c.outcome.is_none() && c.item.is_some()
                || now.saturating_sub(c.info.claimed_at) <= 30 * 86400
        });
        while self.retired.len() >= 100 {
            let key = self.retired.iter().min_by_key(|(_, c)| c.info.claimed_at).map(|(k, _)| k.clone()).unwrap();
            self.retired.remove(&key);
        }
        while self.claims.len() >= 100 {
            let key = self
                .claims
                .iter()
                .filter(|(key, c)| {
                    key.as_str() != keep
                        && (c.outcome.is_some() || c.item.is_none())
                        && now.saturating_sub(c.info.claimed_at) > 8 * 3600
                })
                .min_by_key(|(_, c)| c.info.claimed_at)
                .map(|(k, _)| k.clone());
            let Some(key) = key else { break };
            self.claims.remove(&key);
        }
    }

    pub fn info(&self, enabled: bool, _now: u64) -> ChoreInfo {
        let claim = self.claims.values().max_by_key(|c| (c.info.claimed_at, c.info.generation));
        ChoreInfo {
            id: self.spec.id.clone(),
            owner: self.spec.owner.clone(),
            repository: self.spec.repository.clone(),
            title: self.spec.title.clone(),
            registered: self.registered,
            enabled: self.spec.enabled,
            effective_enabled: enabled && self.spec.enabled && self.registered,
            target: self.spec.target.clone(),
            schedule: self.spec.schedule.clone(),
            next_run: self.next_run,
            claim: claim.map(|c| c.info.clone()),
            outcome: claim.and_then(|c| c.outcome.clone()),
            owner_live: false,
            placement: self.spec.spec.placement.clone(),
            guarded: !self.spec.guard.is_empty(),
            provider: self.spec.spec.provider,
        }
    }
    pub fn claim(&mut self, occurrence: &str, day: &str, key: &str, session: &str, now: u64) -> Result<Claim> {
        anyhow::ensure!(
            store::valid_text(key, 128) && store::valid_text(session, 128),
            "invalid chore claim key or session"
        );
        if let Some(prior) = self.claims.values().chain(self.retired.values()).find(|c| c.key == key) {
            anyhow::ensure!(prior.info.session == session, "chore request belongs to another session");
            return Ok(prior.clone());
        }
        self.prune(occurrence, now);
        anyhow::ensure!(
            self.claims.contains_key(occurrence) || self.claims.len() < 100,
            "chore has too many active scheduling leases"
        );
        let previous = self.claims.get(occurrence);
        if let Some(prior) = previous {
            anyhow::ensure!(prior.outcome.is_none(), "chore occurrence already completed");
            anyhow::ensure!(now.saturating_sub(prior.info.claimed_at) > 8 * 3600, "chore occurrence already claimed");
        }
        let generation = previous.map(|c| c.info.generation + 1).unwrap_or(1);
        let claim = Claim {
            info: ClaimInfo {
                day: day.into(),
                generation,
                session: session.into(),
                claimed_at: now,
                stale_at: now.saturating_add(8 * 3600),
            },
            token: crate::util::uuid_v4()?,
            key: key.into(),
            occurrence: occurrence.into(),
            item: None,
            outcome: None,
        };
        if let Some(old) = self.claims.insert(occurrence.into(), claim.clone()) {
            self.retired.insert(old.key.clone(), old);
        }
        Ok(claim)
    }
    pub fn complete(&mut self, day: &str, generation: u64, token: &str, session: &str, outcome: &str) -> Result<()> {
        anyhow::ensure!(matches!(outcome, "done" | "clear" | "failed"), "chore outcome must be categorical");
        let c = self
            .claims
            .values_mut()
            .find(|c| {
                c.info.day == day && c.info.generation == generation && c.token == token && c.info.session == session
            })
            .context("chore claim was replaced or belongs to another session")?;
        if let Some(previous) = &c.outcome {
            anyhow::ensure!(previous == outcome, "chore already has a different outcome");
            return Ok(());
        }
        c.outcome = Some(outcome.into());
        Ok(())
    }
}
impl store::Queue {
    pub fn chore_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("chore-{}.json", crate::remote::auth::sha256_hex(id.as_bytes())))
    }
    pub fn save_chore(&self, id: &str) -> Result<()> {
        crate::job_sessions::store::write(&self.chore_path(id), self.chores.get(id).context("no such chore")?)
    }
    pub fn chore_register(&mut self, mut spec: ChoreSpec, now: u64) -> Result<()> {
        spec.validate()?;
        if let Some(prior) = self.chores.get(&spec.id) {
            spec.enabled = prior.spec.enabled;
            if prior.spec == spec && prior.registered {
                return Ok(());
            }
        }
        let next = if let Some(prior) = self.chores.get(&spec.id) {
            anyhow::ensure!(
                prior.spec.owner == spec.owner && prior.spec.repository == spec.repository,
                "chore belongs to another owner or repository"
            );
            let mut next = prior.clone();
            let enabled = next.spec.enabled;
            next.spec = spec;
            next.spec.enabled = enabled;
            next.registered = true;
            next.next_run = next.spec.schedule.next(now)?;
            next
        } else {
            anyhow::ensure!(self.chores.len() < 100, "chore capacity reached");
            Chore {
                next_run: spec.schedule.next(now)?,
                spec,
                registered: true,
                claims: BTreeMap::new(),
                retired: BTreeMap::new(),
            }
        };
        crate::job_sessions::store::write(&self.chore_path(&next.spec.id), &next)?;
        self.chores.insert(next.spec.id.clone(), next);
        Ok(())
    }
}
#[cfg(test)]
#[path = "../tests/work_queue/chores.rs"]
mod tests;
