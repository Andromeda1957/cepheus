//! Scriptable control commands (`cepheus ctl ...`, `cepheus stop`).

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::client;
use crate::protocol::{read_msg, write_msg, ClientMsg, ClientRole, DeckState, ServerMsg, Tool, PROTOCOL_VERSION};

/// A control connection: a request at a time, its reply read back while the
/// latest state is kept. The deck's New dialog uses one too, to choose a new
/// Codex session's model (`codexpick.rs`).
pub struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    state: DeckState,
    /// The service's protocol and process, from its welcome.
    version: u32,
    pid: u32,
    next_request: u64,
}

impl Conn {
    fn open(spawn: bool) -> Result<Conn> {
        let stream = if spawn {
            client::connect_or_spawn()?
        } else {
            client::connect().context("the Cepheus service is not running")?
        };
        Conn::over(stream)
    }

    fn local() -> Result<Conn> {
        Conn::over(client::connect_local().context("the native cepheus service is not running")?)
    }

    /// Greet the service on `stream` and wait for its state.
    pub fn over(stream: UnixStream) -> Result<Conn> {
        stream.set_read_timeout(Some(Duration::from_secs(15)))?;
        let writer = stream.try_clone()?;
        let mut conn = Conn {
            reader: BufReader::new(stream),
            writer,
            state: DeckState::default(),
            version: 0,
            pid: 0,
            next_request: 0,
        };
        conn.send(&ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control })?;
        conn.wait(|msg| matches!(msg, ServerMsg::State(_)))?;
        Ok(conn)
    }

    fn native_message(&mut self, id: u64, text: &str) -> Result<Option<crate::protocol::Delivered>> {
        anyhow::ensure!(
            self.version >= crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL,
            "Update needed: direct agent messages require Cepheus protocol 19; nothing was sent"
        );
        self.next_request = self.next_request.checked_add(1).context("message request limit")?;
        let request = self.next_request;
        self.reader.get_ref().set_read_timeout(Some(Duration::from_secs(40)))?;
        self.send(&ClientMsg::SendMessage { id, request, text: text.to_owned(), attachments: false })?;
        match self.wait(|message| matches!(message, ServerMsg::MessageSent { id: target, request: got, .. } if *target == id && *got == request))? {
            ServerMsg::MessageSent { error: None, delivered, .. } => Ok(delivered),
            ServerMsg::MessageSent { error: Some(error), .. } => bail!("{error}"),
            _ => bail!("native message outcome unknown; check the conversation before retrying"),
        }
    }

    /// The service is an older build than this command.
    fn older(&self) -> bool {
        self.version != 0 && self.version < PROTOCOL_VERSION
    }

    fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        write_msg(&mut self.writer, msg)?;
        Ok(())
    }

    /// Read until `want` matches, keeping the latest state; returns that message.
    fn wait(&mut self, want: impl Fn(&ServerMsg) -> bool) -> Result<ServerMsg> {
        loop {
            let Some(msg) = read_msg::<_, ServerMsg>(&mut self.reader)? else {
                if self.older() {
                    bail!(
                        "the service closed the connection: it is an older build (protocol {}) that may not know \
                         this command; :restart in the deck restarts it",
                        self.version
                    );
                }
                bail!("the service closed the connection");
            };
            // A patch is applied and then seen as the whole state it makes.
            let msg = match msg {
                ServerMsg::StatePatch(patch) => {
                    self.state.apply(patch);
                    ServerMsg::State(self.state.clone())
                }
                msg => msg,
            };
            if let ServerMsg::State(state) = &msg {
                self.state = state.clone();
            }
            if let ServerMsg::Welcome { version, pid, .. } = &msg {
                (self.version, self.pid) = (*version, *pid);
            }
            // Before its welcome a service only says that our protocol
            // differs from its own; the welcome says which, and each
            // command decides what that service can do. The owner's
            // protocol-1 service refused every command this way.
            if let ServerMsg::Notice { text, error: true } = &msg {
                if self.pid != 0 {
                    bail!("{text}");
                }
            }
            if want(&msg) {
                return Ok(msg);
            }
        }
    }
}

