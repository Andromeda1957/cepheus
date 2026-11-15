//! Service adapter for reusable host scheduling; all mutation stays in the event loop.
use super::*;
use crate::scheduler::{Owner, Rerun, Start};
#[cfg(test)]
#[path = "../tests/scheduler_service.rs"]
mod tests;
type SchedulerChoice = (Tool, Option<String>, Option<PathBuf>, bool, Option<u64>);

impl Service {
    pub(super) fn scheduler_snapshot(&self) -> crate::scheduler::Snapshot {
        let mut snapshot = self.scheduler.snapshot();
        let accounts = crate::providers::accounts::Settings::from_config(&self.opts.config).ok();
        snapshot.paused = self
            .sessions
            .values()
            .filter(|s| s.alive && (s.retry.quota_reset.is_some() || s.retry.quota_unknown))
            .map(|s| crate::scheduler::Pause {
                session: s.id,
                provider: s.tool,
                reset: s.retry.quota_reset,
                reason: if s.account.as_ref().is_some_and(|account| {
                    accounts
                        .as_ref()
                        .and_then(|settings| settings.profile(account, s.tool).ok())
                        .is_none_or(|profile| Some(&profile.directory) != s.account_root.as_ref())
                }) {
                    "account profile changed; reset unknown"
                } else {
                    "usage limit"
                }
                .into(),
            })
            .collect();
        snapshot
    }

