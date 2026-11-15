//! Authenticated phone access to desktop operations. Slow work uses private
//! service connections, so late replies cannot answer another phone's request.
use super::*;
use crate::gitwt;
use crate::protocol::{InstallRequest, MAX_NAME};
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const CONTROL_LIMIT: Duration = Duration::from_secs(300);
const REPORT_LIMIT: u64 = 2 * 1024 * 1024;
const CAPABILITIES: &[&str] = &[
    "overview",
    "fleet",
    "branches",
    "broadcast",
    "terminal",
    "terminal_screen",
    "terminal_input",
    "terminal_resize",
    "scans",
    "scan",
    "scan_report",
    "install",
    "skills_sync",
    "service",
    "dev",
    "rename",
    "create",
    "move_in",
    "scripts",
    "plugins",
    "plugin",
];

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Fleet {
        operation: String,
        #[serde(default = "empty_args")]
        args: Value,
    },
    Plugins {},
    Plugin {
        plugin: String,
        command: String,
        #[serde(default = "empty_args")]
        args: Value,
    },
    Overview {},
    Branches {},

    Broadcast {
        ids: Vec<u64>,
        text: String,
    },
    Terminal {
        host: Option<String>,
        cwd: Option<String>,
        name: Option<String>,
        command: Option<Vec<String>>,
        #[serde(default)]
        scratch: bool,
    },
    TerminalScreen {
        id: u64,
    },
    TerminalInput {
        id: u64,
        text: Option<String>,
        key: Option<String>,
    },
    TerminalResize {
        id: u64,
        cols: u16,
        rows: u16,
    },
    Scans {
        host: Option<String>,
    },
    Scan {
        host: Option<String>,
        scope: Option<String>,
    },
    ScanReport {
        host: Option<String>,
        id: String,
    },
    Install {
        source: String,
        #[serde(default)]
        agents: Vec<String>,
        host: Option<String>,
        #[serde(default)]
        remove: bool,
        #[serde(default = "yes")]
        dry_run: bool,
    },
    SkillsSync {},
    Service {
        operation: String,
        #[serde(default)]
        confirm: bool,
    },
    Dev {
        operation: String,
        #[serde(default)]
        confirm: bool,
    },
    Rename {
        id: u64,
        name: Option<String>,
    },
    Create {
        tool: Tool,
        model: Option<String>,
        effort: Option<String>,
        text: Option<String>,
        host: Option<String>,
        cwd: Option<String>,
        name: Option<String>,
    },
    MoveIn {
        pid: u32,
    },
}
fn empty_args() -> Value {
    json!({})
}
fn yes() -> bool {
    true
}

fn clean_name(name: &Option<String>) -> Result<()> {
    if name.as_ref().is_some_and(|s| s.chars().count() > MAX_NAME || s.chars().any(char::is_control)) {
        bail!("a name must have at most {MAX_NAME} characters and no control characters");
    }
    Ok(())
}
fn valid_text(text: &str, limit: usize) -> Result<()> {
    if text.trim().is_empty() || text.len() > limit || text.contains('\0') {
        bail!("the text is empty, contains NUL, or is too long");
    }
    Ok(())
}

/// Pin the opened file as well as its canonical pathname to reject symlink
/// escapes, including a swap between resolving and opening the report.
pub(crate) fn read_report(root: &Path, id: &str) -> Result<String> {
    if id.is_empty() || id.len() > 80 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        bail!("invalid scan id");
    }
    let root = root.canonicalize().context("no saved scans here")?;
    let resolved = root.join(id).join("report/report.md").canonicalize().context("this scan has no saved report")?;
    if !resolved.starts_with(&root) {
        bail!("the report is outside the scans folder");
    }
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&resolved)?;
    let opened = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd())).canonicalize()?;
    if !opened.starts_with(&root) {
        bail!("the report is outside the scans folder");
    }
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > REPORT_LIMIT {
        bail!("the report is not a file or exceeds 2 MiB");
    }
    let mut bytes = Vec::new();
    file.take(REPORT_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > REPORT_LIMIT {
        bail!("the report exceeds 2 MiB");
    }
    String::from_utf8(bytes).context("the report is not UTF-8 text")
}