impl crate::codexpick::SessionIo for Conn {
    fn screen(&mut self, id: u64) -> Result<(Vec<String>, u64)> {
        self.send(&ClientMsg::Screen { id, tail: None })?;
        match self.wait(|m| matches!(m, ServerMsg::ScreenText { id: got, .. } if *got == id))? {
            ServerMsg::ScreenText { lines, inputs, .. } => Ok((lines, inputs)),
            _ => bail!("unexpected reply"),
        }
    }

    fn input(&mut self, id: u64, data: &str) -> Result<()> {
        self.send(&ClientMsg::Input { id, data: data.to_string() })
    }

    /// The broadcast's reply is a notice; one that is an error ends the wait.
    fn type_message(&mut self, id: u64, text: &str) -> Result<()> {
        self.native_message(id, text).map(|_| ())
    }
}

fn parse_tool(s: Option<&String>) -> Result<Tool> {
    s.and_then(|s| {
        Tool::parse(s).or_else(|| {
            // A host-only provider is validated by its target service. This is
            // a stable identifier, never a client-supplied executable command.
            crate::providers::valid_id(s).then(|| crate::providers::intern(s).ok().map(Tool::Dynamic)).flatten()
        })
    })
    .context("expected a built-in program or a valid provider id")
}

/// The value after `flag` in `args`, and `args` without both.
fn take_flag(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let at = args.iter().position(|a| a == flag)?;
    let value = args.get(at + 1).cloned();
    args.drain(at..(at + 2).min(args.len()));
    value
}

/// Ask the service to start a session and print its id.
fn start(msg: ClientMsg) -> Result<()> {
    let mut conn = Conn::open(true)?;
    if matches!(&msg, ClientMsg::Create { tool: Tool::Codex, .. }) {
        anyhow::ensure!(conn.version >= 19, "Update needed: native Codex creation requires protocol 19");
        conn.reader.get_ref().set_read_timeout(Some(Duration::from_secs(35)))?;
    }
    conn.send(&msg)?;
    match conn.wait(|m| matches!(m, ServerMsg::Created { .. } | ServerMsg::Notice { error: true, .. }))? {
        ServerMsg::Created { id, .. } => println!("{id}"),
        ServerMsg::Notice { text, .. } => bail!("{text}"),
        _ => {}
    }
    Ok(())
}

fn shared_self_id(native: &crate::protocol::SessionInfo, shared: &DeckState) -> Result<u64> {
    let source = native
        .metadata
        .get("fleet-workspace")
        .and_then(|v| v["source"].as_str())
        .filter(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .context("session source identity is unavailable; update the native Deck service")?;
    let mut matches = shared.sessions.iter().filter(|session| {
        session.alive
            && session.tool == native.tool
            && session.metadata.get("fleet-workspace").is_some_and(|value| {
                value["source"].as_str() == Some(source) && value["session"].as_u64() == Some(native.id)
            })
    });
    let id = matches.next().context("hosted session has not reached the shared registry")?.id;
    anyhow::ensure!(matches.next().is_none(), "hosted session identity is ambiguous");
    Ok(id)
}

fn native_self_session<'a>(
    native: &'a DeckState,
    session_id: Option<&std::ffi::OsStr>,
    codex_thread: Option<&std::ffi::OsStr>,
) -> Result<&'a crate::protocol::SessionInfo> {
    // The deck's own variable names the session exactly whenever it is set.
    if let Some(value) = session_id {
        let raw = value.to_str().context("self requires a Deck-hosted session")?.parse::<u64>()?;
        return native.sessions.iter().find(|s| s.id == raw && s.alive).context("native hosted session is unavailable");
    }
    // A shared Codex app-server runs tools without the deck's variables, but
    // each tool still carries its thread, which the deck records as the
    // session's conversation. Only one live Codex session may claim it.
    let thread = codex_thread
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .context("self requires a Deck-hosted session")?;
    let mut owners = native
        .sessions
        .iter()
        .filter(|s| s.alive && s.tool == Tool::Codex && s.conversation.as_deref() == Some(thread));
    let session = owners.next().context("no hosted Codex session holds this thread")?;
    if owners.next().is_some() {
        bail!("several hosted Codex sessions hold this thread");
    }
    Ok(session)
}