    pub(super) fn fleet_reply(&mut self, cid: u64, request: u64, action: &str, args: &Value) {
        let result = self.fleet_command(action, args);
        let (data, error) = match result {
            Ok(data) => (data, None),
            Err(e) => (Value::Null, Some(format!("{e:#}"))),
        };
        if cid == 0 {
            if let Some(engine) = self.extensions.engine.as_mut() {
                engine.service_result(request, &data, error.as_deref());
            }
        } else {
            self.send(cid, &ServerMsg::FleetResult { request, data, error });
        }
    }
    pub(super) fn fleet_command(&mut self, action: &str, args: &Value) -> Result<Value> {
        anyhow::ensure!(args.is_object() && serde_json::to_vec(args)?.len() <= 262144, "invalid fleet arguments");
        for (field, limit) in [
            ("cwd", 4096),
            ("compatibility", 4096),
            ("placement", 128),
            ("title", 200),
            ("model", 256),
            ("account", 64),
            ("key", 128),
            ("pool", 64),
            ("label", 200),
            ("prompt", 16000),
            ("repository", if crate::attention::is_action(action) { 4096 } else { 256 }),
        ] {
            if let Some(value) = args.get(field).filter(|v| !v.is_null()) {
                anyhow::ensure!(value.as_str().is_some_and(|s| s.len() <= limit), "invalid or oversized {field}");
            }
        }
        if crate::attention::is_action(action) {
            return self.attention_command(action, args);
        }
        if crate::work_queue::is_action(action) {
            return self.work_command(action, args);
        }
        if action.starts_with("job-") {
            return self.job_command(action, args);
        }
        let id = || args["id"].as_u64().context("id is required");
        match action {
            "capabilities" => Ok(
                serde_json::json!({"protocol":crate::protocol::PROTOCOL_VERSION,"facilities":["slots","admission","queue","reruns","jobs","work-queue","task-sources","chores","attention"]}),
            ),
            "status" | "slot-status" => {
                self.scheduler.tick();
                if let Some(id) = args["id"].as_u64() {
                    return Ok(serde_json::json!({"slot":self.scheduler.slots.iter().find(|s|s.id==id)}));
                }
                Ok(serde_json::to_value(self.scheduler_snapshot())?)
            }
            "admission" => {
                anyhow::ensure!(self.scheduler.error.is_none(), "scheduler recovery required");
                let h = self.scheduler.settings.probe.read();
                Ok(serde_json::json!({"admitted":h.admits(),"memory_free":h.memory_free(),"headroom":h}))
            }
            "slot-hold" => {
                let pid =
                    args["pid"].as_u64().filter(|p| *p <= u32::MAX as u64).context("holder pid is required")? as u32;
                let session = args["session"].as_u64();
                if let Some(session) = session {
                    anyhow::ensure!(self.sessions.get(&session).is_some_and(|s| s.alive), "slot session is not alive");
                }
                let owner = Owner::read(pid, session)?;
                let pool = args["pool"].as_str().unwrap_or("heavy");
                let label = args["label"].as_str().unwrap_or("heavy command");
                let compatibility = args["compatibility"].as_str().map(PathBuf::from);
                let id = self.scheduler.hold(
                    pool,
                    owner,
                    label,
                    compatibility,
                    args["key"].as_str().map(str::to_string).unwrap_or(util::uuid_v4()?),
                )?;
                self.scheduler.tick();
                Ok(serde_json::json!({"id":id}))
            }
            "slot-release" => {
                if let Some(key) = args["key"].as_str() {
                    self.scheduler.cancel_key(key)?
                } else {
                    self.scheduler.release(id()?)?
                };
                Ok(serde_json::json!({"released":true}))
            }
            "start" => {
                let key = args["key"]
                    .as_str()
                    .filter(|k| k.len() <= 128 && !k.is_empty())
                    .context("start key is required")?;
                if let Some(id) = self.scheduler.launched.get(key).copied() {
                    if id != 0 {
                        self.scheduler.save()?;
                    }
                    return Ok(if id == 0 {
                        serde_json::json!({"held":true,"reason":"start outcome is uncertain; inspect the original host before retrying"})
                    } else {
                        serde_json::json!({"session":id})
                    });
                }
                let provider: Tool = serde_json::from_value(args["provider"].clone())?;
                anyhow::ensure!(provider.is_agent(), "agent provider required");
                let fallback = args
                    .get("fallback")
                    .filter(|v| !v.is_null())
                    .map(|v| serde_json::from_value::<Tool>(v.clone()))
                    .transpose()?;
                anyhow::ensure!(fallback.is_none_or(Tool::is_agent), "fallback must be an agent provider");
                anyhow::ensure!(self.scheduler.launched.len() < 10000, "start receipt capacity reached");
                let now = util::now_secs();
                let (tool, account, root, held, reset) =
                    self.scheduler_choice(provider, fallback, args["account"].as_str(), now)?;
                if held {
                    return Ok(
                        serde_json::json!({"held":true,"reason":if reset.is_some(){"quota"}else{"quota: reset unknown"},"reset":reset}),
                    );
                }
                anyhow::ensure!(
                    self.scheduler.last_started.get(tool.key()).is_none_or(|at| util::now_ms().saturating_sub(*at)
                        >= self.scheduler.settings.stagger_seconds.saturating_mul(1000)),
                    "provider start is staggered"
                );
                let cwd = args["cwd"].as_str().map(PathBuf::from);
                anyhow::ensure!(
                    cwd.as_ref().is_none_or(|p| p.is_absolute() && p.is_dir()),
                    "working directory is unavailable on this host"
                );
                let cwd = if let Some(repository) = args.get("repository") {
                    let repository = repository.as_str().context("repository identity must be text")?;
                    Some(
                        self.scheduler
                            .placed_directory(repository, cwd.as_deref().context("placed cwd is required")?)?,
                    )
                } else {
                    cwd
                };
                let place = Placement {
                    account: account.clone(),
                    account_root: root,
                    cwd,
                    name: args["title"].as_str().map(clean_name),
                    prompt: args["prompt"].as_str().map(crate::keys::non_owner_message),
                    ..Placement::default()
                };
                let start = StartModel {
                    model: if tool == provider { args["model"].as_str().map(str::to_string) } else { None },
                    effort: None,
                };
                let id = self
                    .scheduler_launch(key, tool, &start, &place)?
                    .context("start outcome is uncertain; inspect the original host before retrying")?;
                Ok(serde_json::json!({"session":id}))
            }
            "enqueue" => {
                anyhow::ensure!(self.scheduler.error.is_none(), "scheduler recovery required");
                anyhow::ensure!(self.scheduler.starts.len() < 1000, "start queue is full");
                let provider: Tool =
                    serde_json::from_value(args["provider"].clone()).context("provider is required")?;
                anyhow::ensure!(provider.is_agent(), "queue starts require an agent provider");
                let cwd = args["cwd"].as_str().map(str::to_string);
                if let Some(cwd) = &cwd {
                    anyhow::ensure!(
                        Path::new(cwd).is_absolute() && Path::new(cwd).is_dir(),
                        "invalid working directory"
                    );
                }
                let model = args["model"].as_str().map(str::to_string);
                anyhow::ensure!(model.as_deref().is_none_or(valid_model_name), "invalid model");
                let fallback = args
                    .get("fallback")
                    .filter(|v| !v.is_null())
                    .map(|v| serde_json::from_value::<Tool>(v.clone()))
                    .transpose()?;
                anyhow::ensure!(fallback.is_none_or(Tool::is_agent), "fallback must be an agent provider");
                let prompt = args["prompt"].as_str().map(str::to_string);
                anyhow::ensure!(prompt.as_ref().is_none_or(|p| p.len() <= 16000), "prompt exceeds 16000 bytes");
                let id = self.scheduler.id();
                self.scheduler.starts.push(Start {
                    host: args["placement"].as_str().unwrap_or("auto").into(),
                    key: util::uuid_v4()?,
                    target: None,
                    attempted_at: None,
                    account: args["account"].as_str().map(str::to_string),
                    id,
                    provider,
                    model,
                    cwd,
                    title: args["title"].as_str().map(clean_name),
                    prompt,
                    fallback,
                    reason: "queued".into(),
                    reset: None,
                    session: None,
                });
                if let Err(error) = self.scheduler.save() {
                    self.scheduler.starts.pop();
                    return Err(error);
                }
                Ok(serde_json::json!({"id":id}))
            }
            "cancel" => {
                let id = id()?;
                let item = self.scheduler.starts.iter().find(|s| s.id == id).context("no such queued start")?;
                anyhow::ensure!(
                    item.session.is_none() && item.attempted_at.is_none(),
                    "start was already dispatched; its ownership cannot be cancelled locally"
                );
                let count = self.scheduler.starts.len();
                self.scheduler.starts.retain(|s| s.id != id);
                anyhow::ensure!(count != self.scheduler.starts.len(), "no such queued start");
                self.scheduler.save()?;
                Ok(serde_json::json!({"cancelled":true}))
            }
            "rerun-record" | "rerun-dismiss" => {
                let session = args["session"].as_u64().context("session is required")?;
                let s = self.sessions.get(&session).filter(|s| s.alive).context("session is not alive")?;
                let conversation = s.conversation.clone();
                let cwd = args["cwd"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| s.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()))
                    .or_else(|| Some(self.opts.start_dir.to_string_lossy().into_owned()));
                anyhow::ensure!(
                    cwd.as_deref().is_none_or(|p| Path::new(p).is_absolute()),
                    "rerun directory must be absolute"
                );
                let authoritative = args["authoritative"].as_bool().unwrap_or(false)
                    || crate::extensions::conventions::repository_owns(
                        self.opts.config.conventions.memory_reruns.as_ref(),
                        s.tool,
                        s.facts
                            .worktree
                            .as_ref()
                            .map(|w| Path::new(&w.path))
                            .or(s.cwd.as_deref())
                            .unwrap_or(&self.opts.start_dir),
                    );
                let commands: Vec<&str> = if let Some(list) = args["commands"].as_array() {
                    anyhow::ensure!(list.len() <= 50, "at most fifty reruns");
                    list.iter().map(|v| v.as_str().context("command must be text")).collect::<Result<_>>()?
                } else {
                    vec![args["command"].as_str().context("command is required")?]
                };
                anyhow::ensure!(
                    commands.iter().all(|c| !c.trim().is_empty() && c.len() <= 4000),
                    "bounded original command is required"
                );
                let wrappers = &self.opts.config.conventions.command_wrappers;
                anyhow::ensure!(
                    authoritative || commands.iter().all(|command| monitor::safe_rerun(command, wrappers)),
                    "only safe original test/build commands can be offered for replay"
                );
                if action == "rerun-dismiss" {
                    let before = self.scheduler.reruns.len();
                    for command in &commands {
                        self.scheduler
                            .memory_dismissed
                            .insert(crate::scheduler::memory_key(session, command), util::now_secs());
                    }
                    self.scheduler.reruns.retain(|r| r.session != session || !commands.contains(&r.command.as_str()));
                    self.scheduler.save()?;
                    return Ok(serde_json::json!({"dismissed":before-self.scheduler.reruns.len()}));
                }
                let mut ids = Vec::new();
                for command in commands {
                    if let Some(existing) =
                        self.scheduler.reruns.iter_mut().find(|r| r.session == session && r.command == command)
                    {
                        existing.authoritative |= authoritative;
                        ids.push(existing.id);
                        continue;
                    }
                    anyhow::ensure!(self.scheduler.reruns.len() < 1000, "rerun list is full");
                    let id = self.scheduler.id();
                    self.scheduler.reruns.push(Rerun {
                        cwd: cwd.clone(),
                        conversation: conversation.clone(),
                        id,
                        session,
                        command: command.into(),
                        at: util::now_secs(),
                        authoritative,
                    });
                    ids.push(id);
                }
                self.scheduler.save()?;
                Ok(if args["commands"].is_array() {
                    serde_json::json!({"ids":ids})
                } else {
                    serde_json::json!({"id":ids[0]})
                })
            }
            "rerun" => {
                anyhow::ensure!(self.scheduler.settings.probe.read().memory_free(), "memory is still short");
                let id = id()?;
                let r = self.scheduler.reruns.iter().find(|r| r.id == id).context("no pending rerun")?.clone();
                anyhow::ensure!(
                    monitor::safe_rerun(&r.command, &self.opts.config.conventions.command_wrappers),
                    "original command is unsafe for replay"
                );
                anyhow::ensure!(!r.authoritative, "repository memory rerun hook owns this command");
                anyhow::ensure!(!self.automatic_input_pending(r.session), "session has pending input");
                let session = self.sessions.get(&r.session).filter(|s| s.alive).context("session ended")?;
                anyhow::ensure!(session.conversation == r.conversation, "rerun belongs to another conversation");
                anyhow::ensure!(
                    session.native_idle(util::now_ms()),
                    "session must be idle with no pending question or unsent draft"
                );
                let text = crate::keys::non_owner_message(&format!(
                    "Memory is free. Rerun this original verification in its original directory {}. It grants no merge, push, CI or deployment authority. Original command (literal):\n{}",
                    r.cwd.as_deref().unwrap_or("the session worktree"),r.command
                ));
                // The rerun stays listed until the agent took its message.
                match self.rerun_message(id, r.session, &text) {
                    None => Ok(serde_json::json!({"submitting":true})),
                    Some(Ok(())) => {
                        self.scheduler.reruns.retain(|r| r.id != id);
                        self.scheduler.save()?;
                        Ok(serde_json::json!({"delivered":true}))
                    }
                    Some(Err(error)) => bail!("the rerun was not delivered and stays listed: {error}"),
                }
            }
            _ => bail!("unknown scheduler action"),
        }
    }
    /// A memory rerun's message the provider answered after `rerun` returned:
    /// taken, the rerun is done; not taken, it stays listed and the owner is
    /// told.
    pub(super) fn rerun_settled(&mut self, rerun: u64, outcome: Result<(), String>) {
        match outcome {
            Ok(()) => {
                self.scheduler.reruns.retain(|r| r.id != rerun);
                if let Err(error) = self.scheduler.save() {
                    self.log_error(&format!(
                        "A delivered memory rerun could not be removed from the saved list: {error:#}"
                    ));
                }
            }
            Err(error) => self.announce_error(format!("The memory rerun was not delivered and stays listed: {error}")),
        }
    }
    pub(super) fn scheduler_choice(
        &self,
        provider: Tool,
        fallback: Option<Tool>,
        account: Option<&str>,
        now: u64,
    ) -> Result<SchedulerChoice> {
        let settings = crate::providers::accounts::Settings::from_config(&self.opts.config)?;
        let identities = profile_identities(&settings);
        let mut primary = None;
        let ready = self.launchable_tools();
        for tool in std::iter::once(provider).chain(fallback.filter(|other| *other != provider)) {
            if !ready.iter().any(|key| key == tool.key()) {
                continue;
            }
            let rotation = rotation_accounts(&settings, tool, &identities);
            let mut choices: Vec<Option<&str>> = if tool == provider && account.is_some() {
                vec![account]
            } else if rotation.is_empty() {
                vec![None]
            } else {
                rotation.into_iter().map(Some).collect()
            };
            if account.is_none() || tool != provider {
                let count = choices.len();
                let at = self.scheduler.account_cursor.get(tool.key()).copied().unwrap_or(0) % count;
                choices.rotate_left(at);
            }
            let default_root = match tool {
                Tool::Claude => Some(self.opts.claude_dir.as_path()),
                Tool::Codex => Some(self.opts.codex_dir.as_path()),
                _ => None,
            };
            let login_tool = match tool {
                Tool::Claude => Some(crate::hosts::logins::LoginTool::Claude),
                Tool::Codex => Some(crate::hosts::logins::LoginTool::Codex),
                _ => None,
            };
            let default_identity = login_tool.and_then(|tool| {
                crate::hosts::logins::state(
                    &crate::hosts::logins::LoginPaths {
                        claude_dir: self.opts.claude_dir.clone(),
                        codex_dir: self.opts.codex_dir.clone(),
                    },
                    tool,
                    crate::hosts::logins::now_ms(),
                )
                .account
            });
            for name in choices {
                let profile = name.map(|name| settings.profile(name, tool)).transpose()?;
                let identity = name.and_then(|name| identities.get(name)).and_then(|id| id.as_deref());
                let includes_default = profile.is_none()
                    || profile.is_some_and(|profile| {
                        default_root.is_some_and(|root| same_account_root(root, &profile.directory))
                            || identity.is_some() && identity == default_identity.as_deref()
                    });
                let identity = identity.or_else(|| includes_default.then_some(default_identity.as_deref()).flatten());
                let aliases: Vec<&str> = settings
                    .profiles
                    .iter()
                    .filter(|(id, profile)| {
                        profile.provider == tool
                            && (Some(id.as_str()) == name
                                || identity.is_some()
                                    && identities.get(id.as_str()).and_then(|id| id.as_deref()) == identity
                                || includes_default
                                    && default_root.is_some_and(|root| same_account_root(root, &profile.directory)))
                    })
                    .map(|(id, _)| id.as_str())
                    .collect();
                let mut windows: Vec<_> = aliases.iter().flat_map(|id| settings.profiles[*id].usage(now)).collect();
                if includes_default {
                    windows.extend(match tool {
                        Tool::Claude => self.usage.claude.clone(),
                        Tool::Codex => self.usage.codex.clone(),
                        _ => Vec::new(),
                    });
                }
                let usage = match tool {
                    Tool::Claude => Usage { claude: windows, ..Usage::default() },
                    Tool::Codex => Usage { codex: windows, ..Usage::default() },
                    _ => Usage::default(),
                };
                let root = profile.map(|profile| profile.directory.clone());
                let mut hold =
                    provider_automation::quota_hold(tool, &usage, self.scheduler.settings.quota_percent, now);
                for session in self.sessions.values().filter(|s| {
                    s.alive
                        && s.tool == tool
                        && (includes_default && s.account.is_none() && s.account_root.is_none()
                            || s.account.as_deref().is_some_and(|id| {
                                aliases.contains(&id)
                                    && settings.profile(id, tool).is_ok_and(|profile| {
                                        s.account_root
                                            .as_ref()
                                            .is_some_and(|root| same_account_root(root, &profile.directory))
                                    })
                            }))
                }) {
                    if session.retry.quota_unknown {
                        hold = Some(None);
                        break;
                    }
                    if let Some(reset) = session.retry.quota_reset.filter(|at| *at > now) {
                        hold = match hold {
                            Some(None) => Some(None),
                            Some(Some(at)) => Some(Some(at.max(reset))),
                            None => Some(Some(reset)),
                        };
                    }
                }
                let choice = (tool, name.map(str::to_string), root, hold.is_some(), hold.flatten());
                if hold.is_none() {
                    return Ok(choice);
                }
                if primary.is_none() {
                    primary = Some(choice)
                }
            }
        }
        primary.context("no provider account is configured")
    }
    /// Write a fail-closed receipt before a process can exist. A zero receipt
    /// survives restart if the process/result boundary cannot be proven later.
    fn scheduler_launch(
        &mut self,
        key: &str,
        tool: Tool,
        start: &StartModel,
        place: &Placement,
    ) -> Result<Option<u64>> {
        if let Some(id) = self.scheduler.launched.get(key).copied() {
            if id != 0 {
                self.scheduler.save()?;
            }
            return Ok((id != 0).then_some(id));
        }
        anyhow::ensure!(self.scheduler.settings.probe.read().admits(), "insufficient headroom");
        anyhow::ensure!(self.scheduler.launched.len() < 10000, "start receipt capacity reached");
        self.scheduler.launched.insert(key.into(), 0);
        self.scheduler.save()?;
        let place = Placement { resource_managed: true, ..place.clone() };
        let id = self.create_with(tool, None, start, &place)?;
        self.scheduler.launched.insert(key.into(), id);
        self.scheduler_started(tool, place.account.as_deref(), util::now_secs());
        self.scheduler.save()?;
        Ok(Some(id))
    }

    fn scheduler_checkpoint(&mut self, keep: &[Start], item: &Start, remaining: &[Start]) -> Result<()> {
        self.scheduler.starts =
            keep.iter().cloned().chain(std::iter::once(item.clone())).chain(remaining.iter().cloned()).collect();
        self.scheduler.save()
    }

    pub(super) fn scheduler_started(&mut self, tool: Tool, account: Option<&str>, _now: u64) {
        self.scheduler.last_started.insert(tool.key().into(), util::now_ms());
        if let Ok(settings) = crate::providers::accounts::Settings::from_config(&self.opts.config) {
            let profiles = rotation_accounts(&settings, tool, &profile_identities(&settings));
            if let Some(index) = account.and_then(|id| profiles.iter().position(|p| *p == id)) {
                self.scheduler.account_cursor.insert(tool.key().into(), index + 1);
            }
        }
    }
    pub(super) fn scheduler_tick(&mut self) {
        let now = util::now_secs();
        if self.scheduler.last_poll == now {
            return;
        }
        self.scheduler.last_poll = now;
        self.scheduler.tick();
        if self.scheduler.error.is_some() || self.shutdown_at.is_some() || self.updating.is_some() {
            return;
        }
        let mut starts = std::mem::take(&mut self.scheduler.starts).into_iter();
        let mut keep = Vec::new();
        while let Some(mut item) = starts.next() {
            if item.session.is_some() {
                keep.push(item);
                continue;
            }
            if item.attempted_at.is_some_and(|at| now.saturating_sub(at) < 1) {
                keep.push(item);
                continue;
            }
            let preferred = item.target.as_deref().unwrap_or(&item.host);
            let target = self
                .workspace_host(item.cwd.as_deref().map(Path::new), preferred, Some(item.provider))
                .or_else(|error| match item.fallback {
                    Some(tool) => self.workspace_host(item.cwd.as_deref().map(Path::new), preferred, Some(tool)),
                    None => Err(error),
                });
            let (host, cwd) = match target {
                Ok(target) => target,
                Err(error) => {
                    item.reason = format!("placement: {error:#}");
                    keep.push(item);
                    continue;
                }
            };
            if let Some(host) = host {
                let Some((repository, _)) =
                    self.scheduler.repository_for(item.cwd.as_deref().map(Path::new).unwrap_or(&self.opts.start_dir))
                else {
                    item.reason = "placement repository is unavailable".into();
                    keep.push(item);
                    continue;
                };
                let args = serde_json::json!({"provider":item.provider,"fallback":item.fallback,"key":item.key,"cwd":cwd,"repository":repository,"model":item.model,"title":item.title,"prompt":item.prompt,"account":item.account});
                if item.attempted_at.is_none() {
                    self.reserve_placement(Some(host.clone()));
                }
                item.target = Some(host.clone());
                item.attempted_at = Some(now);
                let saved = self.scheduler_checkpoint(&keep, &item, starts.as_slice());
                self.scheduler.starts.clear();
                if let Err(error) = saved {
                    item.reason = format!("dispatch receipt: {error:#}");
                    keep.push(item);
                    continue;
                }
                item.reason = match self.scheduler_remote_start(&host, item.id, args) {
                    Ok(()) => "starting on peer".into(),
                    Err(e) => format!("placement: {e:#}"),
                };
                keep.push(item);
                continue;
            }
            let choice = self.scheduler_choice(item.provider, item.fallback, item.account.as_deref(), now);
            let (tool, account, root, held, reset) = match choice {
                Ok(choice) => choice,
                Err(e) => {
                    item.reason = format!("accounts: {e:#}");
                    keep.push(item);
                    continue;
                }
            };
            item.reset = reset;
            if held {
                item.reason = if reset.is_some() { "quota" } else { "quota: reset unknown" }.into();
                keep.push(item);
                continue;
            }
            if self.scheduler.last_started.get(tool.key()).is_some_and(|at| {
                util::now_ms().saturating_sub(*at) < self.scheduler.settings.stagger_seconds.saturating_mul(1000)
            }) {
                item.reason = "stagger".into();
                keep.push(item);
                continue;
            }
            let place = Placement {
                account: account.clone(),
                account_root: root,
                cwd: Some(cwd),
                name: item.title.clone(),
                prompt: item.prompt.as_deref().map(crate::keys::non_owner_message),
                ..Placement::default()
            };
            let model =
                StartModel { model: if tool == item.provider { item.model.clone() } else { None }, effort: None };
            // Native dispatch needs the same sticky destination and durable
            // receipt as peer dispatch; Auto must not move it after a crash.
            item.target = Some("@here".into());
            item.attempted_at = Some(now);
            let outcome = self
                .scheduler_checkpoint(&keep, &item, starts.as_slice())
                .and_then(|()| self.scheduler_launch(&item.key, tool, &model, &place));
            self.scheduler.starts.clear();
            match outcome {
                Ok(Some(id)) => {
                    item.session = Some(id);
                    item.account = account.clone();
                    item.reason = "started".into();
                }
                Ok(None) => {
                    item.reason = "start outcome is uncertain; inspect the original host before retrying".into()
                }
                Err(e) => item.reason = format!("start failed: {e:#}"),
            };
            keep.push(item);
        }
        self.scheduler.starts = keep;
        let _ = self.scheduler.save();
    }
}

fn same_account_root(a: &Path, b: &Path) -> bool {
    a == b || a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b)
}
fn profile_identities(
    settings: &crate::providers::accounts::Settings,
) -> std::collections::BTreeMap<String, Option<String>> {
    settings
        .profiles
        .iter()
        .map(|(id, profile)| {
            (
                id.clone(),
                crate::hosts::logins::state(profile, profile.login_tool(), crate::hosts::logins::now_ms()).account,
            )
        })
        .collect()
}
fn rotation_accounts<'a>(
    settings: &'a crate::providers::accounts::Settings,
    tool: Tool,
    identities: &std::collections::BTreeMap<String, Option<String>>,
) -> Vec<&'a str> {
    let mut known = std::collections::BTreeSet::new();
    settings
        .rotation_profiles(tool)
        .into_iter()
        .filter(|id| {
            identities.get(*id).and_then(|id| id.as_ref()).is_none_or(|identity| known.insert(identity.clone()))
        })
        .collect()
}