/// One request owns this socket through its reply. Outstanding timed-out replies
/// die with it, independently of the gateway bridge and other phones.
pub(super) fn exchange(
    socket: &Path,
    msg: &ClientMsg,
    wait: Duration,
    accept: impl Fn(&ServerMsg) -> bool,
) -> Result<ServerMsg> {
    let reply = exchange_prepared(socket, wait, or_error(accept), None, |stream| {
        write_msg(stream, msg)?;
        Ok(())
    })?;
    failed_on_error(reply)
}

/// `accept`, and any error the service reports.
fn or_error(accept: impl Fn(&ServerMsg) -> bool) -> impl Fn(&ServerMsg) -> bool {
    move |reply| matches!(reply, ServerMsg::Notice { error: true, .. }) || accept(reply)
}

/// The reply, or the error it reports as this request's failure.
fn failed_on_error(reply: ServerMsg) -> Result<ServerMsg> {
    if let ServerMsg::Notice { text, error: true } = &reply {
        bail!("{text}");
    }
    Ok(reply)
}

/// [`exchange`] on a connection that may first have to identify the service
/// (`expected_pid`), with every reply offered to `accept`, errors too: an
/// error the service reports to every client is not this request's failure.
fn exchange_prepared(
    socket: &Path,
    wait: Duration,
    accept: impl Fn(&ServerMsg) -> bool,
    expected_pid: Option<u32>,
    send: impl FnOnce(&mut UnixStream) -> Result<()>,
) -> Result<ServerMsg> {
    let mut stream = crate::client::connect_public_socket(socket).context("the Cepheus service is not running")?;
    stream.set_read_timeout(Some(wait))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write_msg(&mut stream, &ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control })?;
    let deadline = Instant::now() + wait;
    let mut reader = BufReader::new(stream);
    if let Some(expected_pid) = expected_pid {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "The Cepheus service did not identify its private connection; no session was started."
            );
            reader.get_mut().set_read_timeout(Some(remaining))?;
            let hello = read_msg::<_, ServerMsg>(&mut reader)
                .context("The Cepheus service did not identify its private connection; no session was started.")?
                .context(
                    "The Cepheus service closed before identifying its private connection; no session was started.",
                )?;
            if let ServerMsg::Welcome { pid, .. } = hello {
                anyhow::ensure!(
                    pid == expected_pid,
                    "The Cepheus service changed; waiting for its session state before starting."
                );
                break;
            }
            if let ServerMsg::Notice { text, error: true } = hello {
                bail!("{text}");
            }
        }
    }
    send(reader.get_mut())?;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        reader.get_mut().set_read_timeout(Some(remaining))?;
        let reply = match read_msg::<_, ServerMsg>(&mut reader) {
            Ok(Some(reply)) => reply,
            Ok(None) => bail!("the service closed the request"),
            Err(err) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            Err(err) => return Err(err.into()),
        };
        if accept(&reply) {
            return Ok(reply);
        }
    }
    bail!("the service did not answer in time")
}

struct Start {
    tool: Tool,
    model: Option<String>,
    effort: Option<String>,
    text: Option<String>,
    host: Option<String>,
    cwd: Option<String>,
    name: Option<String>,
    command: Vec<String>,
}
impl Hub {
    fn stop_service(&self) -> Result<()> {
        if self.control_state()?.0.sessions.iter().any(|session| session.alive) {
            bail!("{}", crate::protocol::STOP_REFUSED_NOTICE);
        }
        exchange(
            &self.opts.socket,
            &ClientMsg::Shutdown,
            SERVICE_REPLY,
            |reply| matches!(reply, ServerMsg::Notice { text, error: false } if text == crate::protocol::STOP_ACCEPTED_NOTICE),
        )?;
        Ok(())
    }

