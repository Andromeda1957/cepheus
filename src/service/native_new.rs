//! Fresh native creation is an API operation followed by one identity-bound TUI.
use super::*;

pub enum Prepared {
    Codex(crate::providers::messages::new_thread::Prepared),
    OpenCode(crate::providers::messages::opencode_new::Prepared),
}

impl Prepared {
    fn conversation(&self) -> &str {
        match self {
            Self::Codex(p) => &p.conversation,
            Self::OpenCode(p) => &p.conversation,
        }
    }
    fn settings(&self) -> &StartModel {
        match self {
            Self::Codex(p) => &p.settings,
            Self::OpenCode(p) => &p.settings,
        }
    }
    fn verify(self, pid: u32, deadline: Instant, stop: &std::sync::atomic::AtomicBool) -> Result<()> {
        match self {
            Self::Codex(p) => p.verify(pid, deadline, stop),
            Self::OpenCode(p) => p.verify(pid, deadline, stop),
        }
    }
}

pub(super) struct Pending {
    client: u64,
    tool: Tool,
    request: Option<u64>,
    place: Placement,
    command: Vec<String>,
    home: PathBuf,
    deadline: Instant,
}

pub(super) struct Queued {
    client: u64,
    tool: Tool,
    settings: StartModel,
    place: Placement,
    request: Option<u64>,
}

impl Service {
    pub(super) fn start_native_new(
        &mut self,
        client: u64,
        tool: Tool,
        settings: StartModel,
        mut place: Placement,
        request: Option<u64>,
    ) {
        if self.shutdown_at.is_some() || self.native_new_queue.len() >= crate::protocol::MAX_START_BATCH {
            self.native_new_failed(
                client,
                request,
                "Could not start native session: the start queue is full or the service is stopping".into(),
            );
            return;
        }
        // Capture saved launch preferences at submission, before waiting for a
        // preparation slot. Every entry retains its caller's placement/anchor.
        let settings = (|| -> Result<_> {
            if let Some(account) = place.account.as_deref() {
                let accounts = crate::providers::accounts::Settings::from_config(&self.opts.config)?;
                place.account_root = Some(accounts.profile(account, Tool::Codex)?.directory.clone());
            } else if tool == Tool::Codex {
                place.account_root = Some(self.opts.codex_dir.clone());
            }
            place.cwd = Some(place.cwd.as_deref().unwrap_or(&self.opts.start_dir).canonicalize()?);
            self.launch_start(tool, &settings, place.account_root.as_deref())
        })();
        let settings = match settings {
            Ok(settings) => settings,
            Err(error) => {
                self.native_new_failed(client, request, format!("Could not start {}: {error:#}", tool.label()));
                return;
            }
        };
        self.native_new_queue.push_back(Queued { client, tool, settings, place, request });
        self.pump_native_new();
    }

    pub(super) fn pump_native_new(&mut self) {
        loop {
            let Some(next) = self.native_new_queue.front() else { break };
            let abandoned = self.shutdown_at.is_some() || !self.clients.contains_key(&next.client);
            if !abandoned
                && self.native_new_pending.len() + self.placed_pending.len() >= crate::protocol::MAX_PENDING_STARTS
            {
                break;
            }
            let next = self.native_new_queue.pop_front().unwrap();
            if abandoned {
                self.native_new_failed(
                    next.client,
                    next.request,
                    "Could not start native session: the connection closed or service stopped before launch".into(),
                );
            } else {
                self.begin_native_new(next.client, next.tool, next.settings, next.place, next.request);
            }
        }
    }

