//! Event-loop adapter. Launches are delegated to the existing scheduler and jobs.
use super::*;
use crate::work_queue as w;
impl Service {
    fn work_owner_live(&self, owner: &str) -> bool {
        owner == "owner"
            || self.opts.config.lua
                && self.opts.config.plugins.get(owner) != Some(&false)
                && self
                    .extensions
                    .engine
                    .as_ref()
                    .is_some_and(|engine| engine.service_commands().iter().any(|c| c.plugin == owner))
    }
    pub(super) fn work_snapshot(&self) -> w::Snapshot {
        self.work_snapshot_where(|_| true)
    }
    /// The queue as decks are sent it: merged task rows only where a session
    /// still has that worktree, for its badge. `source-list` has every row.
    pub(super) fn work_state(&self) -> w::Snapshot {
        let worktrees: HashSet<&str> =
            self.sessions.values().filter_map(|s| s.facts.worktree.as_ref()).map(|w| w.path.as_str()).collect();
        self.work_snapshot_where(|t| {
            t.state != "merged" || t.worktree.as_deref().is_some_and(|path| worktrees.contains(path))
        })
    }
    fn work_snapshot_where(&self, keep: impl Fn(&w::Task) -> bool) -> w::Snapshot {
        let Some(q) = self.work_queue.as_ref() else {
            return Default::default();
        };
        let mut snapshot = q.snapshot_where(keep);
        for source in &mut snapshot.sources {
            if !self.work_owner_live(&source.owner) {
                source.fresh = false;
                source.error = Some("source owner unavailable".into());
            }
        }
        for c in &mut snapshot.chores {
            c.owner_live = self.work_owner_live(&c.owner);
            c.effective_enabled &= c.owner_live && self.scheduler.error.is_none() && q.error.is_none();
        }
        snapshot.pause_pending = self.work_pause_pending();
        snapshot.pause_effective = !snapshot.enabled && snapshot.pause_pending.is_empty();
        snapshot
    }
    pub(super) fn work_job_policy(&self, key: &str) -> Option<(u64, bool, String)> {
        let q = self.work_queue.as_ref()?;
        let r = q.records.values().find(|r| r.dispatch_key == key)?;
        Some((q.policy_revision, q.enabled, r.dispatch_key.clone()))
    }
    pub(super) fn sync_work_job_policy(&mut self, id: u64) {
        let Some(record) = self.job_sessions.as_ref().and_then(|j| j.records.get(&id)).cloned() else {
            return;
        };
        if self.work_queue.as_ref().is_some_and(|q| q.error.is_some()) {
            return;
        }
        let Some((revision, enabled, binding)) = self.work_job_policy(&record.intent.key) else {
            return;
        };
        if record.intent.gate_binding.as_ref() != Some(&binding) {
            return;
        }
        let jobs = self.job_sessions.as_ref().unwrap();
        if jobs
            .policy_ack(id)
            .ok()
            .flatten()
            .is_some_and(|a| a.policy.revision == revision && a.policy.enabled == enabled && a.outcome != "pending")
        {
            return;
        }
        if let Some(host) = record.executor.as_ref().and_then(|e| e.host.as_ref()) {
            let _ = self.job_peer_send(
                host,
                id,
                "job-policy",
                serde_json::json!({"key":record.intent.key,"binding":binding,"revision":revision,"enabled":enabled}),
            );
        } else if let Ok(ack) =
            self.job_sessions.as_mut().unwrap().policy(&record.intent.key, &binding, revision, enabled)
        {
            let _ = self.job_sessions.as_ref().unwrap().save_policy_ack(id, ack);
        }
    }
    fn work_pause_pending(&self) -> Vec<String> {
        let Some(q) = &self.work_queue else {
            return Vec::new();
        };
        if q.enabled {
            return Vec::new();
        }
        let Some(jobs) = &self.job_sessions else {
            return vec!["local".into()];
        };
        let mut pending = std::collections::BTreeSet::new();
        for r in jobs.records.values().filter(|r| q.records.values().any(|item| item.dispatch_key == r.intent.key)) {
            if r.info.exit_code.is_some() || r.info.signal.is_some() {
                continue;
            }
            if !jobs.policy_ack(r.info.id).ok().flatten().is_some_and(|a| {
                a.policy.revision == q.policy_revision
                    && !a.policy.enabled
                    && matches!(a.outcome.as_str(), "paused" | "running")
            }) {
                pending.insert(r.executor.as_ref().and_then(|e| e.host.clone()).unwrap_or_else(|| "local".into()));
            }
        }
        pending.into_iter().take(128).collect()
    }
    pub(super) fn work_command(&mut self, action: &str, args: &Value) -> Result<Value> {
        anyhow::ensure!(self.work_queue.is_some(), "Work queue storage unavailable; inspect private queue receipts");
        let now = util::now_secs();
        let id = || args["id"].as_u64().context("queue item id required");
        let text = |name: &str| -> Result<&str> { args[name].as_str().context("required text field missing") };
        let external_spec = |value: Value| -> Result<w::Spec> {
            anyhow::ensure!(
                value.get("guard").is_none() && value.get("chore").is_none(),
                "chore guard metadata belongs to a registered chore"
            );
            Ok(serde_json::from_value(value)?)
        };
        match action {
            "queue-add" => {
                let spec = external_spec(args.clone())?;
                let item = self.work_queue.as_mut().unwrap().add_batch(vec![spec], now)?.remove(0);
                self.state_changed();
                Ok(serde_json::json!({"item":item}))
            }
            "queue-batch" => {
                let values = args["items"].as_array().context("batch items required")?;
                let specs = values.iter().cloned().map(external_spec).collect::<Result<Vec<_>>>()?;
                let items = self.work_queue.as_mut().unwrap().add_batch(specs, now)?;
                self.state_changed();
                Ok(serde_json::json!({"items":items}))
            }
            "queue-list" => {
                let q = self.work_queue.as_ref().unwrap();
                let items: Vec<_> = q
                    .snapshot()
                    .items
                    .into_iter()
                    .filter(|i| {
                        args["id"].as_u64().is_none_or(|id| id == i.id)
                            && args["key"].as_str().is_none_or(|key| q.records[&i.id].spec.key == key)
                    })
                    .collect();
                Ok(
                    serde_json::json!({"enabled":q.enabled,"items":items,"pause_pending":self.work_pause_pending(),"pause_effective":!q.enabled&&self.work_pause_pending().is_empty()}),
                )
            }
            "queue-enable" => {
                let enabled = args["enabled"].as_bool().context("enabled boolean required")?;
                self.work_queue.as_mut().unwrap().set_enabled(enabled)?;
                let ids: Vec<_> =
                    self.job_sessions.as_ref().map(|j| j.records.keys().copied().collect()).unwrap_or_default();
                for id in ids {
                    self.sync_work_job_policy(id);
                }
                self.state_changed();
                Ok(
                    serde_json::json!({"enabled":enabled,"pause_pending":self.work_pause_pending(),"pause_effective":!enabled&&self.work_pause_pending().is_empty()}),
                )
            }
            "queue-cancel" => {
                let item = self.work_queue.as_mut().unwrap().cancel(id()?, now)?;
                self.state_changed();
                Ok(serde_json::json!({"item":item}))
            }
            "queue-reorder" => {
                let ids: Vec<u64> = serde_json::from_value(args["ids"].clone())?;
                let items = self.work_queue.as_mut().unwrap().reorder(&ids, now)?;
                self.state_changed();
                Ok(serde_json::json!({"enabled":self.work_queue.as_ref().unwrap().enabled,"items":items}))
            }
            "source-register" => {
                let spec: w::sources::SourceSpec = serde_json::from_value(args.clone())?;
                anyhow::ensure!(self.work_owner_live(&spec.owner), "source owner is not registered or enabled");
                let source_id = spec.id.clone();
                let result = self.work_queue.as_mut().unwrap().source_register(spec)?;
                self.work_queue.as_mut().unwrap().sources.get_mut(&source_id).unwrap().owner_live = true;
                self.state_changed();
                Ok(result)
            }
            "source-publish" | "source-fail" => {
                let source = text("source")?;
                let owner = text("owner")?;
                anyhow::ensure!(self.work_owner_live(owner), "source owner unavailable");
                self.work_queue.as_mut().unwrap().sources.get_mut(source).context("no such source")?.owner_live = true;
                let token = text("publisher_token")?;
                let sequence = args["sequence"].as_u64().context("source sequence required")?;
                let tasks = if action == "source-fail" {
                    None
                } else {
                    let result = serde_json::from_value::<Vec<w::Task>>(args["tasks"].clone());
                    match result {
                        Ok(tasks) => Some(tasks),
                        Err(error) => {
                            let _ = self
                                .work_queue
                                .as_mut()
                                .unwrap()
                                .source_publish(source, owner, token, sequence, None, now);
                            self.state_changed();
                            return Err(error.into());
                        }
                    }
                };
                let result =
                    self.work_queue.as_mut().unwrap().source_publish(source, owner, token, sequence, tasks, now);
                if result.is_err() && action == "source-publish" {
                    let _ = self.work_queue.as_mut().unwrap().source_publish(source, owner, token, sequence, None, now);
                }
                self.state_changed();
                Ok(serde_json::json!({"source":result?}))
            }
            "source-reconcile" => {
                let owner = text("owner")?;
                anyhow::ensure!(self.work_owner_live(owner), "source owner unavailable");
                let ids: Vec<String> = serde_json::from_value(args["ids"].clone())?;
                self.work_queue.as_mut().unwrap().source_reconcile(owner, &ids)?;
                self.state_changed();
                self.work_command("source-list", &serde_json::json!({}))
            }
            "source-list" | "source-refresh" => {
                if action == "source-refresh" {
                    let key = text("source")?;
                    let q = self.work_queue.as_mut().unwrap();
                    let source = q.sources.get_mut(key).context("no such source")?;
                    source.attempted_at = 0;
                    source.updated_at = None;
                    crate::job_sessions::store::write(&q.source_path(key), &q.sources[key])?;
                    self.state_changed();
                }
                let snapshot = self.work_snapshot();
                let sources: Vec<_> = snapshot
                    .sources
                    .into_iter()
                    .filter(|s| args["source"].as_str().is_none_or(|id| id == s.id))
                    .collect();
                let tasks: Vec<_> = snapshot
                    .tasks
                    .into_iter()
                    .filter(|t| args["source"].as_str().is_none_or(|id| id == t.source_id))
                    .collect();
                Ok(serde_json::json!({"sources":sources,"tasks":tasks}))
            }
            "source-queue" => {
                let source = text("source")?;
                let item = text("item")?;
                let q = self.work_queue.as_ref().unwrap();
                anyhow::ensure!(
                    q.sources.get(source).is_some_and(|s| s.info(now).fresh && s.tasks.iter().any(|t| t.id == item)),
                    "source task is unavailable or stale"
                );
                let mut spec = external_spec(args["spec"].clone())?;
                spec.source = Some(w::SourceRef { source_id: source.into(), item_id: item.into() });
                let item = self.work_queue.as_mut().unwrap().add_batch(vec![spec], now)?.remove(0);
                self.state_changed();
                Ok(serde_json::json!({"item":item}))
            }
            "chore-register" => {
                let spec = serde_json::from_value(args.clone())?;
                self.work_queue.as_mut().unwrap().chore_register(spec, now)?;
                self.state_changed();
                self.chore_reply(text("id")?)
            }
            "chore-list" | "chore-probe" => {
                let snapshot = self.work_snapshot();
                let chores: Vec<_> = snapshot
                    .chores
                    .into_iter()
                    .filter(|c| {
                        args["id"].as_str().is_none_or(|id| id == c.id)
                            && args["repository"].as_str().is_none_or(|repo| repo == c.repository)
                    })
                    .collect();
                let mut result = serde_json::json!({"enabled":snapshot.enabled,"chores":chores});
                if action == "chore-probe" {
                    for row in result["chores"].as_array_mut().unwrap() {
                        if let Some(c) =
                            row["id"].as_str().and_then(|id| self.work_queue.as_ref().unwrap().chores.get(id))
                        {
                            row["guard_fingerprint"] =
                                serde_json::json!(crate::remote::auth::sha256_hex(&serde_json::to_vec(&c.spec.guard)?));
                        }
                    }
                }
                Ok(result)
            }
            "chore-enable" | "chore-update" | "chore-remove" => {
                let key = text("id")?;
                let q = self.work_queue.as_mut().unwrap();
                let c = q.chores.get_mut(key).context("no such chore")?;
                let previous = c.clone();
                match action {
                    "chore-enable" => c.spec.enabled = args["enabled"].as_bool().context("enabled boolean required")?,
                    "chore-update" => {
                        let schedule: w::Schedule = serde_json::from_value(args["schedule"].clone())?;
                        schedule.validate()?;
                        c.next_run = schedule.next(now)?;
                        c.spec.schedule = schedule;
                    }
                    _ => {
                        anyhow::ensure!(
                            !c.spec.enabled && c.claims.values().all(|c| c.outcome.is_some()),
                            "disable chore and finish its active claims before removal"
                        );
                        c.registered = false;
                    }
                }
                if let Err(error) = q.save_chore(key) {
                    q.chores.insert(key.into(), previous);
                    return Err(error);
                }
                self.state_changed();
                self.chore_reply(key)
            }
            "chore-claim" => {
                let key = text("id")?;
                let claim = self.claim_chore(key, text("key")?, text("session")?, now)?;
                self.state_changed();
                Ok(
                    serde_json::json!({"claim":{"day":claim.info.day,"generation":claim.info.generation,"session":claim.info.session,"claimed_at":claim.info.claimed_at,"stale_at":claim.info.stale_at,"token":claim.token,"key":claim.key,"occurrence":claim.occurrence,"item":claim.item,"outcome":claim.outcome}}),
                )
            }
            "chore-complete" => {
                let key = text("id")?;
                let q = self.work_queue.as_mut().unwrap();
                let c = q.chores.get_mut(key).context("no such chore")?;
                let old = c.clone();
                c.complete(
                    text("day")?,
                    args["generation"].as_u64().context("generation required")?,
                    text("token")?,
                    text("session")?,
                    text("outcome")?,
                )?;
                if c == &old {
                    return self.chore_reply(key);
                }
                if let Err(e) = q.save_chore(key) {
                    q.chores.insert(key.into(), old);
                    return Err(e);
                }
                self.state_changed();
                self.chore_reply(key)
            }
            "chore-run" => {
                let key = text("id")?;
                let item = self.run_chore(key, text("key")?, now)?;
                let mut result = self.chore_reply(key)?;
                result["item"] = serde_json::to_value(item)?;
                Ok(result)
            }
            "work-receipt" => self.work_turn_receipt(text("key")?),
            "work-start" => self.work_interactive_start(args),
            "work-idle" => {
                let mut spec: w::Spec = serde_json::from_value(args.clone())?;
                if let Some(repository) = args["repository"].as_str() {
                    spec.cwd = self.scheduler.placed_directory(repository, &spec.cwd)?;
                }
                self.work_idle_start(&spec, None)
            }
            _ => bail!("unknown work queue action"),
        }
    }
    fn chore_reply(&self, id: &str) -> Result<Value> {
        let chore = self.work_snapshot().chores.into_iter().find(|c| c.id == id).context("no such chore")?;
        Ok(serde_json::json!({"chore":chore}))
    }
    pub(super) fn work_job_current(&self, key: &str) -> bool {
        let Some(q) = &self.work_queue else {
            return true;
        };
        let Some(ctx) = q.records.values().find(|r| r.dispatch_key == key).and_then(|r| r.spec.chore.as_ref()) else {
            return true;
        };
        q.chores.get(&ctx.id).is_some_and(|chore| {
            chore.claims.values().any(|c| {
                c.token == ctx.token
                    && c.info.generation == ctx.generation
                    && c.info.session == ctx.session
                    && c.outcome.is_none()
            })
        })
    }
    fn quiesce_prior_chore(&mut self, claim: &w::chores::Claim) -> Result<()> {
        let Some(item) = claim.item else {
            return Ok(());
        };
        let r = self
            .work_queue
            .as_ref()
            .unwrap()
            .records
            .get(&item)
            .context("prior chore custody is missing; scheduling takeover held")?
            .clone();
        if r.info.state == "done" {
            return Ok(());
        }
        if r.spec.kind == "job" {
            let jobs = self.job_sessions.as_ref().context("prior chore job custody unavailable")?;
            let record = r
                .info
                .dispatch
                .as_ref()
                .and_then(|d| jobs.records.get(&d.id))
                .or_else(|| jobs.records.values().find(|j| j.intent.key == r.dispatch_key))
                .cloned();
            let Some(record) = record else {
                anyhow::ensure!(!r.attempted, "prior chore launch outcome unknown; scheduling takeover held");
                return Ok(());
            };
            if matches!(record.info.status.as_str(), "finished" | "failed") {
                let known = record.info.exit_code.is_some()
                    || record.info.signal.is_some()
                    || if record.executor.as_ref().is_some_and(|e| e.host.is_some()) {
                        jobs.stop_acked(record.info.id)?
                    } else {
                        crate::job_sessions::store::optional::<crate::job_sessions::Exit>(
                            &jobs.dir(record.info.id).join("exit.json"),
                        )?
                        .is_some_and(|exit| exit.nonce == record.intent.nonce)
                            && !jobs.workload_pending(record.info.id)?
                    };
                anyhow::ensure!(known, "prior chore exit is unknown; scheduling takeover held");
                return Ok(());
            }
            anyhow::ensure!(
                record.info.status == "starting",
                "prior chore turn is still running; scheduling takeover held"
            );
            if let Some(host) = record.executor.as_ref().and_then(|e| e.host.as_ref()) {
                self.job_peer_send(
                    host,
                    record.info.id,
                    "job-stop",
                    serde_json::json!({"key":record.intent.key,"if_unstarted":true}),
                )?;
                bail!("waiting for prior unstarted chore intent to reconcile");
            }
            let (_, cancelled) = self.job_sessions.as_mut().unwrap().cancel_unstarted(&record.intent.key)?;
            anyhow::ensure!(cancelled, "prior chore launch gate is owned; scheduling takeover held");
            return Ok(());
        }
        let Some(dispatch) = r.info.dispatch.clone() else {
            anyhow::ensure!(!r.attempted, "prior interactive launch outcome unknown; scheduling takeover held");
            return Ok(());
        };
        let state = self.build_state();
        let session = state
            .sessions
            .iter()
            .find(|s| s.id == dispatch.id)
            .context("prior chore session is unavailable; scheduling takeover held")?;
        anyhow::ensure!(
            session.alive
                && session.status == Status::Finished
                && r.turn_anchor.is_none_or(|anchor| session
                    .metadata
                    .get("_deck_completed_turns")
                    .and_then(Value::as_u64)
                    .is_some_and(|turn| turn > anchor)),
            "prior chore turn is running or unknown; scheduling takeover held"
        );
        Ok(())
    }
    fn claim_chore(&mut self, id: &str, key: &str, session: &str, now: u64) -> Result<w::chores::Claim> {
        self.work_reconcile_outcomes()?;
        let c = self.work_queue.as_ref().unwrap().chores.get(id).context("no such chore")?;
        anyhow::ensure!(
            c.registered && self.work_owner_live(&c.spec.owner),
            "chore owner is not registered or enabled"
        );
        let calendar = w::schedule::calendar(now, &c.spec.schedule.timezone)?;
        let occurrence = c.spec.schedule.claim_occurrence(&calendar)?;
        let previous_claim = c.claims.get(&occurrence).cloned();
        if let Some(old) = previous_claim
            .filter(|old| old.key != key && old.outcome.is_none() && now.saturating_sub(old.info.claimed_at) > 8 * 3600)
        {
            self.quiesce_prior_chore(&old)?;
        }
        let q = self.work_queue.as_mut().unwrap();
        let previous = q.chores[id].clone();
        let claim = q.chores.get_mut(id).unwrap().claim(&occurrence, &calendar.day, key, session, now)?;
        if q.chores[id] == previous {
            return Ok(claim);
        }
        let c = q.chores.get_mut(id).unwrap();
        let claimed = c.claims.keys().cloned().collect();
        c.next_run = c.spec.schedule.next_unclaimed(now, &claimed)?;
        if let Err(e) = q.save_chore(id) {
            q.chores.insert(id.into(), previous);
            return Err(e);
        }
        Ok(claim)
    }
    fn run_chore(&mut self, id: &str, key: &str, now: u64) -> Result<w::Info> {
        let c = self.work_snapshot().chores.into_iter().find(|c| c.id == id).context("no such chore")?;
        anyhow::ensure!(c.effective_enabled, "enable the work queue, registered owner and chore before running");
        let session = format!("chore-{:010x}", w::store::identity(key));
        let claim = self.claim_chore(id, key, &session, now)?;
        if let Some(item) = claim.item {
            return Ok(self
                .work_queue
                .as_ref()
                .unwrap()
                .records
                .get(&item)
                .context("chore result expired")?
                .info
                .clone());
        }
        anyhow::ensure!(claim.outcome.is_none(), "chore occurrence already completed");
        let q = self.work_queue.as_mut().unwrap();
        let c = q.chores.get(id).unwrap();
        let mut spec = c.spec.spec.clone();
        spec.key = format!(
            "chore-{:010x}-{}-{}",
            w::store::identity(id),
            claim.info.generation,
            w::store::identity(&claim.occurrence)
        );
        spec.title = c.spec.title.clone();
        spec.guard = c.spec.guard.clone();
        spec.chore = Some(w::chores::Context {
            id: id.into(),
            day: claim.info.day.clone(),
            key: claim.key.clone(),
            generation: claim.info.generation,
            token: claim.token.clone(),
            session: claim.info.session.clone(),
        });
        let item = q.add_batch(vec![spec], now)?.remove(0);
        q.chores.get_mut(id).unwrap().claims.get_mut(&claim.occurrence).unwrap().item = Some(item.id);
        q.save_chore(id)?;
        self.state_changed();
        Ok(item)
    }
    fn work_reconcile_outcomes(&mut self) -> Result<()> {
        let Some(q) = &self.work_queue else {
            return Ok(());
        };
        let updates: Vec<_> = q
            .chores
            .iter()
            .filter(|(_, c)| c.spec.completion == "dispatch" && c.spec.guard.is_empty())
            .flat_map(|(id, c)| {
                c.claims.values().filter(|claim| claim.outcome.is_none()).filter_map(|claim| {
                    let state = &q.records.get(&claim.item?)?.info.state;
                    let outcome = match state.as_str() {
                        "done" => "done",
                        "failed" => "failed",
                        _ => return None,
                    };
                    Some((id.clone(), claim.clone(), outcome))
                })
            })
            .collect();
        for (id, claim, outcome) in updates {
            let q = self.work_queue.as_mut().unwrap();
            let previous = q.chores[&id].clone();
            q.chores.get_mut(&id).unwrap().complete(
                &claim.info.day,
                claim.info.generation,
                &claim.token,
                &claim.info.session,
                outcome,
            )?;
            if let Err(error) = q.save_chore(&id) {
                q.chores.insert(id, previous);
                return Err(error);
            }
        }
        Ok(())
    }
    pub(super) fn work_tick(&mut self) {
        self.work_tick_at(util::now_secs());
    }
    fn work_tick_at(&mut self, now: u64) {
        let Some(q) = self.work_queue.as_mut() else {
            return;
        };
        if q.error.is_some() {
            let _ = q.publication_barrier();
        }
        if q.last_poll == now {
            return;
        }
        q.last_poll = now;
        let before = self.work_state();
        let bindings: Vec<_> = self
            .work_queue
            .as_ref()
            .unwrap()
            .sources
            .values()
            .map(|s| (s.spec.id.clone(), self.work_owner_live(&s.spec.owner)))
            .collect();
        for (id, live) in bindings {
            let q = self.work_queue.as_mut().unwrap();
            let source = q.sources.get_mut(&id).unwrap();
            let was_live = source.owner_live;
            source.owner_live = live;
            if !live && (was_live || source.error.as_deref() != Some("source owner unavailable")) {
                source.error = Some("source owner unavailable".into());
                let record = source.clone();
                let _ = crate::job_sessions::store::write(&q.source_path(&id), &record);
            }
        }
        let admitted = self.scheduler.error.is_none() && self.scheduler.settings.probe.read().admits();
        if let Err(_error) = self.work_queue.as_mut().unwrap().refresh_sources(
            &self.opts.exe,
            now,
            admitted && self.shutdown_at.is_none() && self.updating.is_none(),
        ) {
            // Provider output and private command payloads never become fleet errors.
        }
        self.work_observe_dispatches(now);
        if self.work_reconcile_outcomes().is_err() {
            return;
        }
        if self.shutdown_at.is_none()
            && self.updating.is_none()
            && self.work_queue.as_ref().unwrap().enabled
            && self.work_queue.as_ref().unwrap().error.is_none()
        {
            let chores: Vec<_> = self.work_snapshot().chores.into_iter().filter(|c| c.effective_enabled).collect();
            for chore in chores {
                if let Ok(cal) = w::schedule::calendar(now, &chore.schedule.timezone) {
                    if let Ok(Some(occurrence)) = chore.schedule.occurrence(&cal) {
                        let Some(key) =
                            self.work_queue.as_ref().unwrap().chores[&chore.id].scheduled_key(&occurrence, now)
                        else {
                            continue;
                        };
                        let _ = self.run_chore(&chore.id, &key, now);
                    }
                }
            }
            let ids = self.work_queue.as_ref().unwrap().ordered();
            // Existing admission runs for every actual launch; a queue tick never
            // replaces resource, quota, account, repository or stagger checks.
            for id in ids {
                let record = &self.work_queue.as_ref().unwrap().records[&id];
                if matches!(record.info.state.as_str(), "done" | "failed") || record.info.dispatch.is_some() {
                    continue;
                }
                if !self.work_queue.as_ref().unwrap().dependencies_ready(id, now) {
                    let _ = self.work_queue.as_mut().unwrap().transition(
                        id,
                        "held",
                        "dependency not satisfied or source stale",
                        now,
                        None,
                    );
                    continue;
                }
                if let Some(ctx) = &record.spec.chore {
                    let current = self.work_queue.as_ref().unwrap().chores.get(&ctx.id).and_then(|c| {
                        c.claims.values().find(|c| {
                            c.token == ctx.token
                                && c.info.generation == ctx.generation
                                && c.info.day == ctx.day
                                && c.outcome.is_none()
                        })
                    });
                    if current.is_none() {
                        let _ = self.work_queue.as_mut().unwrap().transition(
                            id,
                            "failed",
                            "chore claim replaced",
                            now,
                            None,
                        );
                        continue;
                    }
                }
                if let Err(error) = self.work_dispatch(id, now) {
                    let reason = if error.to_string().contains("unknown") {
                        "interactive launch outcome unknown; inspect the reserved session"
                    } else if error.to_string().contains("Update needed") {
                        "Update needed: selected host requires protocol 14 work queues"
                    } else {
                        "waiting for admission, quota, placement or launch reconciliation"
                    };
                    let _ = self.work_queue.as_mut().unwrap().transition(id, "held", reason, now, None);
                }
            }
        }
        if now % 60 == 0 {
            let _ = self.work_queue.as_mut().unwrap().gc(now);
            let _ = self.work_receipts_gc(now);
        }
        if before != self.work_state() {
            self.state_changed();
        }
    }
    pub(super) fn work_observe_dispatches(&mut self, now: u64) {
        let Some(q) = &self.work_queue else { return };
        // Only an unfinished dispatch has an outcome to observe; without one
        // the whole deck state is not built every second for nothing.
        if !q.records.values().any(|r| r.info.dispatch.is_some() && !matches!(r.info.state.as_str(), "done" | "failed"))
        {
            return;
        }
        let records: Vec<_> = q.records.values().cloned().collect();
        let state = self.build_state();
        for r in records {
            let Some(ref d) = r.info.dispatch else {
                continue;
            };
            if matches!(r.info.state.as_str(), "done" | "failed") {
                continue;
            }
            let idle = r
                .spec
                .chore
                .as_ref()
                .and_then(|ctx| self.work_queue.as_ref().unwrap().chores.get(&ctx.id))
                .is_some_and(|c| c.spec.target == "idle");
            // A session-wide turn counter cannot prove which idle request
            // finished. Only its accepting executor owns the input fence.
            let result = if idle && d.kind == "session" {
                if let Some(host) = r.executor.as_deref().filter(|h| *h != "local") {
                    let _ = self.work_remote_send(
                        host,
                        r.info.id,
                        "work-receipt",
                        serde_json::json!({"key":r.dispatch_key}),
                    );
                    continue;
                }
                match self.work_turn_receipt(&r.dispatch_key) {
                    Ok(receipt) if receipt["session"].as_u64() == Some(d.id) => r
                        .turn_anchor
                        .filter(|at| receipt["completed_turns"].as_u64().is_some_and(|n| n > *at))
                        .map(|_| ("done", "accepted idle turn finished")),
                    _ => Some(("held", "idle turn custody changed; completion outcome unknown")),
                }
            } else {
                match d.kind.as_str() {
                    "job" => state.jobs.iter().find(|j| j.id == d.id).and_then(|j| match j.status.as_str() {
                        "finished" if j.exit_code == Some(0) => Some(("done", "job finished")),
                        "starting" => Some((
                            "held",
                            if j.reason.contains("quota") {
                                "provider quota held"
                            } else {
                                "awaiting job admission or placement"
                            },
                        )),
                        "working" => Some(("dispatched", "job working")),
                        "failed" => Some(("failed", "job failed or exit unknown")),
                        _ => None,
                    }),
                    "session" => state.sessions.iter().find(|s| s.id == d.id).and_then(|s| {
                        if r.turn_anchor.map_or(s.alive && s.status == Status::Finished, |at| {
                            s.metadata.get("_deck_completed_turns").and_then(Value::as_u64).is_some_and(|n| n > at)
                        }) {
                            Some(("done", "interactive turn finished"))
                        } else if !s.alive {
                            Some(("failed", "interactive session ended; completion unknown"))
                        } else {
                            None
                        }
                    }),
                    _ => None,
                }
            };
            if result.is_none() && d.kind == "session" && !state.sessions.iter().any(|s| s.id == d.id) {
                if let Some(host) = r.executor.as_deref().filter(|h| *h != "local") {
                    let _ = self.work_remote_send(
                        host,
                        r.info.id,
                        "work-receipt",
                        serde_json::json!({"key":r.dispatch_key}),
                    );
                } else if let Ok(receipt) = self.work_turn_receipt(&r.dispatch_key) {
                    if r.turn_anchor.is_some_and(|at| receipt["completed_turns"].as_u64().is_some_and(|n| n > at)) {
                        let _ = self.work_queue.as_mut().unwrap().transition(
                            r.info.id,
                            "done",
                            "completed turn receipt reconciled",
                            now,
                            Some(d.clone()),
                        );
                    }
                }
            }
            if let Some((state, reason)) = result {
                let _ = self.work_queue.as_mut().unwrap().transition(r.info.id, state, reason, now, Some(d.clone()));
                if matches!(state, "done" | "failed") {
                    if let Some(ctx) = &r.spec.chore {
                        let q = self.work_queue.as_mut().unwrap();
                        if let Some(c) = q.chores.get_mut(&ctx.id) {
                            if c.spec.completion == "dispatch" && c.spec.guard.is_empty() {
                                let outcome = if state == "done" { "done" } else { "failed" };
                                let _ = c.complete(&ctx.day, ctx.generation, &ctx.token, &ctx.session, outcome);
                                let _ = q.save_chore(&ctx.id);
                            }
                        }
                    }
                }
            }
        }
    }
    fn work_dispatch(&mut self, id: u64, now: u64) -> Result<()> {
        let r = self.work_queue.as_ref().unwrap().records[&id].clone();
        let idle = r
            .spec
            .chore
            .as_ref()
            .and_then(|ctx| self.work_queue.as_ref().unwrap().chores.get(&ctx.id))
            .is_some_and(|c| c.spec.target == "idle");
        if idle && r.spec.placement == "local" {
            let mut spec = r.spec.clone();
            spec.key = r.dispatch_key.clone();
            let data = self.work_idle_start(&spec, Some(id))?;
            let session = data["session"].as_u64().context("idle session receipt missing")?;
            self.work_queue.as_mut().unwrap().records.get_mut(&id).unwrap().turn_anchor = data["turn_anchor"].as_u64();
            self.work_queue.as_mut().unwrap().transition(
                id,
                "dispatched",
                "delivered to a safely idle session",
                now,
                Some(w::Dispatch { kind: "session".into(), id: session }),
            )?;
            return Ok(());
        }
        if r.spec.kind == "job" {
            let mut spec = crate::job_sessions::Spec {
                argv: r.spec.argv.clone(),
                cwd: r.spec.cwd.clone(),
                provider: r.spec.provider,
                account: r.spec.account.clone(),
                stdin: r.spec.stdin.clone().unwrap_or_default(),
                env_clear: r.spec.env_clear,
                env_allowlist: r.spec.env_allowlist.clone(),
                env: r.spec.env.clone(),
            };
            if let Some(ctx) = &r.spec.chore {
                for (k, v) in ctx.env() {
                    spec.env.insert(k, v);
                }
                if !r.spec.guard.is_empty() {
                    let mut argv = ctx.guard(&r.spec.guard);
                    argv.push("--".into());
                    argv.extend(spec.argv);
                    spec.argv = argv;
                }
            }
            spec.validate()?;
            self.work_queue.as_mut().unwrap().begin_attempt(id)?;
            let mut args = serde_json::to_value(spec)?;
            args["key"] = serde_json::json!(r.dispatch_key.clone());
            args["title"] = serde_json::json!(r.spec.title);
            args["placement"] = serde_json::json!(r.spec.placement);
            args["gate_binding"] = serde_json::json!(r.dispatch_key);
            let result = self.job_command("job-start", &args)?;
            let job = result["job"]["id"].as_u64().context("job acceptance missing")?;
            self.work_queue.as_mut().unwrap().transition(
                id,
                "held",
                "awaiting job admission or placement",
                now,
                Some(w::Dispatch { kind: "job".into(), id: job }),
            )?;
            return Ok(());
        }
        if r.executor.as_deref() == Some("local") && !idle {
            let fingerprint = crate::remote::auth::sha256_hex(&serde_json::to_vec(&r.spec)?);
            if let Some(session) = self.reconcile_launch(&r.dispatch_key, Some(&fingerprint))? {
                if r.turn_anchor.is_none() {
                    self.work_queue.as_mut().unwrap().records.get_mut(&id).unwrap().turn_anchor = Some(0);
                    self.work_queue.as_ref().unwrap().save(id)?;
                }
                self.work_queue.as_mut().unwrap().transition(
                    id,
                    "dispatched",
                    "interactive session receipt reconciled",
                    now,
                    Some(w::Dispatch { kind: "session".into(), id: session }),
                )?;
                return Ok(());
            }
        }
        // Resolve Auto without publishing an executor or completion anchor.
        let (host, cwd) = if let Some(host) = &r.executor {
            (
                (host != "local").then(|| host.clone()),
                r.executor_cwd.clone().context("pinned executor directory is unavailable")?,
            )
        } else {
            self.scheduler_host_min(Some(&r.spec.cwd), &r.spec.placement, false, true)?
        };
        let cloud = r.spec.provider.is_some_and(|tool| match host.as_deref() {
            Some(host) => {
                self.job_peer(host).is_some_and(|state| state.providers.iter().any(|p| p.id == tool.key() && p.cloud))
            }
            None => self.opts.config.providers.iter().any(|p| p.id == tool.key() && p.cloud),
        });
        // A prior peer attempt may have an exact launch receipt. Its executor
        // reconciles that receipt before refusing any new cloud launch.
        let peer_replay = r.attempted && r.executor.as_deref().is_some_and(|host| host != "local");
        if !idle && cloud && !peer_replay {
            self.work_queue.as_mut().unwrap().transition(
                id,
                "held",
                "Cloud providers do not support new queued interactive sessions; connect manually.",
                now,
                None,
            )?;
            return Ok(());
        }
        // A new dedicated session starts with no completed turns. Persist this
        // before launch so receipt replay cannot consume a later owner's turn.
        if !idle && r.turn_anchor.is_none() {
            self.work_queue.as_mut().unwrap().records.get_mut(&id).unwrap().turn_anchor = Some(0);
            self.work_queue.as_ref().unwrap().save(id)?;
        }
        if r.executor.is_none() {
            let q = self.work_queue.as_mut().unwrap();
            let record = q.records.get_mut(&id).unwrap();
            record.executor = Some(host.clone().unwrap_or_else(|| "local".into()));
            record.executor_cwd = Some(cwd.clone());
            q.save(id)?;
        }
        let mut spec = r.spec.clone();
        spec.cwd = cwd;
        if let Some(host) = host {
            anyhow::ensure!(spec.guard.is_empty(), "chore guarded sessions require local placement");
            let (repository, _) = self.scheduler.repository_for(&r.spec.cwd).context("repository unavailable")?;
            let mut args = serde_json::to_value(&spec)?;
            args["repository"] = serde_json::json!(repository);
            args["key"] = serde_json::json!(r.dispatch_key.clone());
            self.work_queue.as_mut().unwrap().begin_attempt(id)?;
            self.work_remote_send(&host, id, if idle { "work-idle" } else { "work-start" }, args)?;
            return Ok(());
        }
        if idle {
            spec.key = r.dispatch_key.clone();
            let data = self.work_idle_start(&spec, Some(id))?;
            let session = data["session"].as_u64().context("idle session receipt missing")?;
            self.work_queue.as_mut().unwrap().records.get_mut(&id).unwrap().turn_anchor = data["turn_anchor"].as_u64();
            self.work_queue.as_mut().unwrap().transition(
                id,
                "dispatched",
                "delivered to a safely idle session",
                now,
                Some(w::Dispatch { kind: "session".into(), id: session }),
            )?;
            return Ok(());
        }
        let tool = spec.provider.context("provider required")?;
        let (_, account, root, held, _) = self.scheduler_choice(tool, None, spec.account.as_deref(), now)?;
        anyhow::ensure!(!held, "provider quota held");
        anyhow::ensure!(
            self.scheduler
                .last_started
                .get(tool.key())
                .is_none_or(|at| util::now_ms().saturating_sub(*at)
                    >= self.scheduler.settings.stagger_seconds.saturating_mul(1000)),
            "provider stagger"
        );
        anyhow::ensure!(
            self.scheduler.error.is_none() && self.scheduler.settings.probe.read().admits(),
            "resource admission held"
        );
        self.work_queue.as_mut().unwrap().begin_attempt(id)?;
        let place = Placement {
            account,
            resource_managed: true,
            account_root: root,
            cwd: Some(spec.cwd),
            name: Some(spec.title),
            prompt: spec.prompt.as_deref().map(crate::keys::non_owner_message),
            launch_key: Some(r.dispatch_key.clone()),
            launch_fingerprint: Some(crate::remote::auth::sha256_hex(&serde_json::to_vec(&r.spec)?)),
            guard: spec.guard,
            chore: spec.chore,
            ..Default::default()
        };
        let session = self.create_with(tool, None, &StartModel { model: spec.model, effort: None }, &place)?;
        self.scheduler_started(tool, place.account.as_deref(), now);
        self.scheduler.save()?;
        self.work_queue.as_mut().unwrap().transition(
            id,
            "dispatched",
            "interactive session accepted",
            now,
            Some(w::Dispatch { kind: "session".into(), id: session }),
        )?;
        Ok(())
    }
    pub(super) fn work_peer_result(
        &mut self,
        host: &str,
        tag: u16,
        request: u64,
        data: &Value,
        error: Option<&str>,
    ) -> bool {
        if request & (1u64 << 62) == 0 || request & (1u64 << 63) != 0 {
            return false;
        }
        let id = request & !(1u64 << 62);
        let Some(q) = self.work_queue.as_ref() else {
            return false;
        };
        let Some(r) = q.records.get(&id).filter(|r| r.executor.as_deref() == Some(host)) else {
            return false;
        };
        if let Some(dispatch) = &r.info.dispatch {
            if error.is_none()
                && dispatch.kind == "session"
                && data["session"].as_u64().is_some_and(|id| crate::protocol::host_id(tag, id) == dispatch.id)
                && r.turn_anchor.is_some_and(|at| data["completed_turns"].as_u64().is_some_and(|n| n > at))
            {
                let dispatch = dispatch.clone();
                let _ = self.work_queue.as_mut().unwrap().transition(
                    id,
                    "done",
                    "peer completed turn receipt reconciled",
                    util::now_secs(),
                    Some(dispatch),
                );
                self.state_changed();
            }
            return true;
        }
        if error.is_none() {
            if let Some(session) = data["session"].as_u64() {
                let session = crate::protocol::host_id(tag, session);
                self.work_queue.as_mut().unwrap().records.get_mut(&id).unwrap().turn_anchor =
                    data["turn_anchor"].as_u64().or(Some(0));
                let _ = self.work_queue.as_mut().unwrap().transition(
                    id,
                    "dispatched",
                    "interactive session accepted on peer",
                    util::now_secs(),
                    Some(w::Dispatch { kind: "session".into(), id: session }),
                );
                self.state_changed();
            }
        }
        true
    }
    fn work_interactive_start(&mut self, args: &Value) -> Result<Value> {
        let mut spec: w::Spec = serde_json::from_value(args.clone())?;
        let fingerprint = crate::remote::auth::sha256_hex(&serde_json::to_vec(&spec)?);
        if let Some(session) = self.reconcile_launch(&spec.key, Some(&fingerprint))? {
            return Ok(serde_json::json!({"session":session}));
        }
        spec.validate()?;
        anyhow::ensure!(spec.kind == "session" && spec.guard.is_empty(), "invalid peer interactive launch");
        let tool = spec.provider.context("provider required")?;
        anyhow::ensure!(
            !self.opts.config.providers.iter().any(|p| p.id == tool.key() && p.cloud),
            "Cloud providers do not support new queued interactive sessions; connect manually."
        );
        if let Some(repository) = args["repository"].as_str() {
            spec.cwd = self.scheduler.placed_directory(repository, &spec.cwd)?;
        }
        let (_, account, root, held, reset) =
            self.scheduler_choice(tool, None, spec.account.as_deref(), util::now_secs())?;
        if held {
            return Ok(serde_json::json!({"held":true,"reason":"quota","reset":reset}));
        }
        let place = Placement {
            launch_key: Some(spec.key.clone()),
            resource_managed: true,
            launch_fingerprint: Some(fingerprint),
            account,
            account_root: root,
            cwd: Some(spec.cwd),
            name: Some(spec.title),
            prompt: spec.prompt.as_deref().map(crate::keys::non_owner_message),
            chore: spec.chore.clone(),
            ..Default::default()
        };
        if !self.registry.sessions.iter().any(|e| e.launch_key.as_ref() == place.launch_key.as_ref()) {
            anyhow::ensure!(
                self.scheduler.last_started.get(tool.key()).is_none_or(|at| util::now_ms().saturating_sub(*at)
                    >= self.scheduler.settings.stagger_seconds.saturating_mul(1000)),
                "provider stagger"
            );
        }
        let session = self.create_with(tool, None, &StartModel { model: spec.model, effort: None }, &place)?;
        self.scheduler_started(tool, place.account.as_deref(), util::now_secs());
        self.scheduler.save()?;
        Ok(serde_json::json!({"session":session}))
    }
}

#[cfg(test)]
#[path = "../tests/work_queue/service.rs"]
mod tests;
