//! One persistent plugin runtime owned by the service, independent of clients.
use super::*;
use std::rc::Rc;

use crate::extensions::{self, Command, Plugin, Processes};
use crate::lua::{Engine, JobDone, Places, Request};

#[derive(Default)]
pub(super) struct Runtime {
    pub engine: Option<Engine>,
    pub cancel: Arc<AtomicBool>,
    pub processes: Processes,
    pub workers: Arc<std::sync::atomic::AtomicUsize>,
    pub metadata: BTreeMap<u64, BTreeMap<String, Value>>,
    pub titles: BTreeMap<u64, (String, String)>,
    seen: HashMap<u64, crate::lua::Seen>,
    /// The state the scripts were last given.
    published: Option<Arc<DeckState>>,
    snapshot_at: Option<Instant>,
    /// The service's `state_builds` when the scripts were last given a state.
    snapshot_builds: u64,
    draining: bool,
    pending: BTreeMap<(u64, u64), bool>,
    config: ConfigWatch,
}

/// How often the config file is looked at for plugin settings.
const CONFIG_CHECK: Duration = Duration::from_secs(1);

/// The config file's modification time and length.
type Stamp = (std::time::SystemTime, u64);

/// The config file as the plugins last took it (`upkeep_plugin_config`).
#[derive(Default)]
struct ConfigWatch {
    checked: Option<Instant>,
    /// The file whose plugin settings are in effect.
    applied: Option<Stamp>,
    /// A different file seen at the last look, taken once it stays the same.
    seen: Option<Stamp>,
}

#[path = "../../plugins/native-services.rs"]
pub(super) mod native;

impl Service {
    /// Plugins are turned on and off in the config file while the service runs, with no restart. The file is
    /// looked at once a second and read once it has stayed the same between two looks, so a file still being
    /// written is never taken. A file that does not parse leaves the running settings in place and says why.
    pub(super) fn upkeep_plugin_config(&mut self, now: Instant) {
        let watch = &mut self.extensions.config;
        if watch.checked.is_some_and(|at| now.duration_since(at) < CONFIG_CHECK) {
            return;
        }
        watch.checked = Some(now);
        let Some(path) = self.opts.config_file.clone() else { return };
        let Some(stamp) = std::fs::metadata(&path).ok().and_then(|m| Some((m.modified().ok()?, m.len()))) else {
            return;
        };
        if watch.applied == Some(stamp) {
            watch.seen = None;
            return;
        }
        if watch.seen != Some(stamp) {
            watch.seen = Some(stamp);
            return;
        }
        watch.applied = Some(stamp);
        watch.seen = None;
        let (config, error) = Config::load_from(&path);
        if let Some(error) = error {
            self.announce_error(format!("Plugin settings unchanged: {error}"));
            return;
        }
        if self.take_plugin_settings(config) {
            self.load_service_plugins();
        }
    }

    /// Take the plugin settings of `config`: whether any changed.
    pub(super) fn take_plugin_settings(&mut self, config: Config) -> bool {
        let current = &self.opts.config;
        if (config.lua, &config.plugins, &config.plugin_source_dir, &config.plugin_settings)
            == (current.lua, &current.plugins, &current.plugin_source_dir, &current.plugin_settings)
        {
            return false;
        }
        self.opts.config.lua = config.lua;
        self.opts.config.plugins = config.plugins;
        self.opts.config.plugin_source_dir = config.plugin_source_dir;
        self.opts.config.plugin_settings = config.plugin_settings;
        true
    }

    pub(super) fn load_service_plugins(&mut self) {
        for process in self.extensions.processes.snapshot() {
            if !self.opts.config.lua || self.opts.config.plugins.get(&process.plugin) == Some(&false) {
                let _ = self.extensions.processes.stop(&process.plugin, &process.name);
            }
        }
        self.extensions
            .metadata
            .values_mut()
            .for_each(|m| m.retain(|plugin, _| self.opts.config.plugins.get(plugin) != Some(&false)));
        self.extensions.titles.retain(|_, (plugin, _)| self.opts.config.plugins.get(plugin) != Some(&false));
        if !self.opts.config.lua {
            self.extensions.metadata.clear();
            self.extensions.titles.clear();
        }
        if !self.opts.config.lua || self.opts.config_file.is_none() {
            return;
        }
        let places = Places {
            config_dir: self.opts.config_file.as_ref().unwrap().parent().unwrap_or(Path::new(".")).into(),
            start_dir: self.opts.start_dir.clone(),
        };
        let tx = self.tx.clone();
        match Engine::new_service(
            places,
            Arc::new(move |done| {
                let _ = tx.send(Ev::PluginJob(done));
            }),
        ) {
            Ok(mut engine) => {
                engine.use_source_dir(self.opts.config.plugin_source_dir.clone());
                self.state_changed();
                engine.set_state_source(Rc::new(self.last_state.clone()), None);
                engine.load_service_with(&self.opts.config.plugins);
                self.extensions.engine = Some(engine);
                self.drain_service_plugins();
            }
            Err(error) => self.announce_error(format!("Service plugins: {error}")),
        }
    }