    fn begin_native_new(
        &mut self,
        client: u64,
        tool: Tool,
        settings: StartModel,
        place: Placement,
        request: Option<u64>,
    ) {
        let prepared = (|| -> Result<_> {
            anyhow::ensure!(matches!(tool, Tool::Codex | Tool::OpenCode), "provider has no native New");
            anyhow::ensure!(
                tool != Tool::Codex
                    || self.clients.get(&client).and_then(|client| client.version).is_some_and(|version| version >= 19),
                "native Codex creation requires a protocol-19 client"
            );
            anyhow::ensure!(
                self.shutdown_at.is_none()
                    && self.native_new_pending.len() + self.placed_pending.len() < crate::protocol::MAX_PENDING_STARTS,
                "too many pending native starts, or the service is stopping"
            );
            anyhow::ensure!(
                place.command.is_empty() && place.args.is_empty() && place.prompt.is_none(),
                "native New cannot use a terminal prompt or extra launch arguments"
            );
            anyhow::ensure!(
                settings.model.as_deref().is_none_or(valid_model_name)
                    && settings.effort.as_deref().is_none_or(valid_effort),
                "invalid native model or effort"
            );
            if let Some(account) = place.account.as_deref() {
                anyhow::ensure!(tool == Tool::Codex, "provider does not support native accounts");
                let accounts = crate::providers::accounts::Settings::from_config(&self.opts.config)?;
                anyhow::ensure!(
                    place.account_root.as_ref() == Some(&accounts.profile(account, Tool::Codex)?.directory),
                    "account directory changed while native New was queued"
                );
            } else if tool == Tool::Codex {
                anyhow::ensure!(
                    place.account_root.as_ref() == Some(&self.opts.codex_dir),
                    "default account directory changed while native New was queued"
                );
            }
            let home = place.account_root.clone().unwrap_or_else(|| self.opts.codex_dir.clone());
            let cwd = place.cwd.as_deref().unwrap_or(&self.opts.start_dir).canonicalize()?;
            anyhow::ensure!(cwd.is_dir(), "native New directory is unavailable");
            anyhow::ensure!(place.cwd.as_ref() == Some(&cwd), "native New directory changed while queued");
            let bytes = crate::remote::auth::random_bytes(8)?;
            let nonce = u64::from_le_bytes(bytes.try_into().unwrap());
            anyhow::ensure!(!self.native_new_pending.contains_key(&nonce), "native New identity allocation collided");
            Ok((nonce, home))
        })();
        let (nonce, home) = match prepared {
            Ok(value) => value,
            Err(error) => {
                self.native_new_failed(client, request, format!("Could not start {}: {error:#}", tool.label()));
                return;
            }
        };
        // Queue time does not consume the provider preparation/attachment budget.
        let deadline = Instant::now() + Duration::from_secs(30);
        let config = self.opts.config.clone();
        let cwd = place.cwd.clone().unwrap();
        let name = place.name.as_deref().map(clean_name).filter(|name| !name.is_empty()).unwrap_or_else(|| {
            if tool == Tool::OpenCode {
                String::new()
            } else {
                tool.label().into()
            }
        });
        self.native_new_pending.insert(
            nonce,
            Pending {
                client,
                tool,
                request,
                place,
                command: match tool {
                    Tool::Codex => config.codex_command.clone(),
                    _ => config.opencode_command.clone(),
                },
                home: home.clone(),
                deadline,
            },
        );
        let stop = self.extensions.cancel.clone();
        let worker = crate::extensions::WorkGuard::new(self.extensions.workers.clone());
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _worker = worker;
            let result = match tool {
                Tool::Codex => crate::providers::messages::new_thread::prepare(
                    &config, &cwd, &home, &settings, &name, deadline, &stop,
                )
                .map(Prepared::Codex),
                Tool::OpenCode => crate::providers::messages::opencode_new::prepare(
                    &config, &cwd, &home, &settings, &name, deadline, &stop,
                )
                .map(Prepared::OpenCode),
                _ => unreachable!(),
            }
            .map(Box::new)
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(Ev::NativeNewPrepared { nonce, result });
        });
    }

    pub(super) fn native_new_prepared(&mut self, nonce: u64, result: Result<Box<Prepared>, String>) {
        let Some(pending) = self.native_new_pending.remove(&nonce) else { return };
        let fail = |service: &mut Service, error: String| {
            service.native_new_failed(
                pending.client,
                pending.request,
                format!("Could not start {}: {error}", pending.tool.label()),
            )
        };
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                fail(self, error);
                return;
            }
        };
        if !self.clients.contains_key(&pending.client) {
            self.native_clear_failed(pending.client, pending.request, "the connection closed");
            return;
        }
        let unchanged = self.shutdown_at.is_none()
            && Instant::now() < pending.deadline
            && match pending.tool {
                Tool::Codex => self.opts.config.codex_command == pending.command && !self.opts.config.codex_notify,
                Tool::OpenCode => self.opts.config.opencode_command == pending.command,
                _ => false,
            }
            && pending.place.cwd.as_ref().is_some_and(|cwd| cwd.canonicalize().ok().as_ref() == Some(cwd))
            && (pending.tool != Tool::Codex
                || pending.place.account.as_deref().map_or_else(
                    || self.opts.codex_dir == pending.home,
                    |account| {
                        crate::providers::accounts::Settings::from_config(&self.opts.config).ok().and_then(|accounts| {
                            accounts.profile(account, Tool::Codex).ok().map(|profile| profile.directory.clone())
                        }) == Some(pending.home.clone())
                    },
                ));
        if !unchanged {
            fail(self, "launch configuration changed while native New was preparing; no client was launched".into());
            return;
        }
        let mut place = pending.place;
        if let Prepared::Codex(codex) = prepared.as_ref() {
            place.native_program = Some(codex.program.clone());
            place.native_environment = codex.environment.clone();
        }
        let id = match self.create_with(pending.tool, Some(prepared.conversation().into()), prepared.settings(), &place)
        {
            Ok(id) => id,
            Err(error) => {
                self.native_new_failed(
                    pending.client,
                    pending.request,
                    format!("Could not open the prepared native thread: {error:#}"),
                );
                return;
            }
        };
        self.reserve_placement(None);
        // This identity belongs to a newly created empty backend, never an adopted
        // owner's draft. Resuming it is only the first interface attachment.
        self.sessions.get_mut(&id).unwrap().owner_draft = false;
        let target = provider_placement::PlacedTarget::new(&self.sessions[&id], nonce, pending.client).unwrap();
        let Some(pid) = target.expected.pid else {
            self.close_and_remove(id);
            self.native_new_failed(
                pending.client,
                pending.request,
                "the fresh native client has no process identity".into(),
            );
            return;
        };
        self.placed_pending.insert(id, target);
        let tx = self.tx.clone();
        let worker = crate::extensions::WorkGuard::new(self.extensions.workers.clone());
        let stop = self.sessions.get_mut(&id).unwrap().chat.control_cancel();
        let deadline = pending.deadline;
        thread::spawn(move || {
            let _worker = worker;
            let result = prepared.verify(pid, deadline, &stop).map_err(|error| format!("{error:#}"));
            let _ = tx.send(Ev::NativeNewReady { client: pending.client, id, request: pending.request, nonce, result });
        });
    }

    pub(super) fn native_new_ready(
        &mut self,
        client: u64,
        id: u64,
        request: Option<u64>,
        nonce: u64,
        result: Result<(), String>,
    ) {
        let Some(pending) =
            self.placed_pending.get(&id).filter(|pending| pending.nonce == nonce && pending.owner == client)
        else {
            return;
        };
        let unchanged = self.sessions.get(&id).is_some_and(|session| pending.matches(session, nonce));
        self.placed_pending.remove(&id);
        if unchanged && result.is_ok() && self.shutdown_at.is_none() {
            if !self.native_clear_created(client, request, id) {
                self.send(client, &ServerMsg::Created { id, request });
            }
        } else {
            if unchanged {
                self.close_and_remove(id);
            }
            self.native_new_failed(
                client,
                request,
                format!(
                    "Fresh native binding was not confirmed: {}",
                    result.err().unwrap_or_else(|| "the client identity changed".into())
                ),
            );
        }
        self.last_status = Instant::now() - STATUS_INTERVAL;
    }
}
