//! A delayed phone operation retains its original connection and executor target.
use super::*;
use crate::attention::{store::Target, ExpectedTarget};

#[derive(Clone, Debug)]
pub(super) struct TargetLease {
    pub id: u64,
    pub local_id: u64,
    pub host: Option<String>,
    pub tool: Tool,
    pub expected: ExpectedTarget,
    pub generation: u64,
    notice_prefix: Option<String>,
}

impl TargetLease {
    fn from_session(info: &SessionInfo, generation: u64) -> Result<Self> {
        let expected = ExpectedTarget::from_session(info)
            .context("This session has no verified process identity; update its Cepheus service before sending delayed controls.")?;
        anyhow::ensure!((split_id(info.id).0 == 0) == info.host.is_none(), "the session host route is unavailable");
        Ok(Self {
            id: info.id,
            local_id: split_id(info.id).1,
            host: info.host.clone(),
            tool: info.tool,
            expected,
            generation,
            notice_prefix: None,
        })
    }

    pub(super) fn matches(&self, info: &SessionInfo) -> bool {
        info.alive
            && info.id == self.id
            && info.tool == self.tool
            && info.host == self.host
            && self.expected.matches(&Target {
                session: info.id,
                spawned_ms: info.spawned_ms,
                pid: info.pid,
                conversation: info.conversation.clone(),
            })
    }

    pub(super) fn message(&self, request: u64, operation: &str, mut args: Value) -> ClientMsg {
        args["id"] = json!(self.local_id);
        args["host"] = json!(self.host);
        args["expected"] = json!(self.expected);
        args["operation"] = json!(operation);
        ClientMsg::Plugin { request, plugin: format!("provider-{}", self.tool.key()), command: "target".into(), args }
    }
}

impl Hub {
    pub(super) fn finish_create(
        self: &Arc<Self>,
        id: u64,
        tool: Tool,
        model: Option<String>,
        effort: Option<String>,
        text: Option<String>,
        marker: (u64, u64),
    ) -> Result<()> {
        // The service's Created receipt already verifies native New settings.
        // Existing-session Model remains a separate explicit native operation.
        let _ = (model, effort);
        if text.is_none() {
            return self.wait_for_session_state(id, marker);
        }
        let anchor = self.wait_for_created_target(id, tool, marker)?;
        let info = self.target_session(&anchor)?;
        let who = crate::protocol::session_label(info.tool, &info.title);
        let hub = Arc::clone(self);
        thread::spawn(move || {
            let outcome = hub.when_target_ready(&anchor).and_then(|lease| {
                if let Some(text) = text {
                    hub.type_when_target_ready(&lease, &text)?;
                }
                Ok(())
            });
            if let Err(error) = outcome {
                hub.push_all(&json!({"t":"notice","text":format!("{who}: {error:#}"),"error":true}).to_string());
            }
        });
        Ok(())
    }

    pub(super) fn target_lease(&self, id: u64) -> Result<TargetLease> {
        let inner = lock(&self.inner);
        anyhow::ensure!(inner.service_ready, "The Cepheus service is not ready; waiting for its session state.");
        Self::target_lease_in(&inner, id)
    }

    pub(super) fn target_lease_marker(&self, id: u64) -> Result<(TargetLease, (u64, u64))> {
        let inner = lock(&self.inner);
        anyhow::ensure!(inner.service_ready, "The Cepheus service is not ready; waiting for its session state.");
        Ok((Self::target_lease_in(&inner, id)?, (inner.service_generation, inner.state_revision)))
    }

    fn target_lease_in(inner: &Inner, id: u64) -> Result<TargetLease> {
        let info = inner
            .state
            .as_ref()
            .and_then(|state| state.sessions.iter().find(|s| s.id == id))
            .context("no such session")?;
        let mut lease = TargetLease::from_session(info, inner.service_generation)?;
        if let Some(host) = &lease.host {
            let host = inner.hosts.iter().find(|h| &h.name == host).context("unknown host")?;
            lease.notice_prefix = Some(format!("{}: ", host.label));
        }
        Ok(lease)
    }