    pub(super) fn service_plugin_commands(&self) -> Vec<Plugin> {
        let mut plugins = extensions::plugins(&self.opts.config.plugins);
        if let Some(engine) = &self.extensions.engine {
            for command in engine.service_commands() {
                if self.opts.config.plugins.get(&command.plugin) == Some(&false) {
                    continue;
                }
                if let Some(plugin) = plugins.iter_mut().find(|p| p.id == command.plugin) {
                    plugin.commands.push(Command {
                        name: command.name,
                        description: command.description,
                        args: command.args,
                    });
                } else {
                    plugins.push(Plugin {
                        id: command.plugin.clone(),
                        name: command.plugin,
                        description: String::new(),
                        enabled: true,
                        commands: vec![Command {
                            name: command.name,
                            description: command.description,
                            args: command.args,
                        }],
                    });
                }
            }
        }
        for provider in &self.opts.config.providers {
            if provider.id == "dot" && provider.validate().is_ok() {
                plugins.push(Plugin {
                    id: "provider-dot".into(),
                    name: provider.label.clone(),
                    description: "Direct cloud Dot messages and images".into(),
                    enabled: self.opts.config.plugins.get("provider-dot") != Some(&false),
                    commands: vec![Command {
                        name: "send".into(),
                        description: "Send to the exact existing Dot controller through its private API".into(),
                        args: vec!["id".into(), "expected".into(), "text".into(), "images".into()],
                    }],
                });
                continue;
            }
            if provider.cloud || provider.validate().is_err() {
                continue;
            }
            let id = format!("provider-{}", provider.id);
            let target = Command {
                name: "target".into(),
                description: "Operate on the exact captured native session".into(),
                args: vec!["id".into(), "expected".into(), "operation".into()],
            };
            if let Some(plugin) = plugins.iter_mut().find(|plugin| plugin.id == id) {
                if !plugin.commands.iter().any(|command| command.name == "target") {
                    plugin.commands.push(target);
                }
            } else {
                plugins.push(Plugin {
                    enabled: self.opts.config.plugins.get(&id) != Some(&false),
                    id,
                    name: provider.label.clone(),
                    description: "Configured PTY provider".into(),
                    commands: vec![target],
                });
            }
        }
        for plugin in &mut plugins {
            if !plugin.enabled {
                plugin.commands.clear();
            }
        }
        plugins
    }

    pub(super) fn handle_plugin(&mut self, client: u64, request: u64, plugin: &str, command: &str, args: &Value) {
        if self.shutdown_at.is_some() {
            self.emit_plugin_reply(client, request, Value::Null, Some("Service is stopping".into()));
            return;
        }
        let allowed = self
            .service_plugin_commands()
            .iter()
            .any(|p| p.id == plugin && p.enabled && p.commands.iter().any(|c| c.name == command));
        if !allowed
            || !args.is_object()
            || serde_json::to_vec(args)
                .map_or(true, |b| b.len() > native::argument_limit(&self.opts.config, plugin, command))
        {
            self.emit_plugin_reply(
                client,
                request,
                Value::Null,
                Some("No such enabled plugin command or invalid arguments".into()),
            );
            return;
        }
        if self.extensions.pending.len() >= 8 || self.extensions.pending.contains_key(&(client, request)) {
            self.emit_plugin_reply(
                client,
                request,
                Value::Null,
                Some("Too many pending plugin commands or duplicate request ID".into()),
            );
            return;
        }
        self.extensions.pending.insert((client, request), false);
        if native::start(self, client, request, plugin, command, args) {
            return;
        }
        self.extensions.pending.remove(&(client, request));
        let result = self.invoke_plugin(plugin, command, args);
        match result {
            Ok(data) => self.plugin_reply(client, request, data, None),
            Err(error) => self.plugin_reply(client, request, Value::Null, Some(error)),
        }
        self.drain_service_plugins();
    }