    fn control_config(&self) -> Result<Config> {
        let (config, error) = match self.opts.config_file.as_deref() {
            Some(path) => Config::load_from(path),
            None => Config::load(),
        };
        if let Some(error) = error {
            bail!("config: {error}");
        }
        Ok(config)
    }
    fn control_host(&self, host: Option<&str>) -> Result<()> {
        if let Some(name) = host {
            if name == "@here" {
                return Ok(());
            }
            if !lock(&self.inner).hosts.iter().any(|h| h.name == name) {
                bail!("unknown host");
            }
            if !self.host_online(name) {
                bail!("{name} is not reachable now");
            }
        }
        Ok(())
    }
    fn control_state(&self) -> Result<(DeckState, Vec<HostInfo>)> {
        let inner = lock(&self.inner);
        Ok((inner.state.clone().context("the service has not sent its state")?, inner.hosts.clone()))
    }
    fn shell_session(&self, id: u64) -> Result<SessionInfo> {
        let info = self.live_session(id)?;
        if info.tool != Tool::Shell {
            bail!("raw terminal controls are only available for shell sessions");
        }
        if !info.alive {
            bail!("the terminal has ended");
        }
        self.control_host(info.host.as_deref())?;
        Ok(info)
    }
    fn control_create(self: &Arc<Self>, start: Start) -> Result<Value> {
        let Start { tool, model, effort, text, host, cwd, name, command } = start;
        self.control_host(host.as_deref())?;
        clean_name(&name)?;
        if cwd
            .as_ref()
            .is_some_and(|s| s.len() > 4096 || !Path::new(s).is_absolute() || s.chars().any(char::is_control))
        {
            bail!("the start folder must be an absolute path without control characters");
        }
        if command.len() > 32
            || command.iter().any(|s| s.len() > 4096 || s.contains('\0'))
            || command.iter().map(String::len).sum::<usize>() > SEND_MAX
        {
            bail!("the terminal command is too long or contains NUL");
        }
        if model.as_deref().is_some_and(|m| !service::valid_model_name(m)) {
            bail!("invalid model name");
        }
        if effort.as_deref().is_some_and(|e| !service::valid_effort(e)) {
            bail!("invalid effort");
        }
        if let Some(text) = text.as_deref() {
            valid_text(text, SEND_MAX)?;
        }
        let msg = ClientMsg::Create {
            tool,
            model: model.clone(),
            effort: effort.clone(),
            host,
            cwd,
            command,
            name,
            request: None,
        };
        let marker = self.session_state_marker()?;
        let reply =
            self.exchange_private(&msg, CONTROL_LIMIT, marker, or_error(|m| matches!(m, ServerMsg::Created { .. })))?;
        let ServerMsg::Created { id, .. } = failed_on_error(reply)? else {
            bail!("unexpected create reply");
        };
        self.finish_create(id, tool, model, effort, text, marker)?;
        Ok(json!({"id":id}))
    }
    /// Send `msg` on a connection of its own to the service the bridge
    /// reached at `marker`, and wait there for the reply `accept` takes,
    /// errors included. Nothing is written until that connection identifies
    /// the same service and the bridge is still its own; a reply that comes
    /// after the request gave up closes with the connection.
    pub(super) fn exchange_private(
        &self,
        msg: &ClientMsg,
        wait: Duration,
        marker: (u64, u64),
        accept: impl Fn(&ServerMsg) -> bool,
    ) -> Result<ServerMsg> {
        let pid = {
            let inner = lock(&self.inner);
            anyhow::ensure!(
                inner.service_ready && inner.service_generation == marker.0,
                "The Cepheus service connection changed; check Home before starting another session."
            );
            inner.service_pid.context("The Cepheus service identity is not ready; waiting for its session state.")?
        };
        exchange_prepared(&self.opts.socket, wait, accept, Some(pid), |stream| {
            // Serialize this dispatch with bridge replacement, without holding state across I/O.
            let bridge = lock(&self.service);
            let inner = lock(&self.inner);
            anyhow::ensure!(
                inner.service_ready && inner.service_generation == marker.0 && inner.service_pid == Some(pid),
                "The Cepheus service connection changed; check Home before starting another session."
            );
            drop(inner);
            write_msg(stream, msg)?;
            drop(bridge);
            Ok(())
        })
    }
    pub(super) fn control(self: &Arc<Self>, action: &str, mut args: Value) -> Result<Value> {
        if !args.is_object() {
            bail!("control arguments must be an object");
        }
        if let Some(owner) = crate::extensions::phone_action_owner(action) {
            if !self.control_config()?.plugin_enabled(&owner) {
                bail!("Plugin {owner} is disabled");
            }
        }
        if action == "scripts" {
            let (state, hosts) = self.control_state()?;
            return crate::tui::phone::run(&self.opts.socket, state, hosts, args);
        }
        args["action"] = json!(action);
        let action: Action = serde_json::from_value(args).context("invalid control request")?;
        match action {
            Action::Fleet { operation, args } => {
                if crate::attention::is_action(&operation) {
                    anyhow::ensure!(
                        lock(&self.inner).service_version >= 15,
                        "Update needed: attention requires protocol 15"
                    );
                }
                if operation == "attention-send" {
                    anyhow::ensure!(
                        lock(&self.inner).service_version >= 17,
                        "Update needed: identity-safe attention send requires protocol 17"
                    );
                }
                if operation == "attention-supervisor-rules"
                    && args["rules"]
                        .as_array()
                        .is_some_and(|rules| rules.iter().any(|r| r["pattern"]["kind"] == "native_exact"))
                {
                    anyhow::ensure!(
                        lock(&self.inner).service_version >= 16,
                        "Update needed: native permission rules require protocol 16"
                    );
                }
                if operation.starts_with("job-") {
                    anyhow::ensure!(lock(&self.inner).service_version >= 14, "Update needed: jobs require protocol 14");
                }
                anyhow::ensure!(
                    lock(&self.inner).service_version >= 14,
                    "Update needed: fleet scheduling requires protocol 14"
                );
                anyhow::ensure!(args.is_object() && operation.len() <= 32, "invalid fleet request");
                match exchange(
                    &self.opts.socket,
                    &ClientMsg::Fleet { request: 1, action: operation, args },
                    CONTROL_LIMIT,
                    |m| matches!(m, ServerMsg::FleetResult { request: 1, .. }),
                )? {
                    ServerMsg::FleetResult { data, error: None, .. } => Ok(data),
                    ServerMsg::FleetResult { error: Some(e), .. } => bail!("{e}"),
                    _ => bail!("unexpected fleet reply"),
                }
            }
            Action::Plugins {} => {
                if lock(&self.inner).service_version < 11 {
                    bail!("Update the service for plugin discovery");
                }
                match exchange(&self.opts.socket, &ClientMsg::PluginList { request: 1 }, SERVICE_REPLY, |m| {
                    matches!(m, ServerMsg::PluginResult { request: 1, .. })
                })? {
                    ServerMsg::PluginResult { data, error, .. } => {
                        if let Some(error) = error {
                            bail!("{error}");
                        }
                        Ok(data)
                    }
                    _ => bail!("Unexpected plugin discovery reply"),
                }
            }
            Action::Plugin { plugin, command, args } => {
                if lock(&self.inner).service_version < 11 {
                    bail!("Update the service for plugin commands");
                }
                if !crate::extensions::valid_id(&plugin)
                    || !crate::extensions::valid_id(&command)
                    || !args.is_object()
                    || serde_json::to_vec(&args)?.len() > 16 * 1024
                {
                    bail!("Invalid plugin command or arguments");
                }
                match exchange(
                    &self.opts.socket,
                    &ClientMsg::Plugin { request: 1, plugin, command, args },
                    CONTROL_LIMIT,
                    |m| matches!(m, ServerMsg::PluginResult { request: 1, .. }),
                )? {
                    ServerMsg::PluginResult { data, error, .. } => {
                        if let Some(error) = error {
                            bail!("{error}");
                        }
                        Ok(if data.is_object() { data } else { json!({"value":data}) })
                    }
                    _ => bail!("Unexpected plugin command reply"),
                }
            }
            Action::Overview {} => {
                let version = lock(&self.inner).service_version;
                let (state, hosts) = self.control_state()?;
                let config = self.control_config()?;
                let terminals: Vec<Value> = state
                    .sessions
                    .iter()
                    .filter(|s| s.tool == Tool::Shell)
                    .map(|s| json!({"id":s.id,"title":s.title,"host":s.host,"alive":s.alive}))
                    .collect();
                Ok(
                    json!({"protocol":version,"capabilities":CAPABILITIES.iter().copied().filter(|a|(*a!="fleet" || version>=14) && (*a!="terminal_resize" || version>=10) && (!matches!(*a,"plugins"|"plugin") || version>=11) && crate::extensions::phone_action_owner(a).is_none_or(|owner|config.plugin_enabled(&owner))).collect::<Vec<_>>(),"terminals":terminals,"hosts":hosts,"dev_server":state.dev_server,"panels":state.panels,"start_dir":state.start_dir,"here":self.links.coordinator_label().unwrap_or_else(||self.opts.here.clone())}),
                )
            }
            Action::Branches {} => {
                let (state, hosts) = self.control_state()?;
                let mut items = Vec::new();
                let local = if state.branches.is_empty() && !self.links.is_shared() {
                    gitwt::branches(Path::new(&state.start_dir))
                } else {
                    state.branches.clone()
                };
                let sessions = phone_sessions(&state, &self.order_for(&state), &hosts);
                for (host, label, branches) in
                    std::iter::once((None, self.opts.here.clone(), local)).chain(hosts.iter().map(|h| {
                        (
                            Some(h.name.clone()),
                            h.label.clone(),
                            h.stats.as_ref().map(|s| s.branches.clone()).unwrap_or_default(),
                        )
                    }))
                {
                    for branch in branches {
                        let active: Vec<Value> = state
                            .sessions
                            .iter()
                            .filter(|s| {
                                if s.host != host || !s.alive || !s.tool.is_agent() {
                                    return false;
                                }
                                match &s.worktree {
                                    Some(w) => {
                                        w.branch.as_deref() == Some(branch.branch.as_str())
                                            || (!branch.path.is_empty() && w.path == branch.path)
                                    }
                                    None => {
                                        branch.main
                                            || s.cwd
                                                .as_deref()
                                                .is_some_and(|cwd| !branch.path.is_empty() && cwd == branch.path)
                                    }
                                }
                            })
                            .filter_map(|s| sessions.iter().find(|v| v["id"] == s.id).cloned())
                            .collect();
                        let mut item = serde_json::to_value(branch)?;
                        item["host"] = json!(host);
                        item["host_label"] = json!(label);
                        item["sessions"] = json!(active);
                        items.push(item);
                    }
                }
                Ok(json!({"items":items,"hosts":hosts}))
            }
            Action::Broadcast { ids, text } => {
                valid_text(&text, SEND_MAX)?;
                if ids.is_empty() || ids.len() > 100 {
                    bail!("choose between 1 and 100 sessions");
                }
                let mut seen = std::collections::HashSet::new();
                for id in &ids {
                    if !seen.insert(*id) {
                        bail!("duplicate session");
                    }
                    let info = self.live_session(*id)?;
                    if !info.alive || info.tool == Tool::Shell {
                        bail!("choose live agent sessions");
                    }
                    self.control_host(info.host.as_deref())?;
                }
                let mut accepted = Vec::new();
                let mut errors = Vec::new();
                for id in ids {
                    match self.type_broadcast(id, &text) {
                        Ok(()) => accepted.push(id),
                        Err(e) => errors.push(json!({"id":id,"error":format!("{e:#}")})),
                    }
                }
                Ok(json!({"accepted":accepted,"errors":errors}))
            }
            Action::Terminal { host, cwd, name, command, scratch } => {
                let name = if scratch {
                    if let Some(existing) = self
                        .control_state()?
                        .0
                        .sessions
                        .iter()
                        .find(|s| s.tool == Tool::Shell && s.host == host && s.title == "scratch")
                    {
                        self.control_host(host.as_deref())?;
                        let id = existing.id;
                        if !existing.alive {
                            exchange(
                                &self.opts.socket,
                                &ClientMsg::Reopen { id },
                                CONTROL_LIMIT,
                                |m| matches!(m, ServerMsg::Created { id: got, .. } if *got == id),
                            )?;
                        }
                        return Ok(json!({"id":id}));
                    }
                    Some("scratch".to_string())
                } else {
                    name
                };
                self.control_create(Start {
                    tool: Tool::Shell,
                    model: None,
                    effort: None,
                    text: None,
                    host,
                    cwd,
                    name,
                    command: command.unwrap_or_default(),
                })
            }
            Action::Create { tool, model, effort, text, host, cwd, name } => {
                self.control_create(Start { tool, model, effort, text, host, cwd, name, command: Vec::new() })
            }
            Action::TerminalScreen { id } => {
                let info = self.live_session(id)?;
                if info.tool != Tool::Shell {
                    bail!("raw terminal controls are only available for shell sessions");
                }
                let (lines, _) = self.screen(id)?;
                Ok(json!({"id":id,"text":screen_text(&lines),"alive":info.alive,"title":info.title}))
            }
            Action::TerminalInput { id, text, key } => {
                self.shell_session(id)?;
                let data = match (text, key) {
                    (Some(text), None) => {
                        if text.is_empty()
                            || text.len() > SEND_MAX
                            || text.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
                        {
                            bail!("terminal text must be bounded literal text; use key for control keys");
                        }
                        text
                    }
                    (None, Some(key)) => match key.as_str() {
                        "enter" => "\r",
                        "tab" => "\t",
                        "escape" => "\x1b",
                        "backspace" => "\x7f",
                        "ctrl_c" => "\x03",
                        "ctrl_d" => "\x04",
                        "up" => "\x1b[A",
                        "down" => "\x1b[B",
                        "right" => "\x1b[C",
                        "left" => "\x1b[D",
                        "home" => "\x1b[H",
                        "end" => "\x1b[F",
                        _ => bail!("unknown terminal key"),
                    }
                    .to_string(),
                    _ => bail!("send either literal text or one control key"),
                };
                self.input(id, &data)?;
                Ok(json!({"id":id}))
            }
            Action::TerminalResize { id, cols, rows } => {
                self.shell_session(id)?;
                if !(20..=500).contains(&cols) || !(5..=300).contains(&rows) {
                    bail!("terminal size is outside 20..500 columns and 5..300 rows");
                }
                if lock(&self.inner).service_version < 10 {
                    bail!("update the service to resize shells separately");
                }
                exchange(
                    &self.opts.socket,
                    &ClientMsg::ResizeTerminal { id, cols, rows },
                    SERVICE_REPLY,
                    |m| matches!(m,ServerMsg::ScreenText {id:got,..} if *got==id),
                )?;
                Ok(json!({"id":id,"cols":cols,"rows":rows}))
            }
            Action::Scans { host } => {
                if let Some(name) = host.as_deref().filter(|name| *name != "all") {
                    self.control_host(Some(name))?;
                }
                let items: Vec<_> = self
                    .control_state()?
                    .0
                    .scans
                    .into_iter()
                    .filter(|s| host.as_deref() == Some("all") || s.host == host)
                    .collect();
                Ok(json!({"items":items}))
            }
            Action::Scan { host, scope } => {
                self.control_host(host.as_deref())?;
                if scope.as_ref().is_some_and(|s| {
                    s.len() > 200
                        || s.is_empty()
                        || s.starts_with('/')
                        || s.split('/').any(|p| p == ".." || p == "." || p.is_empty())
                        || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
                }) {
                    bail!("invalid scan scope");
                }
                match exchange(&self.opts.socket, &ClientMsg::SecurityScan { host, scope }, CONTROL_LIMIT, |m| {
                    matches!(m, ServerMsg::Created { .. })
                })? {
                    ServerMsg::Created { id, .. } => Ok(json!({"id":id})),
                    _ => bail!("unexpected scan reply"),
                }
            }
            Action::ScanReport { host, id } => {
                self.control_host(host.as_deref())?;
                if !self.control_state()?.0.scans.iter().any(|s| s.id == id && s.host == host) {
                    bail!("no such scan");
                }
                let text = if host.is_some() || self.links.is_shared() {
                    match self.links.ask_source(host.as_deref(), &HostReq::ScanReport { id: id.clone() }, None)? {
                        HostResp::ScanReport { text } => text,
                        other => return Err(links::unexpected(&other)),
                    }
                } else {
                    read_report(&self.control_config()?.scans_dir(), &id)?
                };
                if text.len() as u64 > REPORT_LIMIT {
                    bail!("the report exceeds 2 MiB");
                }
                Ok(json!({"id":id,"text":text}))
            }
            Action::Install { source, agents, host, remove, dry_run } => {
                self.control_host(host.as_deref())?;
                valid_text(&source, 4096)?;
                if source.chars().any(char::is_control)
                    || agents.len() > 3
                    || agents.iter().any(|a| !matches!(a.as_str(), "claude" | "codex" | "agy"))
                {
                    bail!("invalid installer source or agents");
                }
                let request = InstallRequest { source, agents, remove, dry_run };
                match exchange(&self.opts.socket, &ClientMsg::Install { request, host }, CONTROL_LIMIT, |m| {
                    matches!(m, ServerMsg::Installed { .. })
                })? {
                    ServerMsg::Installed { ok, lines, .. } => Ok(
                        json!({"ok":ok,"items":lines.iter().map(|text|json!({"text":text})).collect::<Vec<_>>(),"lines":lines}),
                    ),
                    _ => bail!("unexpected installer reply"),
                }
            }
            Action::SkillsSync {} => {
                match exchange(
                    &self.opts.socket,
                    &ClientMsg::SyncSkills,
                    CONTROL_LIMIT,
                    |m| matches!(m,ServerMsg::Notice {text,..} if text.starts_with("The deck's skills") || text.starts_with("Skills:")),
                )? {
                    ServerMsg::Notice { text, .. } => Ok(json!({"text":text})),
                    _ => bail!("unexpected skill sync reply"),
                }
            }
            Action::Service { operation, confirm } => {
                match operation.as_str() {
                    "restart" => self.send_service(&ClientMsg::Restart)?,
                    "stop" if confirm => self.stop_service()?,
                    "stop" => bail!("confirm stopping the empty service"),
                    _ => bail!("unknown service operation"),
                }
                Ok(json!({"operation":operation}))
            }
            Action::Dev { operation, confirm } => {
                let run = match operation.as_str() {
                    "on" => true,
                    "off" if confirm => false,
                    "off" => bail!("confirm stopping the development server"),
                    _ => bail!("unknown development server operation"),
                };
                self.send_service(&ClientMsg::DevServer { run })?;
                Ok(json!({"operation":operation}))
            }
            Action::Rename { id, name } => {
                self.live_session(id)?;
                clean_name(&name)?;
                self.send_service(&ClientMsg::Rename { id, name: name.clone() })?;
                Ok(json!({"id":id,"name":name}))
            }
            Action::MoveIn { pid } => {
                let outside = self
                    .control_state()?
                    .0
                    .outside
                    .into_iter()
                    .find(|o| o.pid == pid)
                    .context("this outside process is gone")?;
                self.control_host(outside.host.as_deref())?;
                let msg = ClientMsg::MoveIn { pid, tool: outside.tool, conversation: outside.conversation };
                match exchange(&self.opts.socket, &msg, CONTROL_LIMIT, |m| matches!(m, ServerMsg::Created { .. }))? {
                    ServerMsg::Created { id, .. } => Ok(json!({"id":id})),
                    _ => bail!("unexpected move-in reply"),
                }
            }
        }
    }
}

/// Limit expensive authenticated requests without holding up pings or chat.
pub(super) struct Permit(Arc<Hub>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.control_jobs.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Hub {
    pub(super) fn begin_control(self: &Arc<Self>) -> Option<Permit> {
        self.control_jobs.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < 4).then_some(n + 1)).ok()?;
        Some(Permit(Arc::clone(self)))
    }
}
#[cfg(test)]
#[path = "../tests/remote_control_logic.rs"]
mod tests;
