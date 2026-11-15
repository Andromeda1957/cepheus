//! Service policy adapter; subprocess custody lives in the independent supervisor.
use super::*;
use crate::job_sessions::{self as jobs, Executor, Spec};
impl Service {
    pub(super) fn job_command(&mut self, action: &str, args: &Value) -> Result<Value> {
        anyhow::ensure!(
            self.job_sessions.is_some(),
            "Job storage unavailable; inspect private job receipts before retrying"
        );
        self.job_sessions.as_mut().unwrap().refresh()?;
        let id = || -> Result<u64> { args["job"].as_u64().context("job id required") };
        match action {
            "job-start" => {
                let key = args["key"].as_str().context("job request key required")?.to_string();
                let title = args["title"].as_str().unwrap_or("Job");
                anyhow::ensure!(title.len() <= 200 && !title.chars().any(char::is_control), "invalid job title");
                let placement = args["placement"].as_str().unwrap_or("auto");
                anyhow::ensure!(placement.len() <= 128 && !placement.is_empty(), "invalid job placement");
                let mut spec = args.clone();
                for field in ["key", "title", "placement", "repository", "gate_binding"] {
                    spec.as_object_mut().unwrap().remove(field);
                }
                let spec: Spec = serde_json::from_value(spec).context("invalid job specification")?;
                let repository = args["repository"].as_str().map(str::to_string);
                if let Some(id) = &repository {
                    let (_, root) = self
                        .scheduler
                        .repository_for(&spec.cwd)
                        .filter(|(name, _)| name == id)
                        .context("job cwd is outside registered repository")?;
                    let cwd = spec.cwd.canonicalize()?;
                    anyhow::ensure!(cwd.starts_with(root.canonicalize()?), "job cwd escaped registered repository");
                }
                let info = self.job_sessions.as_mut().unwrap().add_gated(
                    key,
                    title.into(),
                    spec,
                    placement.into(),
                    repository,
                    args["gate_binding"].as_str().map(str::to_string),
                )?;
                self.state_changed();
                Ok(serde_json::json!({"job":info}))
            }
            "job-policy" => {
                let key = args["key"].as_str().context("job key required")?;
                let binding = args["binding"].as_str().context("job gate binding required")?;
                let revision = args["revision"].as_u64().context("job policy revision required")?;
                let enabled = args["enabled"].as_bool().context("job policy enabled required")?;
                let ack = self.job_sessions.as_mut().unwrap().policy(key, binding, revision, enabled)?;
                Ok(serde_json::json!({"policy":ack}))
            }
            "job-status" => {
                let all = if let Some(key) = args["key"].as_str() {
                    self.job_sessions
                        .as_ref()
                        .unwrap()
                        .records
                        .values()
                        .filter(|r| r.intent.key == key)
                        .map(|r| r.info.clone())
                        .collect()
                } else {
                    self.build_state().jobs
                };
                let all: Vec<jobs::Info> = if let Some(id) = args["job"].as_u64() {
                    all.into_iter().filter(|j| j.id == id).collect()
                } else {
                    all
                };
                Ok(serde_json::json!({"jobs":all}))
            }
            "job-log" => self.job_sessions.as_ref().unwrap().log(
                id()?,
                args["offset"].as_u64().unwrap_or(0),
                args["limit"].as_u64().unwrap_or(65536),
            ),
            "job-stop" => {
                if args["if_unstarted"] == true {
                    let key = args["key"].as_str().context("conditional cancellation key required")?;
                    let (info, cancelled) = self.job_sessions.as_mut().unwrap().cancel_unstarted(key)?;
                    self.state_changed();
                    return Ok(serde_json::json!({"job":info,"cancelled":cancelled,"pending":!cancelled}));
                }
                let info = if let Some(key) = args["key"].as_str() {
                    self.job_sessions.as_mut().unwrap().cancel_key(key)?
                } else {
                    Some(self.job_sessions.as_mut().unwrap().stop(id()?)?)
                };
                let pending = info
                    .as_ref()
                    .is_some_and(|info| self.job_sessions.as_ref().unwrap().workload_pending(info.id).unwrap_or(true));
                self.state_changed();
                Ok(serde_json::json!({"job":info,"cancelled":true,"pending":pending}))
            }
            "job-dismiss" => {
                let info = self.job_sessions.as_mut().unwrap().dismiss(id()?)?;
                self.state_changed();
                Ok(serde_json::json!({"job":info}))
            }
            _ => bail!("unknown job action"),
        }
    }
    pub(super) fn jobs_tick(&mut self) {
        self.expire_job_routes();
        let now = util::now_secs();
        let Some(jobs) = self.job_sessions.as_mut() else { return };
        if jobs.last_poll == now || jobs.idle() {
            // With no job retained or running there is nothing to read.
            return;
        }
        jobs.last_poll = now;
        let before = jobs.snapshot();
        if let Err(error) = jobs.refresh() {
            self.log_error(&format!("Job recovery: {error:#}"));
            return;
        }
        if before != self.job_sessions.as_ref().unwrap().snapshot() {
            self.state_changed();
        }
        let mut records: Vec<_> = self.job_sessions.as_ref().unwrap().records.values().cloned().collect();
        if let Some(queue) = &self.work_queue {
            let ranks: std::collections::BTreeMap<_, _> = queue
                .ordered()
                .into_iter()
                .enumerate()
                .map(|(rank, id)| (queue.records[&id].dispatch_key.clone(), rank))
                .collect();
            records.sort_by_key(|r| ranks.get(&r.intent.key).copied().map(|rank| (0, rank)).unwrap_or((1, 0)));
        }
        for record in records {
            let id = record.info.id;
            if record.executor.as_ref().is_some_and(|e| e.host.is_some()) {
                let target = record.executor.as_ref().unwrap().host.as_ref().unwrap();
                if let Some(remote) =
                    self.job_peer(target).and_then(|s| s.jobs.iter().find(|j| crate::protocol::split_id(j.id).1 == id))
                {
                    let mut info = remote.clone();
                    info.id = id;
                    info.host = Some(target.clone());
                    let changed = self.job_sessions.as_ref().unwrap().records[&id].info != info;
                    self.job_sessions.as_mut().unwrap().records.get_mut(&id).unwrap().info = info;
                    let _ = self.job_sessions.as_mut().unwrap().save_public(id);
                    if changed {
                        self.state_changed();
                    }
                }
            }
            // Explicit cancellations outrank terminal display status and survive
            // shutdown/update; failed/unknown can still own a live workload.
            if let Some(host) = record.executor.as_ref().and_then(|e| e.host.as_ref()) {
                let jobs = self.job_sessions.as_ref().unwrap();
                if jobs.stop_requested(id).unwrap_or(true) {
                    if !jobs.stop_acked(id).unwrap_or(false)
                        && (record.attempted_at == 0 || now.saturating_sub(record.attempted_at) >= 1)
                    {
                        let _ = self.job_peer_send(host, id, "job-stop", serde_json::json!({"key":record.intent.key}));
                        self.job_sessions.as_mut().unwrap().records.get_mut(&id).unwrap().attempted_at = now;
                    }
                    continue;
                }
            }
            self.sync_work_job_policy(id);
            let current = self.job_sessions.as_ref().unwrap().records[&id].info.clone();
            if matches!(current.status.as_str(), "finished" | "failed")
                || self.shutdown_at.is_some()
                || self.updating.is_some()
            {
                continue;
            }
            // A durable peer acceptance ends submission retries, even while the
            // accepted job is held. Peer state still supplies progress and stops
            // above are always forwarded. Lost replies retain the same key.
            if record.executor.as_ref().is_some_and(|e| e.host.is_some())
                && self.job_sessions.as_ref().unwrap().remote_accepted(id).unwrap_or(false)
            {
                continue;
            }
            if record.attempted_at > 0 && now.saturating_sub(record.attempted_at) < 2 {
                continue;
            }
            if record.executor.as_ref().is_none_or(|e| e.host.is_none())
                && self.job_sessions.as_ref().unwrap().spawned(id).unwrap_or(true)
            {
                continue;
            }
            let result = self.dispatch_job(id);
            if let Err(error) = result {
                let jobs = self.job_sessions.as_mut().unwrap();
                let reason = format!("held: {error:#}");
                let changed = jobs.records[&id].info.reason != reason;
                jobs.records.get_mut(&id).unwrap().info.reason = reason;
                let _ = jobs.save_public(id);
                if changed {
                    self.state_changed();
                }
            }
        }
    }
    pub(super) fn job_peer_result(&mut self, host: &str, request: u64, data: &Value, error: Option<&str>) -> bool {
        if request & (1u64 << 63) == 0 {
            return false;
        }
        let request = request & !(1u64 << 63);
        let Some(jobs) = self.job_sessions.as_mut() else { return false };
        let Some(record) = jobs
            .records
            .get_mut(&request)
            .filter(|r| r.executor.as_ref().and_then(|e| e.host.as_deref()) == Some(host))
        else {
            return false;
        };
        if error.is_none() && data.get("policy").is_some() {
            if let Ok(ack) = serde_json::from_value::<jobs::policy::Ack>(data["policy"].clone()) {
                let _ = jobs.save_policy_ack(request, ack);
            }
            self.state_changed();
            return true;
        }
        if let Some(error) = error {
            record.info.reason = format!("remote executor: {error}");
        } else if let Ok(mut info) = serde_json::from_value::<jobs::Info>(data["job"].clone()) {
            info.id = request;
            info.host = Some(host.into());
            record.info = info;
        } else if data["cancelled"] == true && data["job"].is_null() {
            record.info.status = "failed".into();
            record.info.reason = "cancelled before remote dispatch".into();
            record.info.finished_at = Some(util::now_secs());
        }
        if error.is_none()
            && data["cancelled"] != true
            && data.get("job").is_some_and(|job| serde_json::from_value::<jobs::Info>(job.clone()).is_ok())
        {
            // Persist once before forgetting the need to retry this immutable
            // request. A failed marker write leaves retries enabled.
            let _ = jobs.mark_remote_accepted(request);
        }
        if error.is_none() && data["cancelled"] == true && data["pending"] != true {
            let nonce = jobs.records[&request].intent.nonce.clone();
            let _ = jobs::store::write(&jobs.dir(request).join("stop-acked.json"), &nonce);
        }
        let _ = jobs.save_public(request);
        self.state_changed();
        true
    }
    fn dispatch_job(&mut self, id: u64) -> Result<()> {
        let record = self.job_sessions.as_ref().unwrap().records[&id].clone();
        if !self.work_job_current(&record.intent.key) {
            // Only unspawned local intents reach this point. A stale schedule
            // fence cannot start a late provider turn; running turns are kept.
            self.job_sessions.as_mut().unwrap().stop(id)?;
            return Ok(());
        }
        if let Some((_, enabled, _)) = self.work_job_policy(&record.intent.key) {
            anyhow::ensure!(enabled, "work queue paused");
        }
        if record.executor.is_none() {
            let needs_queue = record.intent.gate_binding.is_some()
                || crate::paths::env_names("CHORE_ID").iter().any(|name| record.intent.spec.env.contains_key(name));
            let (host, cwd) =
                self.scheduler_host_min(Some(&record.intent.spec.cwd), &record.intent.placement, true, needs_queue)?;
            let repository = self.scheduler.repository_for(&record.intent.spec.cwd).map(|(id, _)| id);
            let executor = Executor { host: host.clone(), cwd, repository };
            jobs::store::write(&self.job_sessions.as_ref().unwrap().dir(id).join("executor.json"), &executor)?;
            let r = self.job_sessions.as_mut().unwrap().records.get_mut(&id).unwrap();
            r.info.host = host;
            r.executor = Some(executor);
        }
        let executor = self.job_sessions.as_ref().unwrap().records[&id].executor.clone().unwrap();
        if let Some(host) = &executor.host {
            let mut args = serde_json::to_value(&record.intent.spec)?;
            args["key"] = serde_json::json!(record.intent.key);
            args["title"] = serde_json::json!(record.info.title);
            args["placement"] = serde_json::json!("local");
            args["cwd"] = serde_json::json!(executor.cwd);
            args["repository"] = serde_json::json!(executor.repository);
            if let Some(binding) = &record.intent.gate_binding {
                args["gate_binding"] = serde_json::json!(binding);
            }
            self.job_peer_send(host, id, "job-start", args)?;
            self.job_sessions.as_mut().unwrap().records.get_mut(&id).unwrap().attempted_at = util::now_secs();
            return Ok(());
        }
        anyhow::ensure!(
            self.scheduler.error.is_none() && self.scheduler.settings.probe.read().admits(),
            "memory or CPU headroom is short"
        );
        let mut spec = record.intent.spec.clone();
        spec.cwd = executor.cwd;
        if let Some(provider) = spec.provider {
            let (_, account, root, held, reset) =
                self.scheduler_choice(provider, None, spec.account.as_deref(), util::now_secs())?;
            anyhow::ensure!(!held, "provider quota held until {reset:?}");
            anyhow::ensure!(
                self.scheduler.last_started.get(provider.key()).is_none_or(|at| util::now_ms().saturating_sub(*at)
                    >= self.scheduler.settings.stagger_seconds.saturating_mul(1000)),
                "provider starts are staggered"
            );
            if let Some(root) = root {
                let name = if provider == Tool::Claude { "CLAUDE_CONFIG_DIR" } else { "CODEX_HOME" };
                spec.env.insert(name.into(), root.to_string_lossy().into_owned());
            }
            self.job_sessions.as_mut().unwrap().start(id, &self.opts.exe, spec)?;
            self.scheduler_started(provider, account.as_deref(), util::now_secs());
            self.scheduler.save()?;
        } else {
            self.job_sessions.as_mut().unwrap().start(id, &self.opts.exe, spec)?;
        }
        Ok(())
    }
}