fn own_session_id() -> Result<u64> {
    let raw = crate::paths::env_var("SESSION");
    let thread = std::env::var_os("CODEX_THREAD_ID").or_else(|| std::env::var_os("CODEX_SESSION_ID"));
    let native = Conn::local()?;
    let session = native_self_session(&native.state, raw.as_deref(), thread.as_deref())?;
    let mut public = Conn::open(false)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(id) = shared_self_id(session, &public.state) {
            return Ok(id);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return shared_self_id(session, &public.state);
        }
        public.reader.get_mut().set_read_timeout(Some(remaining))?;
        public.wait(|msg| matches!(msg, ServerMsg::State(_)))?;
    }
}

fn parse_id(s: Option<&String>) -> Result<u64> {
    s.context("missing session id")?.parse().context("session ids are numbers")
}

/// Expand `\r`, `\n`, `\t`, `\e`, `\\` and `\xHH` escapes.
pub fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('\\') => out.push('\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(b) => out.push(char::from(b)),
                    Err(_) => {
                        out.push_str("\\x");
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

pub fn run(args: &[String]) -> i32 {
    match ctl(args) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("cepheus: {err:#}");
            1
        }
    }
}

fn slot_options(args: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    let mut options = std::collections::BTreeMap::new();
    let mut iter = args.iter();
    while let Some(raw) = iter.next() {
        let (flag, value) = if let Some((flag, value)) = raw.split_once('=') {
            (flag, value.to_string())
        } else {
            (raw.as_str(), iter.next().context("slot option needs a value")?.clone())
        };
        anyhow::ensure!(
            ["--pool", "--label", "--compatibility", "--session", "--pid", "--max-wait", "--request-key"]
                .contains(&flag),
            "unknown slot option {flag}"
        );
        anyhow::ensure!(options.insert(flag.into(), value).is_none(), "duplicate slot option {flag}");
    }
    Ok(options)
}

