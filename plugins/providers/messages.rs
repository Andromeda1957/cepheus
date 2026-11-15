//! Native owner-message delivery for providers with a shared live-session API.
//! Never use a second agent process with --resume as a messaging transport.
use std::io::Read;
use std::os::unix::{ffi::OsStrExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::{config::Config, protocol::Tool};

#[path = "controls.rs"]
pub mod controls;
#[path = "new_thread.rs"]
pub mod new_thread;
#[path = "opencode_new.rs"]
pub mod opencode_new;

#[derive(Clone)]
pub struct Request {
    program: String,
    context: Vec<String>,
    tool: Tool,
    conversation: String,
    text: String,
    cwd: PathBuf,
    codex_home: PathBuf,
    runtime: Option<u32>,
    runtime_start: Option<u64>,
    owner: bool,
    exact_resume: bool,
    steer: bool,
    client_id: Option<String>,
    environment: Vec<(String, String)>,
}

pub enum Delivery {
    /// The provider's own receipt, which says how far the message got.
    Accepted(crate::protocol::Delivered),
    /// A read-only capability probe established that this CLI lacks the API.
    Unsupported,
    Failed(String),
}

/// Only recognized direct CLI launches can share their endpoint reliably.
/// Unknown wrappers and private runtimes cannot authorize owner delivery.
pub fn request(
    tool: Tool,
    config: &Config,
    conversation: &str,
    text: &str,
    cwd: &Path,
    codex_home: &Path,
) -> Option<Request> {
    // Slash commands belong to the interactive client, rather than its agent.
    let first = text.split_whitespace().next().unwrap_or("");
    if first.strip_prefix('/').is_some_and(|s| !s.contains('/')) {
        return None;
    }
    let command = match tool {
        Tool::Codex if !config.codex_notify && uuid(conversation) => &config.codex_command,
        Tool::OpenCode if super::other::is_opencode_id(conversation) => &config.opencode_command,
        _ => return None,
    };
    let (program, args) = command.split_first()?;
    let expected = if tool == Tool::Codex { "codex" } else { "opencode" };
    if Path::new(program).file_name()?.to_str()? != expected {
        return None;
    }
    let mut context = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let keep = match tool {
            Tool::Codex => ["--remote", "--remote-auth-token-env", "--profile", "-p"].contains(&arg.as_str()),
            Tool::OpenCode => arg == "--server",
            _ => false,
        };
        if keep {
            let value = args.next()?;
            if tool == Tool::Codex && arg == "--remote" && !value.starts_with("unix://") {
                return None;
            }
            context.extend([arg.clone(), value.clone()]);
        } else if (tool == Tool::Codex
            && ["--search", "--yolo", "--dangerously-bypass-approvals-and-sandbox", "--no-alt-screen"]
                .contains(&arg.as_str()))
            || (tool == Tool::OpenCode && arg == "--auto")
        {
            // UI and approval switches do not change the shared server identity.
        } else {
            // In particular --no-daemon/--standalone, -c overrides and unknown
            // endpoint options must never accidentally target another runtime.
            return None;
        }
    }
    Some(Request {
        program: program.clone(),
        context,
        tool,
        conversation: conversation.into(),
        text: text.into(),
        cwd: cwd.into(),
        codex_home: codex_home.into(),
        runtime: None,
        runtime_start: None,
        owner: false,
        exact_resume: false,
        steer: false,
        client_id: None,
        environment: Vec::new(),
    })
}

pub(crate) fn uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(
            |(i, c)| {
                if [8, 13, 18, 23].contains(&i) {
                    c == b'-'
                } else {
                    c.is_ascii_hexdigit()
                }
            },
        )
}

fn native_program(tool: Tool, program: &std::ffi::OsStr) -> bool {
    Path::new(program)
        .file_name()
        .is_some_and(|name| name == tool.key() || (tool == Tool::OpenCode && name == "opencode.exe"))
}

// An OpenCode launcher can advertise the native argv before exec. Pin only
// the loaded native executable, or an explicitly matched interpreter script.
fn native_runtime(tool: Tool, pid: u32, program: &std::ffi::OsStr) -> bool {
    native_program(tool, program)
        && (tool != Tool::OpenCode
            || std::fs::read_link(format!("/proc/{pid}/exe")).ok().is_some_and(|path| {
                let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
                native_program(tool, std::ffi::OsStr::new(name.strip_suffix(" (deleted)").unwrap_or(name)))
            }))
}