    pub(super) fn plugin_reply(&mut self, client: u64, request: u64, data: Value, error: Option<String>) {
        if self.extensions.pending.remove(&(client, request)) == Some(true) {
            self.native.installs = self.native.installs.saturating_sub(1);
        }
        self.emit_plugin_reply(client, request, data, error);
    }

    fn emit_plugin_reply(&mut self, client: u64, request: u64, mut data: Value, mut error: Option<String>) {
        if serde_json::to_vec(&data).map_or(true, |b| b.len() > 16 * 1024) {
            data = Value::Null;
            error = Some("Plugin result exceeds 16 KiB".into());
        }
        if client == 0 {
            if request > 0 {
                if let Some(engine) = self.extensions.engine.as_mut() {
                    engine.service_result(request, &data, error.as_deref());
                }
            }
        } else {
            self.send(client, &ServerMsg::PluginResult { request, data, error });
        }
    }

    /// Only the native target screen command may return a full bounded screen.
    /// Other plugin results retain their existing 16 KiB limit.
    pub(super) fn plugin_target_reply(&mut self, client: u64, request: u64, data: Value, error: Option<String>) {
        if serde_json::to_vec(&data).map_or(true, |bytes| bytes.len() > 1024 * 1024) {
            self.plugin_reply(client, request, Value::Null, Some("Target result exceeds 1 MiB".into()));
            return;
        }
        self.extensions.pending.remove(&(client, request));
        if client == 0 {
            self.emit_plugin_reply(client, request, data, error);
        } else {
            self.send(client, &ServerMsg::PluginResult { request, data, error });
        }
    }

    pub(super) fn invoke_plugin(&mut self, plugin: &str, command: &str, args: &Value) -> Result<Value, String> {
        if !extensions::valid_id(plugin)
            || !extensions::valid_id(command)
            || !args.is_object()
            || serde_json::to_vec(args).map_err(|e| e.to_string())?.len()
                > native::argument_limit(&self.opts.config, plugin, command)
        {
            return Err("Invalid plugin command or arguments".into());
        }
        if !self
            .service_plugin_commands()
            .iter()
            .any(|p| p.id == plugin && p.enabled && p.commands.iter().any(|c| c.name == command))
        {
            return Err("No such enabled plugin command".into());
        }
        if let Some(result) = native::command(self, plugin, command, args) {
            return result;
        }
        let result =
            self.extensions.engine.as_mut().ok_or("Service plugins are off")?.service_command(plugin, command, args);
        result
    }

    pub(super) fn service_plugin_job(&mut self, done: JobDone) {
        if let Some(engine) = self.extensions.engine.as_mut() {
            engine.job_done(done);
        }
        self.drain_service_plugins();
    }

    pub(super) fn service_plugins_busy(&self) -> bool {
        self.extensions.workers.load(Ordering::SeqCst) > 0
            || self.native.scan_preparing > 0
            || !self.extensions.pending.is_empty()
            || self.extensions.engine.as_ref().is_some_and(Engine::has_jobs)
    }