    pub(super) fn wait_for_created_target(&self, id: u64, tool: Tool, marker: (u64, u64)) -> Result<TargetLease> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut observed: Option<SessionInfo> = None;
        loop {
            {
                let inner = lock(&self.inner);
                anyhow::ensure!(
                    inner.service_ready && inner.service_generation == marker.0,
                    "The Cepheus service connection changed; check the session before retrying."
                );
                if inner.state_revision > marker.1 {
                    let info =
                        inner.state.as_ref().and_then(|state| state.sessions.iter().find(|session| session.id == id));
                    if let Some(info) = info {
                        anyhow::ensure!(
                            info.alive && info.tool == tool,
                            "the newly created session changed before it was ready"
                        );
                        if let Some(previous) = &observed {
                            anyhow::ensure!(
                                info.host == previous.host
                                    && (previous.spawned_ms == 0 || info.spawned_ms == previous.spawned_ms)
                                    && (previous.pid.is_none() || info.pid == previous.pid)
                                    && (previous.conversation.is_none() || info.conversation == previous.conversation),
                                "the newly created session identity changed before it was ready"
                            );
                        }
                        if ExpectedTarget::from_session(info).is_some() {
                            return Self::target_lease_in(&inner, id);
                        }
                        observed = Some(info.clone());
                    } else {
                        anyhow::ensure!(
                            observed.is_none(),
                            "the newly created session disappeared before it was ready"
                        );
                    }
                }
            }
            anyhow::ensure!(Instant::now() < deadline, "the new session has not reached the shared view yet");
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Initial native-ID publication may enrich a just-created process while it
    /// starts. Once learned, that identity cannot be replaced by another one.
    pub(super) fn when_target_ready(&self, anchor: &TargetLease) -> Result<TargetLease> {
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut lease = anchor.clone();
        loop {
            {
                let inner = lock(&self.inner);
                anyhow::ensure!(
                    inner.service_ready && inner.service_generation == lease.generation,
                    "The Cepheus service connection changed; the delayed operation was held."
                );
                let info = inner
                    .state
                    .as_ref()
                    .and_then(|state| state.sessions.iter().find(|s| s.id == lease.id))
                    .context("the original session is no longer available")?;
                if lease.expected.conversation.is_none() {
                    lease.expected.conversation = info.conversation.clone();
                }
                anyhow::ensure!(lease.matches(info), "The session identity changed; the delayed operation was held.");
                if !matches!(info.tool, Tool::Codex | Tool::Claude | Tool::OpenCode)
                    || lease.expected.conversation.is_some()
                {
                    return Ok(lease);
                }
            }
            anyhow::ensure!(Instant::now() < deadline, "the program has not published a ready session identity");
            thread::sleep(Duration::from_millis(300));
        }
    }

    pub(super) fn type_when_target_ready(&self, lease: &TargetLease, text: &str) -> Result<()> {
        self.target_send(lease, text)
    }

    fn request_target(
        &self,
        lease: &TargetLease,
        message: &ClientMsg,
        want: Want,
        wait: Duration,
    ) -> Result<ServerMsg> {
        let _one = (!matches!(want, Want::TargetMessage(..))).then(|| lock(&self.request_lock));
        let source = ReplySource { tag: Some(split_id(lease.id).0), notice_prefix: lease.notice_prefix.clone() };
        let (tx, rx) = mpsc::channel();
        let waiter = self.next();
        lock(&self.waiters).push(Waiter { id: waiter, want, source, tx });
        let result = (|| {
            let mut service = lock(&self.service);
            let inner = lock(&self.inner);
            Self::target_session_in(&inner, lease)?;
            if matches!(want, Want::TargetMessage(..)) {
                let required = if matches!(message, ClientMsg::Plugin { args, .. } if args["client_id"].is_string()) {
                    21
                } else {
                    crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL
                };
                anyhow::ensure!(
                    inner.service_version >= required,
                    "Update needed: direct agent messages require Cepheus protocol {required}; nothing was sent"
                );
            }
            drop(inner);
            write_msg(service.as_mut().context("the Cepheus service is not running")?, message)
                .context("the Cepheus service connection broke")?;
            drop(service);
            rx.recv_timeout(wait).context("the guarded operation did not answer; check the session before retrying")
        })();
        lock(&self.waiters).retain(|w| w.id != waiter);
        result
    }

    pub(super) fn target_session(&self, lease: &TargetLease) -> Result<SessionInfo> {
        let inner = lock(&self.inner);
        Self::target_session_in(&inner, lease)
    }

    pub(super) fn target_session_in(inner: &Inner, lease: &TargetLease) -> Result<SessionInfo> {
        anyhow::ensure!(
            inner.service_ready && inner.service_generation == lease.generation,
            "The Cepheus service connection changed; check the session before retrying."
        );
        let info = inner
            .state
            .as_ref()
            .and_then(|state| state.sessions.iter().find(|s| s.id == lease.id))
            .context("The original session is no longer available.")?;
        anyhow::ensure!(lease.matches(info), "The session identity changed; the delayed operation was held.");
        Ok(info.clone())
    }

    pub(super) fn target_send(&self, lease: &TargetLease, text: &str) -> Result<()> {
        self.target_send_with_attachments(lease, text, false, &[], None).map(|_| ())
    }

    pub(super) fn target_send_with_attachments(
        &self,
        lease: &TargetLease,
        text: &str,
        attachments: bool,
        images: &[(std::path::PathBuf, String)],
        client_id: Option<&str>,
    ) -> Result<Option<crate::protocol::Delivered>> {
        let request = self.next();
        let mut args = json!({"text":text,"attachments":attachments});
        if let Some(client_id) = client_id {
            args["client_id"] = json!(client_id);
        }
        if !images.is_empty() {
            args["images"] = json!(images);
        }
        let reply = self.request_target(
            lease,
            &lease.message(request, "send", args),
            Want::TargetMessage(lease.id, request),
            Duration::from_secs(40),
        )?;
        match reply {
            ServerMsg::MessageSent { error: None, delivered, .. } => Ok(delivered),
            ServerMsg::MessageSent { error: Some(error), .. } | ServerMsg::PluginResult { error: Some(error), .. } => {
                bail!("{error}")
            }
            _ => bail!("delivery status is unknown; check the conversation before retrying"),
        }
    }

    /// The phone's Clear: the session's own host starts a fresh native
    /// session in its place and closes the cleared one (`native_clear`);
    /// nothing is typed into a terminal.
    pub(super) fn target_clear(&self, lease: &TargetLease, marker: (u64, u64)) -> Result<Option<u64>> {
        let request = self.next();
        let message = lease.message(request, "clear", json!({}));
        let unknown = "clear status is unknown; check the session before retrying";
        let fresh = match self.request_target(lease, &message, Want::Plugin(request), Duration::from_secs(60))? {
            ServerMsg::PluginResult { error: Some(error), .. } => bail!("{error}"),
            ServerMsg::PluginResult { data, .. } => data["id"].as_u64().context(unknown)?,
            _ => bail!(unknown),
        };
        let replacement = crate::protocol::host_id(split_id(lease.id).0, fresh);
        self.wait_for_created_target(replacement, lease.tool, marker)?;
        if let Err(error) = self.keep_place(lease.id, replacement) {
            let text = format!("The fresh session could not take the cleared one's place: {error:#}");
            self.push_all(&json!({"t":"notice","text":text,"error":true}).to_string());
        }
        Ok(Some(replacement))
    }

    /// The fresh session takes the cleared one's place in the shared order,
    /// as a session cleared by typing `/clear` keeps it.
    fn keep_place(&self, cleared: u64, fresh: u64) -> Result<()> {
        let state = lock(&self.inner).state.clone().context("the session state is unavailable")?;
        let order = self.order_for(&state);
        let place = |id: u64| order.iter().position(|o| *o == id).unwrap_or(usize::MAX);
        let mut listed: Vec<_> = state.sessions.iter().collect();
        listed.sort_by_key(|s| (place(s.id), s.created_at, s.id));
        let mut seen = std::collections::HashSet::new();
        let ids: Vec<u64> = order
            .iter()
            .copied()
            .chain(listed.iter().map(|s| s.id))
            .map(|id| if id == cleared { fresh } else { id })
            .filter(|id| state.sessions.iter().any(|s| s.id == *id) && seen.insert(*id))
            .collect();
        self.reorder(&ids)
    }

    pub(super) fn switch_model_at(&self, lease: &TargetLease, model: Option<&str>, effort: Option<&str>) -> Result<()> {
        let model = model.filter(|model| !model.trim().is_empty());
        let effort = effort.filter(|effort| !effort.trim().is_empty());
        anyhow::ensure!(
            model.is_none_or(service::valid_model_name) && effort.is_none_or(service::valid_effort),
            "invalid model or effort"
        );
        anyhow::ensure!(model.is_some() || effort.is_some(), "no model or effort given");
        match lease.tool {
            Tool::Claude => {
                if let Some(model) = model {
                    self.target_send(lease, &format!("/model {model}"))?;
                }
                if let Some(effort) = effort {
                    if model.is_some() {
                        thread::sleep(Duration::from_millis(1500));
                    }
                    self.target_send(lease, &format!("/effort {effort}"))?;
                }
                Ok(())
            }
            Tool::Codex => self.codex_pick_at(lease, model, effort),
            _ => bail!("switch this program's model at the desk"),
        }
    }

    pub(super) fn codex_pick_at(&self, lease: &TargetLease, model: Option<&str>, effort: Option<&str>) -> Result<()> {
        let mut args = json!({"action":"settings"});
        if let Some(model) = model {
            args["model"] = json!(model);
        }
        if let Some(effort) = effort {
            args["effort"] = json!(effort);
        }
        if self.codex_control_at(lease, args)?.is_some() {
            self.target_session(lease)?;
            return Ok(());
        }
        bail!("native model settings are unavailable; no terminal input was sent")
    }

    pub(super) fn codex_control_at(&self, lease: &TargetLease, mut args: Value) -> Result<Option<Value>> {
        self.target_session(lease)?;
        if lease.tool != Tool::Codex
            || lock(&self.inner).service_version < crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL
        {
            return Ok(None);
        }
        let Some(conversation) = &lease.expected.conversation else { return Ok(None) };
        let request = self.next();
        let discovered = target_data(self.request_target(
            lease,
            &ClientMsg::PluginList { request },
            Want::Plugin(request),
            SERVICE_REPLY,
        )?)?;
        if !discovered["plugins"].as_array().is_some_and(|plugins| {
            plugins.iter().any(|plugin| {
                plugin["id"] == "provider-codex"
                    && plugin["enabled"] == true
                    && plugin["commands"]
                        .as_array()
                        .is_some_and(|commands| commands.iter().any(|c| c["name"] == "control"))
            })
        }) {
            return Ok(None);
        }
        args["id"] = json!(lease.local_id);
        args["host"] = json!(lease.host);
        args["conversation"] = json!(conversation);
        args["expected"] = json!(lease.expected);
        // Large existing question answers retain their guarded compatibility path.
        if serde_json::to_vec(&args)?.len() > 16 * 1024 {
            return Ok(None);
        }
        let request = self.next();
        let data = target_data(self.request_target(
            lease,
            &ClientMsg::Plugin { request, plugin: "provider-codex".into(), command: "control".into(), args },
            Want::Plugin(request),
            Duration::from_secs(12),
        )?)?;
        if data["supported"] == false {
            return Ok(None);
        }
        anyhow::ensure!(
            data["supported"] == true && data["accepted"] == true,
            "native control outcome unknown; check the conversation before retrying"
        );
        Ok(Some(data))
    }
}

pub(super) fn target_data(reply: ServerMsg) -> Result<Value> {
    match reply {
        ServerMsg::PluginResult { data, error: None, .. } => Ok(data),
        ServerMsg::PluginResult { data, error: Some(error), .. } if data["code"] == crate::protocol::QUESTION_STALE => {
            Err(crate::protocol::QuestionStale(error).into())
        }
        ServerMsg::PluginResult { error: Some(error), .. } => bail!("{error}"),
        _ => bail!("unexpected guarded control reply"),
    }
}