pub(crate) const MANAGED_CODEX_ENV: [&str; 5] = [
    "CODEX_MANAGED_PACKAGE_ROOT",
    "CODEX_MANAGED_BY_NPM",
    "CODEX_MANAGED_BY_BUN",
    "CODEX_MANAGED_BY_PNPM",
    "CODEX_MANAGED_BY_VITE_PLUS",
];

fn managed_environment(pid: u32, start: u64) -> anyhow::Result<Vec<(String, String)>> {
    anyhow::ensure!(crate::procs::alive_with_start(pid, Some(start)), "native runtime changed");
    let bytes = std::fs::read(format!("/proc/{pid}/environ"))?;
    anyhow::ensure!(bytes.len() <= 1_048_576, "native environment exceeded its limit");
    let mut values = Vec::new();
    for key in MANAGED_CODEX_ENV {
        let prefix = format!("{key}=");
        if let Some(value) = bytes.split(|byte| *byte == 0).find_map(|item| item.strip_prefix(prefix.as_bytes())) {
            values.push((key.into(), std::str::from_utf8(value)?.into()));
        }
    }
    anyhow::ensure!(crate::procs::alive_with_start(pid, Some(start)), "native runtime changed");
    Ok(values)
}

// Official Codex control endpoints are owned symlinks into its private
// daemon directory. Validate both objects, while retaining short path aliases.
fn owned_socket(
    path: &Path,
    deadline: Instant,
    stop: &AtomicBool,
) -> anyhow::Result<(std::os::unix::net::UnixStream, u32, u64)> {
    use anyhow::{ensure, Context};
    use std::os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt},
    };
    let uid = unsafe { libc::geteuid() };
    let endpoint = std::fs::symlink_metadata(path)?;
    ensure!(
        endpoint.uid() == uid && (endpoint.file_type().is_socket() || endpoint.file_type().is_symlink()),
        "native endpoint is not owned or is not a socket link"
    );
    let target = std::fs::symlink_metadata(path.canonicalize()?)?;
    ensure!(target.file_type().is_socket() && target.uid() == uid, "native endpoint is not an owned socket");
    let stream = crate::remote::streaming::connect_local(path, deadline, stop)?;
    let after = std::fs::symlink_metadata(path)?;
    let connected = std::fs::metadata(path)?;
    ensure!(
        after.uid() == uid
            && after.dev() == endpoint.dev()
            && after.ino() == endpoint.ino()
            && connected.file_type().is_socket()
            && connected.uid() == uid
            && connected.dev() == target.dev()
            && connected.ino() == target.ino(),
        "native endpoint changed during connection"
    );
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED writes the correctly sized kernel credential buffer.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut size,
        )
    };
    ensure!(
        result == 0 && size as usize == std::mem::size_of::<libc::ucred>() && peer.uid == uid && peer.pid > 0,
        "native endpoint ownership could not be verified"
    );
    let pid = peer.pid as u32;
    let start = crate::procs::stat(pid).context("native endpoint process disappeared")?.start_ticks;
    Ok((stream, pid, start))
}

impl Request {
    pub fn for_new(mut self, pid: u32) -> Self {
        self.exact_resume = true;
        self.pin_runtime(pid)
    }

    pub fn pin_runtime(mut self, pid: u32) -> Self {
        self.runtime = Some(pid);
        self.runtime_start = crate::procs::stat(pid).map(|stat| stat.start_ticks);
        self
    }

    /// Owner messages address a captured backend thread, independently of the
    /// conversation the attached TUI originally resumed. The backend must
    /// positively identify the loaded thread before any message is queued.
    pub fn for_owner(mut self, pid: u32) -> Self {
        self.owner = true;
        self.pin_runtime(pid)
    }

    pub fn with_steering(mut self, enabled: bool) -> Self {
        self.steer = enabled;
        self
    }

    pub fn with_client_id(mut self, id: Option<&str>) -> Self {
        self.client_id = id.map(str::to_owned);
        self
    }

