//! Source registration and whole-snapshot validation. Provider policy lives in plugins.
use super::*;
use anyhow::{Context, Result};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceSpec {
    pub id: String,
    pub owner: String,
    pub repository: String,
    #[serde(default = "push")]
    pub mode: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default = "interval")]
    pub interval_seconds: u64,
    #[serde(default = "yes")]
    pub enabled: bool,
}
fn push() -> String {
    "push".into()
}
fn interval() -> u64 {
    20
}
fn yes() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    #[serde(skip)]
    pub owner_live: bool,
    pub spec: SourceSpec,
    pub token: String,
    pub sequence: u64,
    pub updated_at: Option<u64>,
    pub error: Option<String>,
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub refresh_job: Option<u64>,
    #[serde(skip)]
    pub refreshing: bool,
    #[serde(skip)]
    pub attempted_at: u64,
}
impl SourceSpec {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            store::valid_text(&self.id, 256)
                && store::valid_text(&self.owner, 64)
                && store::valid_text(&self.repository, 4096),
            "invalid source identity"
        );
        anyhow::ensure!((20..=86400).contains(&self.interval_seconds), "source interval must be 20..86400 seconds");
        anyhow::ensure!(matches!(self.mode.as_str(), "push" | "command"), "invalid source mode");
        if self.mode == "command" {
            anyhow::ensure!(self.argv.first().is_some_and(|s| !s.is_empty()), "source command requires an executable");
            anyhow::ensure!(self.cwd.as_ref().is_some_and(|d| d.is_absolute() && d.is_dir()), "source cwd unavailable");
            let mut argv = vec![
                "/usr/bin/timeout".into(),
                "--signal=TERM".into(),
                "--kill-after=2".into(),
                "--".into(),
                "60".into(),
            ];
            argv.extend(self.argv.clone());
            crate::job_sessions::Spec {
                argv,
                cwd: self.cwd.clone().context("source cwd missing")?,
                ..Default::default()
            }
            .validate()?;
        } else {
            anyhow::ensure!(self.argv.is_empty(), "push source does not run commands");
        }
        Ok(())
    }
}
impl Source {
    pub fn info(&self, now: u64) -> SourceInfo {
        SourceInfo {
            id: self.spec.id.clone(),
            owner: self.spec.owner.clone(),
            repository: self.spec.repository.clone(),
            host: None,
            enabled: self.spec.enabled,
            updated_at: self.updated_at,
            fresh: self.owner_live
                && self.spec.enabled
                && self.error.is_none()
                && self
                    .updated_at
                    .is_some_and(|at| now.saturating_sub(at) <= self.spec.interval_seconds.saturating_mul(3).max(60)),
            error: self.error.clone(),
            omitted: 0,
        }
    }
}
impl store::Queue {
    pub fn source_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("source-{}.json", crate::remote::auth::sha256_hex(id.as_bytes())))
    }
    pub fn source_register(&mut self, spec: SourceSpec) -> Result<serde_json::Value> {
        spec.validate()?;
        if let Some(old) = self.sources.get(&spec.id) {
            anyhow::ensure!(
                old.spec.owner == spec.owner && old.spec.repository == spec.repository,
                "source belongs to another owner or repository"
            );
            if old.spec == spec {
                return Ok(
                    serde_json::json!({"source":old.info(crate::util::now_secs()),"publisher_token":old.token,"sequence":old.sequence}),
                );
            }
        }
        anyhow::ensure!(self.sources.contains_key(&spec.id) || self.sources.len() < 100, "source capacity reached");
        let id = spec.id.clone();
        let source = Source {
            owner_live: spec.owner == "owner",
            spec,
            token: crate::util::uuid_v4()?,
            sequence: 0,
            updated_at: None,
            error: Some("awaiting refresh".into()),
            tasks: self.sources.get(&id).map(|s| s.tasks.clone()).unwrap_or_default(),
            refresh_job: None,
            refreshing: false,
            attempted_at: 0,
        };
        crate::job_sessions::store::write(&self.source_path(&id), &source)?;
        self.sources.insert(id.clone(), source);
        let source = &self.sources[&id];
        Ok(
            serde_json::json!({"source":source.info(crate::util::now_secs()),"publisher_token":source.token,"sequence":source.sequence}),
        )
    }
    pub fn source_reconcile(&mut self, owner: &str, ids: &[String]) -> Result<()> {
        anyhow::ensure!(
            ids.len() <= 100 && ids.iter().all(|id| store::valid_text(id, 256)),
            "invalid source reconciliation"
        );
        let wanted: std::collections::BTreeSet<_> = ids.iter().collect();
        let omitted: Vec<_> = self
            .sources
            .values()
            .filter(|s| s.spec.owner == owner && s.spec.enabled && !wanted.contains(&s.spec.id))
            .cloned()
            .collect();
        for mut source in omitted {
            source.spec.enabled = false;
            source.error = Some("source disabled".into());
            crate::job_sessions::store::write(&self.source_path(&source.spec.id), &source)?;
            self.sources.insert(source.spec.id.clone(), source);
        }
        Ok(())
    }
    pub fn source_publish(
        &mut self,
        id: &str,
        owner: &str,
        token: &str,
        sequence: u64,
        tasks: Option<Vec<Task>>,
        now: u64,
    ) -> Result<SourceInfo> {
        let old = self.sources.get(id).context("no such registered source")?;
        anyhow::ensure!(
            old.spec.owner == owner && old.token == token && sequence > old.sequence,
            "invalid source owner, publisher or stale sequence"
        );
        anyhow::ensure!(old.owner_live && old.spec.enabled, "source owner unavailable or disabled");
        let mut next = old.clone();
        next.sequence = sequence;
        next.refresh_job = None;
        next.refreshing = false;
        next.attempted_at = now;
        if let Some(mut tasks) = tasks {
            anyhow::ensure!(
                tasks.len() <= 1000 && serde_json::to_vec(&tasks)?.len() <= 262144,
                "source snapshot too large"
            );
            anyhow::ensure!(
                self.sources
                    .iter()
                    .filter(|(other, _)| other.as_str() != id)
                    .map(|(_, s)| s.tasks.len())
                    .sum::<usize>()
                    + tasks.len()
                    <= 1000,
                "fleet source row capacity reached"
            );
            let mut ids = std::collections::BTreeSet::new();
            for task in &mut tasks {
                anyhow::ensure!(
                    store::valid_text(&task.id, 256)
                        && store::valid_text(&task.title, 200)
                        && ids.insert(task.id.clone()),
                    "invalid or duplicate source task"
                );
                anyhow::ensure!(
                    matches!(task.state.as_str(), "active" | "ready" | "merged" | "stopped"),
                    "invalid source state"
                );
                anyhow::ensure!(
                    task.worktree.as_ref().is_none_or(|s| store::valid_text(s, 4096))
                        && task.branch.as_ref().is_none_or(|s| store::valid_text(s, 256))
                        && task.next_step.as_ref().is_none_or(|s| s.len() <= 4000 && !s.contains('\0')),
                    "invalid source metadata"
                );
                anyhow::ensure!(
                    task.target.as_ref().is_none_or(|target| {
                        store::valid_text(&target.tool, 64)
                            && crate::protocol::Tool::parse(&target.tool).is_some_and(|tool| tool.is_agent())
                            && crate::service::valid_conversation_id(&target.conversation)
                    }),
                    "invalid source conversation target"
                );
                task.source_id = id.into();
            }
            next.tasks = tasks;
            next.updated_at = Some(now);
            next.error = None;
        } else {
            next.error = Some("source refresh failed".into());
        }
        // Unchanged rows only renew freshness, in memory: a publisher that
        // republishes on a timer costs no disk write.
        if next.tasks != old.tasks || next.error != old.error {
            crate::job_sessions::store::write(&self.source_path(id), &next)?;
        }
        let info = next.info(now);
        self.sources.insert(id.into(), next);
        Ok(info)
    }
}

#[cfg(test)]
#[path = "../tests/work_queue/sources.rs"]
mod tests;