fn fleet_call(conn: &mut Conn, action: &str, args: serde_json::Value) -> Result<serde_json::Value> {
    if action == "attention-send" {
        anyhow::ensure!(conn.version >= 17, "Update needed: identity-safe attention send requires protocol 17");
    }
    if crate::attention::is_action(action) {
        anyhow::ensure!(conn.version >= 15, "Update needed: attention requires protocol 15");
    }
    if crate::work_queue::is_action(action) {
        anyhow::ensure!(conn.version >= 14, "Update needed: work queues, sources and chores require protocol 14");
    }
    if action.starts_with("job-") {
        anyhow::ensure!(conn.version >= 14, "Update needed: jobs require protocol 14");
    }
    anyhow::ensure!(conn.version >= 14, "Update needed: the service does not support fleet scheduling (protocol 14)");
    conn.send(&ClientMsg::Fleet { request: 1, action: action.into(), args })?;
    match conn.wait(|m| matches!(m, ServerMsg::FleetResult { request: 1, .. }))? {
        ServerMsg::FleetResult { data, error: None, .. } => Ok(data),
        ServerMsg::FleetResult { error: Some(error), .. } => bail!("{error}"),
        _ => bail!("unexpected scheduler reply"),
    }
}
#[path = "agent_messaging.rs"]
mod agent_controls;
#[path = "job_sessions/ctl.rs"]
mod job_controls;
#[path = "work_queue/ctl.rs"]
mod work_controls;
fn ctl(args: &[String]) -> Result<()> {
    let cmd = args.first().map(String::as_str).unwrap_or("status");
    match cmd {
        "agent" => agent_controls::run(&args[1..])?,
        "job" => job_controls::run(&args[1..])?,
        "self" => println!("{}", own_session_id()?),
        "queue" | "source" | "chore" => work_controls::run(cmd, &args[1..])?,
        "fleet" => {
            let action = args.get(1).context("fleet action is required")?;
            let native = matches!(
                action.as_str(),
                "capabilities"
                    | "admission"
                    | "slot-status"
                    | "slot-hold"
                    | "slot-release"
                    | "rerun-record"
                    | "rerun-dismiss"
                    | "start"
            );
            let mut conn = if native { Conn::local()? } else { Conn::open(false)? };
            let data = work_controls::json_argument(args.get(2))?;
            println!("{}", fleet_call(&mut conn, action, data)?);
        }
        "admission" => {
            let mut conn = Conn::local()?;
            println!("{}", fleet_call(&mut conn, "admission", serde_json::json!({}))?);
        }
        "slot" => {
            let rest = args.get(2..).unwrap_or_default().to_vec();
            let action = args.get(1).context("slot hold|status|release")?.as_str();
            let mut conn = Conn::local()?;
            match action {
                "status" => {
                    println!("{}", fleet_call(&mut conn, "slot-status", serde_json::json!({}))?);
                }
                "release" => {
                    let id = rest.first().context("slot id is required")?.parse::<u64>()?;
                    println!("{}", fleet_call(&mut conn, "slot-release", serde_json::json!({"id":id}))?);
                }
                "hold" => {
                    let mut options = slot_options(&rest)?;
                    let pool = options.remove("--pool").unwrap_or_else(|| "heavy".into());
                    let label = options.remove("--label").unwrap_or_else(|| "heavy command".into());
                    let compatibility = options.remove("--compatibility");
                    let session = options
                        .remove("--session")
                        .or_else(|| crate::paths::env_var("SESSION").and_then(|value| value.into_string().ok()))
                        .map(|v| v.parse::<u64>())
                        .transpose()?;
                    let pid = options
                        .remove("--pid")
                        .map(|v| v.parse::<u32>())
                        .transpose()?
                        .unwrap_or_else(|| unsafe { libc::getppid() } as u32);
                    let wait = options.remove("--max-wait").map(|v| v.parse::<f64>()).transpose()?.unwrap_or(3600.0);
                    anyhow::ensure!(wait.is_finite() && wait >= 0.0, "invalid maximum wait");
                    let key = options.remove("--request-key").unwrap_or(crate::util::uuid_v4()?);
                    let data = fleet_call(
                        &mut conn,
                        "slot-hold",
                        serde_json::json!({"pool":pool,"pid":pid,"session":session,"label":label,"compatibility":compatibility,"key":key}),
                    )?;
                    let id = data["id"].as_u64().context("missing slot id")?;
                    let since = std::time::Instant::now();
                    loop {
                        let status = fleet_call(&mut conn, "slot-status", serde_json::json!({"id":id}))?;
                        if status["slot"]["granted_at"].as_u64().is_some() {
                            println!("{data}");
                            break;
                        }
                        if status["slot"].is_null() {
                            bail!("slot request was released")
                        }
                        if since.elapsed().as_secs_f64() >= wait {
                            let _ = fleet_call(&mut conn, "slot-release", serde_json::json!({"key":key}));
                            bail!("slot wait expired")
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                _ => bail!("slot hold|status|release"),
            }
        }
        "status" => {
            let conn = Conn::open(false)?;
            println!("{}", serde_json::to_string_pretty(&conn.state)?);
        }
        "new" => {
            let mut rest = args[1..].to_vec();
            let cwd = take_flag(&mut rest, "--cwd");
            let name = take_flag(&mut rest, "--name");
            let model = take_flag(&mut rest, "--model");
            let explicit_host = take_flag(&mut rest, "--host");
            let tool = parse_tool(rest.first())?;
            let host = explicit_host.or_else(|| rest.get(1).cloned());
            start(ClientMsg::Create {
                tool,
                model,
                effort: None,
                host,
                cwd,
                command: Vec::new(),
                name,
                request: None,
            })?;
        }
        // A terminal session: the owner's shell, or the command after `--`.
        "term" | "terminal" | "shell" => {
            let mut rest = args[1..].to_vec();
            let command = match rest.iter().position(|a| a == "--") {
                Some(at) => rest.split_off(at).into_iter().skip(1).collect(),
                None => Vec::new(),
            };
            let cwd = take_flag(&mut rest, "--cwd");
            let name = take_flag(&mut rest, "--name");
            let host = rest.first().cloned();
            start(ClientMsg::Create {
                tool: Tool::Shell,
                model: None,
                effort: None,
                host,
                cwd,
                command,
                name,
                request: None,
            })?;
        }
        // A Codex Security scan in a scratch worktree (`service/jobs.rs`).
        "scan" => {
            let mut rest = args[1..].to_vec();
            let scope = take_flag(&mut rest, "--scope");
            let host = rest.first().cloned();
            start(ClientMsg::SecurityScan { host, scope })?;
        }
        "scans" => {
            let conn = Conn::open(false)?;
            println!("{}", serde_json::to_string_pretty(&conn.state.scans)?);
        }
        "branches" => {
            let conn = Conn::open(false)?;
            println!("{}", serde_json::to_string_pretty(&conn.state.branches)?);
        }
        // `install <source> [--agents claude,codex,agy] [--host <name>] [--dry-run] [--remove]`.
        "install" => {
            let mut rest = args[1..].to_vec();
            let host = take_flag(&mut rest, "--host");
            let agents = take_flag(&mut rest, "--agents").unwrap_or_default();
            let dry_run = rest.iter().any(|a| a == "--dry-run");
            let remove = rest.iter().any(|a| a == "--remove");
            let source =
                rest.iter().find(|a| !a.starts_with("--")).cloned().context("usage: cepheus ctl install <source>")?;
            let agents = agents.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
            let request = crate::protocol::InstallRequest { source, agents, dry_run, remove };
            let mut conn = Conn::open(false)?;
            conn.send(&ClientMsg::Install { request, host })?;
            match conn.wait(|m| matches!(m, ServerMsg::Installed { .. } | ServerMsg::Notice { error: true, .. }))? {
                ServerMsg::Installed { host, ok, lines } => {
                    println!("{host}:");
                    for line in lines {
                        println!("  {line}");
                    }
                    if !ok {
                        bail!("the install did not complete");
                    }
                }
                ServerMsg::Notice { text, .. } => bail!("{text}"),
                _ => {}
            }
        }
        "skills" => {
            let mut conn = Conn::open(false)?;
            conn.send(&ClientMsg::SyncSkills)?;
            if let ServerMsg::Notice { text, .. } = conn.wait(|m| matches!(m, ServerMsg::Notice { .. }))? {
                println!("{text}");
            }
        }
        "resume" => {
            let tool = parse_tool(args.get(1))?;
            let conversation = args.get(2).context("missing conversation id")?.clone();
            let host = args.get(3).cloned();
            let mut conn = Conn::open(true)?;
            conn.send(&ClientMsg::Resume { tool, conversation, host })?;
            if let ServerMsg::Created { id, .. } = conn.wait(|m| matches!(m, ServerMsg::Created { .. }))? {
                println!("{id}");
            }
        }
        "attention" => {
            let operation = args.get(1).map(String::as_str).unwrap_or("status");
            let action = format!("attention-{operation}");
            let data = work_controls::json_argument(args.get(2))?;
            let mut conn = Conn::open(false)?;
            println!("{}", fleet_call(&mut conn, &action, data)?);
        }
        "send" => {
            if args.get(1).is_some_and(|s| s == "--when-idle" || s == "--idle") {
                let id = parse_id(args.get(2))?;
                let text = unescape(&args[3..].join(" "));
                let key = crate::util::uuid_v4()?;
                let mut conn = Conn::open(false)?;
                anyhow::ensure!(conn.version >= 17, "Update needed: identity-safe attention send requires protocol 17");
                let session = conn.state.sessions.iter().find(|s| s.id == id).context("target session unavailable")?;
                let expected = crate::attention::ExpectedTarget::from_session(session)
                    .context("target process identity unknown; refresh before sending")?;
                let host = session.host.clone().unwrap_or_else(|| "@here".into());
                let args =
                    serde_json::json!({"key":key,"session":id,"text":text,"host":host,"expected_target":expected});
                println!("{}", fleet_call(&mut conn, "attention-send", args)?);
                return Ok(());
            }
            let id = parse_id(args.get(1))?;
            let text = unescape(&args[2..].join(" "));
            let mut conn = Conn::open(false)?;
            // Say exactly what the provider confirmed; none of these means
            // the agent has finished or answered.
            match conn.native_message(id, &text)? {
                Some(crate::protocol::Delivered::Queued) => {
                    println!("Queued: the agent takes the message after its current turn.")
                }
                Some(crate::protocol::Delivered::Steered) => {
                    println!("Steered: the agent accepted the message into its active turn.")
                }
                Some(crate::protocol::Delivered::Started) => {
                    println!("Started: the agent began a turn with the message.")
                }
                Some(crate::protocol::Delivered::Submitted) => {
                    println!("Submitted: the provider accepted the message but did not confirm that a turn started.")
                }
                None => {}
            }
        }
        "keys" => {
            let id = parse_id(args.get(1))?;
            let text = unescape(&args[2..].join(" "));
            let mut conn = Conn::open(false)?;
            conn.send(&ClientMsg::Input { id, data: text })?;
            // Round-trip so the input is processed before we exit.
            conn.send(&ClientMsg::Screen { id, tail: None })?;
            conn.wait(|m| matches!(m, ServerMsg::ScreenText { .. }))?;
        }
        "broadcast" => {
            let text = unescape(&args[1..].join(" "));
            if text.trim().is_empty() {
                bail!("usage: cepheus ctl broadcast <message>");
            }
            let mut conn = Conn::open(false)?;
            if conn.version < crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL {
                bail!("Update needed: direct agent messages require Cepheus protocol 19; nothing was sent");
            }
            conn.reader.get_ref().set_read_timeout(Some(Duration::from_secs(50)))?;
            conn.send(&ClientMsg::Broadcast { running_only: true, ids: vec![], text })?;
            let result = |text: &str| {
                text.starts_with("Sent to")
                    || text.split_once(": ").is_some_and(|(_, tail)| tail.starts_with("Sent to"))
            };
            let reply = conn.wait(|m| matches!(m, ServerMsg::Notice { text, .. } if result(text))).map_err(|e| {
                if result(&e.to_string()) {
                    e
                } else {
                    anyhow::anyhow!("no delivery result arrived ({e}); check the agents before retrying")
                }
            })?;
            // A partial or failed delivery exits non-zero, so a script or
            // agent sees that the broadcast did not reach every host.
            if let ServerMsg::Notice { text, error } = reply {
                if error {
                    bail!("{text}");
                }
                println!("{text}");
            }
        }
        "screen" => {
            let id = parse_id(args.get(1))?;
            let mut conn = Conn::open(false)?;
            conn.send(&ClientMsg::Screen { id, tail: None })?;
            if let ServerMsg::ScreenText { lines, .. } = conn.wait(|m| matches!(m, ServerMsg::ScreenText { .. }))? {
                for line in lines {
                    println!("{line}");
                }
            }
        }
        "cd" => {
            anyhow::ensure!(args.len() == 3, "usage: cepheus ctl cd <id> <directory>");
            let id = parse_id(args.get(1))?;
            let mut conn = Conn::open(false)?;
            anyhow::ensure!(conn.version >= 14, "update the deck service to change directories");
            conn.send(&ClientMsg::ChangeDirectory { id, path: args[2].clone() })?;
            match conn.wait(|m| matches!(m, ServerMsg::DirectoryChanged { id: got, .. } if *got == id))? {
                ServerMsg::DirectoryChanged { error: Some(error), .. } => bail!("{error}"),
                ServerMsg::DirectoryChanged { cwd: Some(cwd), .. } => println!("{cwd}"),
                _ => bail!("the service returned no directory"),
            }
        }
        "close" | "reopen" | "forget" => {
            let id = parse_id(args.get(1))?;
            let mut conn = Conn::open(false)?;
            let msg = match cmd {
                "close" => ClientMsg::Close { id },
                "reopen" => ClientMsg::Reopen { id },
                _ => ClientMsg::Forget { id },
            };
            conn.send(&msg)?;
            conn.send(&ClientMsg::Screen { id, tail: None })?;
            conn.wait(|m| matches!(m, ServerMsg::ScreenText { .. }))?;
        }
        "conversations" => {
            let mut conn = Conn::open(false)?;
            conn.send(&ClientMsg::ListConversations { request: None, query: String::new() })?;
            if let ServerMsg::Conversations { items, .. } =
                conn.wait(|m| matches!(m, ServerMsg::Conversations { .. }))?
            {
                println!("{}", serde_json::to_string_pretty(&items)?);
            }
        }
        other => bail!(
            "unknown ctl command {other:?} (status, new, term, resume, send, broadcast, screen, close, reopen, forget, \
             cd, conversations, scan, scans, branches, install, skills)"
        ),
    }
    Ok(())
}

/// `cepheus restart`: run the service's program again in its own
/// process, as an update does; every session keeps running. The deck's
/// restart key does the same, and also restarts an older service.
pub fn restart() -> i32 {
    let result = (|| -> Result<String> {
        let mut conn = Conn::open(false)?;
        if conn.version < 3 {
            bail!(
                "the running service is an older build (protocol {}) that cannot restart in place; the deck's \
                 :restart command stops it, starts it again and reopens each session",
                conn.version
            );
        }
        conn.send(&ClientMsg::Restart)?;
        // It closes every connection as its program starts again.
        loop {
            match read_msg::<_, ServerMsg>(&mut conn.reader) {
                Ok(Some(ServerMsg::Notice { text, error: true })) => bail!("{text}"),
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(err) => return Err(err).context("waiting for the service to restart"),
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let conn = loop {
            match Conn::open(false) {
                Ok(conn) => break conn,
                Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
                Err(err) => return Err(err).context("connecting after the restart"),
            }
        };
        Ok(format!("cepheus: service restarted (pid {}); every session kept running", conn.pid))
    })();
    match result {
        Ok(line) => {
            println!("{line}");
            0
        }
        Err(err) => {
            eprintln!("cepheus: {err:#}");
            1
        }
    }
}

/// Stop only an empty service. The service checks again before accepting it.
pub(crate) fn stop_connection(conn: &mut Conn, _assume_yes: bool) -> Result<()> {
    if conn.state.sessions.iter().any(|session| session.alive) {
        bail!("{}", crate::protocol::STOP_REFUSED_NOTICE);
    }
    conn.send(&ClientMsg::Shutdown)?;
    while let Some(reply) = read_msg::<_, ServerMsg>(&mut conn.reader)? {
        if let ServerMsg::Notice { text, error: true } = reply {
            bail!("{text}");
        }
    }
    Ok(())
}

pub fn stop(assume_yes: bool) -> i32 {
    let result = (|| -> Result<()> {
        let mut conn = match Conn::open(false) {
            Ok(conn) => conn,
            Err(_) => {
                println!("cepheus: the service is not running");
                return Ok(());
            }
        };
        stop_connection(&mut conn, assume_yes)?;
        println!("cepheus: service stopped");
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("cepheus: {err:#}");
            1
        }
    }
}

#[cfg(test)]
#[path = "tests/ctl.rs"]
mod tests;