    fn runtime_current(&self) -> bool {
        self.runtime.is_none_or(|pid| {
            self.runtime_start.is_some_and(|start| crate::procs::alive_with_start(pid, Some(start)))
                && self.matches_runtime(pid)
        })
    }
    /// A service handover can adopt a process launched under older settings.
    /// Check its actual runtime, including its Codex home, before using an API.
    pub fn matches_runtime(&self, pid: u32) -> bool {
        let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else { return false };
        if bytes.len() > 65_536 {
            return false;
        }
        let args: Vec<_> = bytes
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        let offset = match args.as_slice() {
            [program, ..] if native_runtime(self.tool, pid, program.as_ref()) => 1,
            // Script launchers are useful for locally configured CLI wrappers.
            [_, program, ..] if program == &self.program => 2,
            _ => return false,
        };
        let mut context = Vec::new();
        let mut args = args[offset..].iter();
        let mut directory = false;
        let mut resumed = false;
        while let Some(arg) = args.next() {
            let keep = match self.tool {
                Tool::Codex => ["--remote", "--remote-auth-token-env", "--profile", "-p"].contains(&arg.as_str()),
                Tool::OpenCode => arg == "--server",
                _ => false,
            };
            if keep {
                let Some(value) = args.next() else { return false };
                context.extend([arg.clone(), value.clone()]);
            } else if (self.tool == Tool::Codex
                && ["--search", "--yolo", "--dangerously-bypass-approvals-and-sandbox", "--no-alt-screen"]
                    .contains(&arg.as_str()))
                || (self.tool == Tool::OpenCode && arg == "--auto")
            {
            } else if self.tool == Tool::Codex && arg == "--cd" {
                let Some(value) = args.next() else { return false };
                let path = Path::new(value);
                if directory
                    || !path.is_absolute()
                    || !path.is_dir()
                    || path.canonicalize().ok() != self.cwd.canonicalize().ok()
                {
                    return false;
                }
                directory = true;
            } else if ["--model", "-m"].contains(&arg.as_str()) {
                if args.next().is_none() {
                    return false;
                }
            } else if self.tool == Tool::Codex && arg == "--" {
                // The message the launch submitted, last
                // (`codex resume ID -- MESSAGE`).
                if args.next().is_none() || args.next().is_some() {
                    return false;
                }
            } else if (self.tool == Tool::Codex && arg == "resume")
                || (self.tool == Tool::OpenCode && arg == "--session")
            {
                resumed = true;
                let resumed = args.next();
                if resumed != Some(&self.conversation)
                    && !(self.owner && self.tool == Tool::Codex && resumed.is_some_and(|id| uuid(id)))
                {
                    return false;
                }
            } else {
                return false;
            }
        }
        if context != self.context || (self.exact_resume && !resumed) {
            return false;
        }
        if self.tool == Tool::Codex {
            let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else { return false };
            if env.len() > 1_048_576 {
                return false;
            }
            let value = |key: &[u8]| {
                env.split(|b| *b == 0)
                    .find_map(|v| v.strip_prefix(key))
                    .map(|v| PathBuf::from(std::ffi::OsStr::from_bytes(v)))
            };
            let actual = value(b"CODEX_HOME=").or_else(|| value(b"HOME=").map(|p| p.join(".codex")));
            let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
            if actual.as_deref().map(canonical) != Some(canonical(&self.codex_home)) {
                return false;
            }
        }
        true
    }