    pub(super) fn service_plugins_tick(&mut self, now: Instant) {
        self.extensions.metadata.retain(|id, _| self.sessions.contains_key(id));
        self.extensions.titles.retain(|id, _| self.sessions.contains_key(id));
        for (plugin, name, ended) in self.extensions.processes.poll(now) {
            let payload = serde_json::json!({"plugin":plugin,"name":name,"result":format!("{ended:?}")});
            if let Some(engine) = self.extensions.engine.as_mut() {
                engine.emit_json("process_end", &payload);
            }
        }
        // The scripts read the state the decks were last sent (built once
        // for both), at most at the status cadence; only the fields a script
        // asks for become JSON, and those that did not change stay as read.
        // The status refresh brings that state up to date at the same
        // cadence; when nothing did since the last snapshot, `state_changed`
        // does it here (and sends any change to the decks), so the scripts
        // never read a state older than one snapshot.
        if self.opts.config.lua
            && self.extensions.engine.is_some()
            && self.extensions.snapshot_at.is_none_or(|at| now.saturating_duration_since(at) >= STATUS_INTERVAL)
        {
            self.extensions.snapshot_at = Some(now);
            if self.state_builds == self.extensions.snapshot_builds {
                self.state_changed();
            }
            self.extensions.snapshot_builds = self.state_builds;
            let state = self.last_state.clone();
            if self.extensions.published.as_ref().is_none_or(|published| !Arc::ptr_eq(published, &state)) {
                let previous = self.extensions.published.replace(state.clone());
                let changed = previous.as_ref().map(|previous| state.changed(previous));
                if changed.as_ref().is_none_or(|changed| !changed.is_empty()) {
                    let mut changes = crate::lua::changes(&self.extensions.seen, &state.sessions);
                    for gone in previous.iter().flat_map(|previous| &previous.sessions) {
                        if gone.alive && !state.sessions.iter().any(|s| s.id == gone.id) {
                            let mut ended = serde_json::to_value(gone).unwrap_or_default();
                            ended["alive"] = Value::Bool(false);
                            ended["status"] = serde_json::json!("ended");
                            changes.push(crate::lua::Change { event: "session_end", payload: ended });
                        }
                    }
                    self.extensions.seen = crate::lua::seen(&state.sessions);
                    if let Some(engine) = self.extensions.engine.as_mut() {
                        engine.set_state_source(Rc::new(state), changed.as_deref());
                        for change in changes {
                            engine.emit_json(change.event, &change.payload);
                        }
                        engine.emit_json("state", &Value::Null);
                    }
                }
            }
        }
        if let Some(engine) = self.extensions.engine.as_mut() {
            engine.tick(now);
        }
        self.drain_service_plugins();
    }

    fn drain_service_plugins(&mut self) {
        if self.extensions.draining {
            return;
        }
        self.extensions.draining = true;
        // The limit also bounds chains of commands and callbacks.
        for _ in 0..8 {
            let Some(engine) = self.extensions.engine.as_mut() else {
                break;
            };
            let requests = engine.take_requests();
            let errors = engine.take_errors();
            for error in errors {
                self.announce_error(error);
            }
            if requests.is_empty() {
                break;
            }
            for request in requests {
                match request {
                    Request::Fleet { action, args, callback } => self.fleet_reply(0, callback, &action, &args),
                    Request::Notify { text, error } => {
                        if error {
                            self.announce_error(text);
                        } else {
                            self.broadcast(&ServerMsg::Notice { text, error: false });
                        }
                    }
                    Request::ServiceCall { plugin, command, args, callback } => {
                        self.handle_plugin(0, callback.unwrap_or(0), &plugin, &command, &args);
                    }
                    Request::Annotate { plugin, id, data } => {
                        if self.opts.config.plugins.get(&plugin) != Some(&false) && self.sessions.contains_key(&id) {
                            let metadata = self.extensions.metadata.entry(id).or_default();
                            if metadata.len() < 32 || metadata.contains_key(&plugin) {
                                metadata.insert(plugin, data);
                            }
                        }
                    }
                    Request::Title { plugin, id, text } => {
                        if self.opts.config.plugins.get(&plugin) != Some(&false) && self.sessions.contains_key(&id) {
                            if let Some(text) = text {
                                self.extensions.titles.insert(id, (plugin, text));
                            } else if self.extensions.titles.get(&id).is_some_and(|(owner, _)| owner == &plugin) {
                                self.extensions.titles.remove(&id);
                            }
                        }
                    }
                    Request::Process { plugin, name, spec } => {
                        let result = if self.shutdown_at.is_some() {
                            Err(anyhow::anyhow!("Service is stopping"))
                        } else if self.opts.config.plugins.get(&plugin) == Some(&false) {
                            Err(anyhow::anyhow!("Plugin is disabled"))
                        } else if let Some(spec) = spec {
                            serde_json::from_value(spec).context("Plugin process specification").and_then(|spec| {
                                self.extensions.processes.start(
                                    &plugin,
                                    &name,
                                    spec,
                                    &self.opts.start_dir,
                                    &self.opts.state_dir,
                                    &child_env_remove(std::env::vars_os().filter_map(|(k, _)| k.into_string().ok())),
                                )
                            })
                        } else {
                            self.extensions.processes.stop(&plugin, &name)
                        };
                        if let Err(error) = result {
                            self.announce_error(format!("Plugin {plugin}: {error:#}"));
                        }
                    }
                    Request::Send { id, text } => {
                        if self.sessions.get(&id).is_some_and(|s| s.alive) {
                            self.broadcast_message(0, &[id], &text);
                        }
                    }
                    _ => {
                        self.log_error("A service plugin requested an operation belonging to a view");
                    }
                }
            }
        }
        self.extensions.draining = false;
    }
}