    pub fn deliver(&self, deadline: Instant, stop: &AtomicBool) -> Delivery {
        if self.owner && self.steer && self.tool == Tool::Codex {
            let Some(pid) = self.runtime else {
                return Delivery::Failed("native runtime identity is unavailable; nothing was sent".into());
            };
            return match self.control_until(
                pid,
                controls::Control::OwnerInput { text: self.text.clone(), paths: Vec::new() },
                deadline,
                stop,
            ) {
                Ok(value) if value["accepted"] == true => match serde_json::from_value(value["delivered"].clone()) {
                    Ok(delivered) => Delivery::Accepted(delivered),
                    Err(_) => Delivery::Failed("native input has no delivery receipt; check before retrying".into()),
                },
                Ok(_) => Delivery::Unsupported,
                Err(error) => Delivery::Failed(format!("{error:#}")),
            };
        }
        // Queued automatic input still needs positive native thread/cwd
        // verification, independently of its non-steering delivery intent.
        if self.owner && self.tool == Tool::Codex {
            let Some(pid) = self.runtime else {
                return Delivery::Failed("native runtime identity is unavailable; nothing was sent".into());
            };
            match self.control_until(pid, controls::Control::Capabilities, deadline, stop) {
                Ok(value) if value["supported"] == true => {}
                Ok(_) => return Delivery::Unsupported,
                Err(error) => return Delivery::Failed(error.to_string()),
            }
        }
        let probe = match self.tool {
            Tool::Codex => vec!["queue".into(), "--help".into()],
            Tool::OpenCode => vec!["api".into(), "--help".into()],
            _ => return Delivery::Unsupported,
        };
        let help = match self.run(&probe, deadline, stop) {
            Ok((true, help)) => help,
            Ok((false, _)) => return Delivery::Unsupported,
            Err(error) => return Delivery::Failed(error),
        };
        let supported = match self.tool {
            Tool::Codex => help.contains("--thread") && help.contains("--message"),
            Tool::OpenCode => help.contains("--data") && help.contains("--param"),
            _ => false,
        };
        if !supported {
            return Delivery::Unsupported;
        }
        if self.tool == Tool::OpenCode {
            // Prompting an id the server does not know would start a session
            // the owner's TUI is not showing, so it must already exist.
            let get =
                vec!["api".into(), "session.get".into(), "--param".into(), format!("sessionID={}", self.conversation)];
            match self.run(&get, deadline, stop) {
                Ok((success, body)) => {
                    let value = serde_json::from_str::<serde_json::Value>(&body).unwrap_or_default();
                    if !success
                        && value["_tag"] == "SessionNotFoundError"
                        && value["sessionID"].as_str() == Some(&self.conversation)
                    {
                        return Delivery::Failed("the OpenCode session does not exist yet; nothing was sent".into());
                    }
                    if !success || value["data"]["id"].as_str() != Some(&self.conversation) {
                        return Delivery::Failed(
                            "could not verify the existing native session; nothing was submitted".into(),
                        );
                    }
                }
                Err(error) => return Delivery::Failed(error),
            }
        }
        let args = match self.tool {
            Tool::Codex => vec![
                "queue".into(),
                "--thread".into(),
                self.conversation.clone(),
                "--message".into(),
                self.text.clone(),
            ],
            Tool::OpenCode => vec![
                "api".into(),
                "session.prompt".into(),
                "--param".into(),
                format!("sessionID={}", self.conversation),
                "--data".into(),
                serde_json::json!({"text": self.text, "delivery": "queue", "resume": true}).to_string(),
            ],
            _ => unreachable!(),
        };
        // Once attempted, even a timeout could mean admission succeeded, so an
        // uncertain result is reported and never sent a second time.
        match self.run(&args, deadline, stop) {
            Ok((true, receipt)) => {
                let accepted = match self.tool {
                    // queue without --resume exits successfully only after enqueue.
                    Tool::Codex => true,
                    Tool::OpenCode => serde_json::from_str::<serde_json::Value>(&receipt).ok().is_some_and(|v| {
                        v["data"]["sessionID"].as_str() == Some(&self.conversation)
                            && v["data"]["id"].as_str().is_some()
                            && v["data"]["type"] == "user"
                    }),
                    _ => false,
                };
                if accepted {
                    match self.tool {
                        Tool::Codex => match self.wake_codex(deadline, stop) {
                            Ok(delivered) => Delivery::Accepted(delivered),
                            Err(_) => Delivery::Failed(
                                "message queued, but its start could not be confirmed; check the conversation before retrying"
                                    .into(),
                            ),
                        },
                        // The server's inbox runs it after any current turn.
                        _ => Delivery::Accepted(crate::protocol::Delivered::Queued),
                    }
                } else {
                    Delivery::Failed(
                        "native delivery returned no recognized receipt; check the conversation before retrying".into(),
                    )
                }
            }
            Ok((false, _)) => Delivery::Failed("native delivery failed; check the conversation before retrying".into()),
            Err(error) => Delivery::Failed(error),
        }
    }

    /// Enqueue auto-starts ordinary idle threads, but Codex deliberately leaves
    /// interrupted threads asleep. An explicit new owner send resumes that FIFO.
    /// queue/start atomically refuses active turns; it never steers or interrupts.
    /// Says whether the message started a turn or waits behind the current one.
    fn wake_codex(&self, deadline: Instant, stop: &AtomicBool) -> anyhow::Result<crate::protocol::Delivered> {
        use crate::protocol::Delivered;
        use serde_json::{json, Value};
        use std::os::unix::net::UnixStream;
        use tungstenite::{Message, WebSocket};
        let endpoint = self.context.windows(2).find(|pair| pair[0] == "--remote").map(|pair| pair[1].as_str());
        let socket = match endpoint.and_then(|e| e.strip_prefix("unix://")).filter(|p| !p.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => self.codex_home.join("app-server-control/app-server-control.sock"),
        };
        let until = deadline.min(Instant::now() + Duration::from_secs(8));
        anyhow::ensure!(Instant::now() < until && !stop.load(Ordering::Relaxed), "activation canceled or expired");
        let stream = if self.owner {
            owned_socket(&socket, until, stop)?.0
        } else {
            crate::remote::streaming::connect_local(&socket, until, stop)?
        };
        stream.set_read_timeout(Some(until.saturating_duration_since(Instant::now())))?;
        stream.set_write_timeout(Some(until.saturating_duration_since(Instant::now())))?;
        let config = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(1 << 20))
            .max_frame_size(Some(1 << 20));
        let (mut ws, _) = tungstenite::client::client_with_config("ws://localhost/", stream, Some(config))?;
        ws.get_ref().set_read_timeout(Some(Duration::from_millis(200)))?;
        let rpc = |ws: &mut WebSocket<UnixStream>, id: u64, method: &str, params: Value| -> anyhow::Result<Value> {
            anyhow::ensure!(
                self.runtime_current() && !stop.load(Ordering::SeqCst),
                "the captured native target changed"
            );
            ws.send(Message::text(json!({"id":id,"method":method,"params":params}).to_string()))?;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                match ws.read() {
                    Ok(Message::Text(text)) => {
                        let v: Value = serde_json::from_str(&text)?;
                        if v.get("id").and_then(Value::as_u64) == Some(id) && v.get("method").is_none() {
                            return Ok(v);
                        }
                    }
                    Ok(Message::Close(_)) => anyhow::bail!("activation connection closed"),
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(error))
                        if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            anyhow::bail!("activation deadline expired")
        };
        let init = rpc(
            &mut ws,
            1,
            "initialize",
            json!({"clientInfo":{"name":"cepheus","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),
        )?;
        anyhow::ensure!(init.get("result").is_some(), "could not initialize activation");
        ws.send(Message::text(json!({"method":"initialized"}).to_string()))?;
        let read = rpc(&mut ws, 2, "thread/read", json!({"threadId":self.conversation,"includeTurns":false}))?;
        anyhow::ensure!(
            read["result"]["thread"]["id"].as_str() == Some(&self.conversation),
            "session identity changed"
        );
        match read["result"]["thread"]["status"]["type"].as_str() {
            Some("active") => return Ok(Delivered::Queued),
            Some("idle") => {}
            _ => anyhow::bail!("queued session is not available"),
        }
        anyhow::ensure!(!stop.load(Ordering::Relaxed), "activation canceled");
        let start = rpc(&mut ws, 3, "thread/queue/start", json!({"threadId":self.conversation}))?;
        if start["result"]["turn"]["status"] == "inProgress" {
            return Ok(Delivered::Started);
        }
        // Automatic dispatch can win the race after our idle read: an empty
        // queue means it already took the message into a turn, while a pending
        // turn may still be ahead of it.
        anyhow::ensure!(start["error"]["code"] == -32600, "could not activate queued message");
        match start["error"]["message"].as_str() {
            Some("queue is empty") => Ok(Delivered::Started),
            Some("thread already has an active or pending turn") => Ok(Delivered::Queued),
            _ => anyhow::bail!("could not activate queued message"),
        }
    }

    fn run(&self, args: &[String], deadline: Instant, stop: &AtomicBool) -> Result<(bool, String), String> {
        self.run_limit(args, deadline, stop, 65_536)
    }

    fn run_limit(
        &self,
        args: &[String],
        deadline: Instant,
        stop: &AtomicBool,
        limit: usize,
    ) -> Result<(bool, String), String> {
        if !self.runtime_current() {
            return Err("the captured native runtime changed; nothing further was sent".into());
        }
        if stop.load(Ordering::Relaxed) {
            return Err("delivery canceled; check the conversation before retrying".into());
        }
        if Instant::now() >= deadline {
            return Err("delivery expired before submission; nothing further was sent".into());
        }
        let mut command = if self.runtime.is_some() {
            let pid = self.runtime.ok_or("native runtime identity is unavailable")?;
            let bytes =
                std::fs::read(format!("/proc/{pid}/cmdline")).map_err(|_| "native runtime command is unavailable")?;
            let parts: Vec<_> = bytes.split(|byte| *byte == 0).filter(|part| !part.is_empty()).collect();
            // Use the executable already loaded by the validated process. A
            // PATH launcher may upgrade the tool before every invocation.
            let mut command = Command::new(format!("/proc/{pid}/exe"));
            if parts.first().is_some_and(|part| native_runtime(self.tool, pid, std::ffi::OsStr::from_bytes(part))) {
            } else if parts.get(1).is_some_and(|part| *part == self.program.as_bytes()) {
                // An explicitly configured script runs through that process's
                // exact interpreter, never through a different PATH launcher.
                command.arg(&self.program);
            } else {
                return Err("native runtime executable could not be pinned; nothing was sent".into());
            }
            command
        } else {
            Command::new(&self.program)
        };
        command
            .args(args)
            .args(&self.context)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(crate::service::inherited_session_var) {
                command.env_remove(name);
            }
        }
        for name in crate::paths::env_names("SOCKET") {
            command.env_remove(name);
        }
        if self.tool == Tool::Codex {
            command.env("CODEX_HOME", &self.codex_home);
            for key in MANAGED_CODEX_ENV {
                command.env_remove(key);
            }
            let environment = if let (Some(pid), Some(start)) = (self.runtime, self.runtime_start) {
                managed_environment(pid, start).map_err(|error| error.to_string())?
            } else {
                self.environment.clone()
            };
            command.envs(environment);
        }
        // This may start the account's Codex daemon, which runs every turn.
        crate::client_temp::contain(&mut command, "deliver").map_err(|error| {
            format!("could not start native delivery ({error:#}); nothing was typed into the terminal")
        })?;
        let mut child = command
            .spawn()
            .map_err(|_| "could not start native delivery; nothing was typed into the terminal".to_string())?;
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.take((limit + 1) as u64).read_to_end(&mut bytes);
            let _ = tx.send((result.is_ok() && bytes.len() <= limit, bytes));
        });
        // A first account daemon installs its native runtime package. New
        // already owns a bounded deadline; ordinary message commands keep 10s.
        let daemon_start = self.runtime.is_none()
            && self.tool == Tool::Codex
            && args.iter().map(String::as_str).eq(["app-server", "daemon", "start"]);
        let until = if daemon_start { deadline } else { deadline.min(Instant::now() + Duration::from_secs(10)) };
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < until && !stop.load(Ordering::Relaxed) => {
                    thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    // Only this command's newly created process group is ours.
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.wait();
                    return Err(if stop.load(Ordering::Relaxed) {
                        "delivery canceled; check the conversation before retrying"
                    } else {
                        "native delivery timed out; check the conversation before retrying"
                    }
                    .into());
                }
            }
        };
        if stop.load(Ordering::Relaxed) {
            return Err("delivery canceled; check the conversation before retrying".into());
        }
        let (complete, bytes) = rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "native delivery receipt is incomplete; check the conversation before retrying".to_string())?;
        if !complete {
            return Err("native delivery receipt exceeded its limit".into());
        }
        Ok((status.success(), String::from_utf8_lossy(&bytes).into_owned()))
    }
}

#[cfg(test)]
#[path = "../../src/tests/native_opencode_messages.rs"]
mod tests;
