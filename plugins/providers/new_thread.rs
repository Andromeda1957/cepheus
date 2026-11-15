//! Prepare a fresh native thread before launching its first interactive client.
//! Empty Codex threads have no rollout yet, so discovery cannot bind that client.
use super::{request, uuid, Request};
use crate::{config::Config, protocol::Tool, service::StartModel};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

pub struct Prepared {
    pub conversation: String,
    pub program: PathBuf,
    name: String,
    pub settings: StartModel,
    pub environment: Vec<(String, String)>,
    request: Request,
    connection: Connection,
}

struct Connection {
    socket: WebSocket<UnixStream>,
    pid: u32,
    start: u64,
    next: u64,
}

impl Connection {
    fn current(&self, deadline: Instant, stop: &AtomicBool) -> Result<()> {
        ensure!(!stop.load(Ordering::SeqCst) && Instant::now() < deadline, "native New canceled or expired");
        ensure!(crate::procs::alive_with_start(self.pid, Some(self.start)), "native backend changed during New");
        Ok(())
    }

    /// Settings ACK means queued. The provider's canonical applied-settings
    /// record is the durability receipt; equal-value wire notices are deduped.
    fn wait_settings(
        &self,
        thread: &Value,
        request: &Request,
        settings: &StartModel,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<()> {
        use std::io::{BufRead, Read};
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let path = PathBuf::from(thread["path"].as_str().context("native New has no settings receipt path")?);
        let account = request.codex_home.canonicalize()?;
        ensure!(
            account.metadata()?.uid() == unsafe { libc::geteuid() } && account.is_dir(),
            "native New settings account is not an owned directory"
        );
        ensure!(
            path.is_absolute() && path.canonicalize()? == path && path.starts_with(account.join("sessions")),
            "native New settings receipt is outside its canonical account"
        );
        let before = std::fs::symlink_metadata(&path)?;
        ensure!(
            before.is_file() && before.uid() == unsafe { libc::geteuid() } && before.mode() & 0o002 == 0,
            "native New settings receipt is not an owned regular file"
        );
        let file =
            std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path)?;
        let opened = file.metadata()?;
        ensure!(
            opened.is_file()
                && opened.uid() == unsafe { libc::geteuid() }
                && (opened.dev(), opened.ino()) == (before.dev(), before.ino()),
            "native New settings receipt identity changed"
        );
        const LIMIT: u64 = 1 << 20;
        ensure!(opened.len() <= LIMIT, "native New settings receipt is too large");
        let mut reader = std::io::BufReader::new(file.take(LIMIT + 1));
        let mut line = Vec::new();
        let mut total = 0;
        let mut metadata_seen = false;
        let mut latest_settings = None;
        loop {
            self.current(deadline, stop)?;
            let count = reader.read_until(b'\n', &mut line)?;
            total += count as u64;
            ensure!(total <= LIMIT, "native New settings receipt is too large");
            if line.last() == Some(&b'\n') {
                let record: Value = serde_json::from_slice(&line)?;
                let payload = &record["payload"];
                if record["type"] == "session_meta" {
                    ensure!(
                        payload["id"].as_str() == Some(request.conversation.as_str())
                            && payload["cwd"].as_str().is_some_and(|cwd| Path::new(cwd) == request.cwd),
                        "native New settings metadata identity changed"
                    );
                    metadata_seen = true;
                } else if record["type"] == "event_msg" && payload["type"] == "thread_settings_applied" {
                    let snapshot = &payload["thread_settings"];
                    ensure!(
                        metadata_seen
                            && payload["thread_id"].as_str() == Some(request.conversation.as_str())
                            && snapshot["cwd"].as_str().is_some_and(|cwd| Path::new(cwd) == request.cwd),
                        "native New persisted settings identity changed"
                    );
                    latest_settings = Some(
                        snapshot["model"].as_str() == settings.model.as_deref()
                            && match settings.effort.as_deref() {
                                Some(effort) => snapshot["reasoning_effort"].as_str() == Some(effort),
                                None => snapshot.get("reasoning_effort").is_none_or(Value::is_null),
                            },
                    );
                }
                line.clear();
            }
            if count == 0 {
                let current = reader.get_ref().get_ref().metadata()?;
                ensure!(
                    current.len() <= LIMIT && current.len() >= total,
                    "native New settings receipt was truncated or is too large"
                );
                if current.len() > total {
                    continue;
                }
                if line.is_empty() && latest_settings.is_some() {
                    ensure!(
                        latest_settings == Some(true),
                        "native New persisted settings do not match its selected settings"
                    );
                    let after = std::fs::symlink_metadata(&path)?;
                    let current_account = request.codex_home.canonicalize()?;
                    ensure!(
                        current_account == account
                            && current_account.metadata()?.uid() == unsafe { libc::geteuid() }
                            && path.starts_with(current_account.join("sessions"))
                            && path.canonicalize()? == path
                            && after.is_file()
                            && after.uid() == unsafe { libc::geteuid() }
                            && after.mode() & 0o002 == 0
                            && (after.dev(), after.ino()) == (opened.dev(), opened.ino())
                            && after.len() == total,
                        "native New settings receipt changed during verification"
                    );
                    self.current(deadline, stop)?;
                    return Ok(());
                }
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(20)));
            }
        }
    }

    fn rpc(&mut self, method: &str, params: Value, deadline: Instant, stop: &AtomicBool) -> Result<Value> {
        self.current(deadline, stop)?;
        self.next += 1;
        let id = self.next;
        self.socket.send(Message::text(json!({"id":id,"method":method,"params":params}).to_string()))?;
        loop {
            self.current(deadline, stop)?;
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let reply: Value = serde_json::from_str(&text)?;
                    if reply.get("method").is_none() && reply["id"].as_u64() == Some(id) {
                        self.current(deadline, stop)?;
                        return reply
                            .get("result")
                            .cloned()
                            .context("native New was refused; no terminal fallback was attempted");
                    }
                }
                Ok(Message::Close(_)) => bail!("native New connection closed; nothing was retried"),
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn connect(path: &Path, deadline: Instant, stop: &AtomicBool) -> Result<Connection> {
    let (stream, pid, start) = super::owned_socket(path, deadline, stop)?;
    stream.set_read_timeout(Some(deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(2))))?;
    stream.set_write_timeout(Some(deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(2))))?;
    let config =
        tungstenite::protocol::WebSocketConfig::default().max_message_size(Some(1 << 20)).max_frame_size(Some(1 << 20));
    let (socket, _) = tungstenite::client::client_with_config("ws://localhost/", stream, Some(config))?;
    socket.get_ref().set_read_timeout(Some(Duration::from_millis(200)))?;
    let mut connection = Connection { socket, pid, start, next: 0 };
    connection.rpc("initialize", json!({"clientInfo":{"name":"cepheus-new","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}), deadline, stop)?;
    connection.socket.send(Message::text(json!({"method":"initialized"}).to_string()))?;
    Ok(connection)
}

fn executable(program: &str) -> Result<PathBuf> {
    let path = if Path::new(program).is_absolute() {
        PathBuf::from(program)
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(program))
            .find(|path| path.is_file())
            .context("Codex executable is unavailable")?
    };
    Ok(path.canonicalize()?)
}

fn native_binary(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0; 4];
    std::fs::File::open(path).and_then(|mut file| file.read_exact(&mut magic)).is_ok() && magic == *b"\x7fELF"
}

struct Located {
    program: PathBuf,
    version: Option<String>,
    environment: Vec<(String, String)>,
}

fn owned_file(path: &Path) -> Result<std::fs::Metadata> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && [0, unsafe { libc::geteuid() }].contains(&metadata.uid()) && metadata.mode() & 0o002 == 0,
        "the installed Codex package is not an owned regular file"
    );
    Ok(metadata)
}

fn package(path: &Path) -> Result<Value> {
    ensure!(owned_file(path)?.len() <= 65_536, "Codex package metadata is too large");
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn locate_native(request: &Request, deadline: Instant, stop: &AtomicBool) -> Result<Located> {
    let mut program = executable(&request.program)?;
    owned_file(&program)?;
    if program.file_name().is_some_and(|name| name == "codex.js") {
        let root = program
            .parent()
            .filter(|path| path.file_name().is_some_and(|name| name == "bin"))
            .and_then(Path::parent)
            .context("unsupported Codex JavaScript launcher")?;
        let metadata = package(&root.join("package.json"))?;
        ensure!(metadata["name"] == "@openai/codex", "unsupported Codex JavaScript package");
        let version = metadata["version"]
            .as_str()
            .filter(|value| !value.is_empty() && value.len() < 64)
            .context("Codex package version is unavailable")?
            .to_owned();
        let alias = root.join("node_modules/@openai/codex-linux-x64");
        let native = package(&alias.join("package.json"))?;
        ensure!(
            cfg!(target_arch = "x86_64")
                && metadata["optionalDependencies"]["@openai/codex-linux-x64"]
                    == format!("npm:@openai/codex@{version}-linux-x64")
                && native["name"] == "@openai/codex"
                && native["version"] == format!("{version}-linux-x64"),
            "unsupported native Codex package layout"
        );
        let binary = alias.join("vendor/x86_64-unknown-linux-musl/bin/codex").canonicalize()?;
        ensure!(binary.starts_with(root) && native_binary(&binary), "the packaged native Codex binary is unavailable");
        owned_file(&binary)?;
        return Ok(Located {
            program: binary,
            version: Some(version),
            environment: vec![
                ("CODEX_MANAGED_PACKAGE_ROOT".into(), root.to_string_lossy().into_owned()),
                ("CODEX_MANAGED_BY_NPM".into(), "1".into()),
            ],
        });
    }
    let mise = native_binary(&program) && program.file_name().is_some_and(|name| name == "mise");
    let shell_mise = !native_binary(&program)
        && owned_file(&program)?.len() <= 4096
        && std::fs::read_to_string(&program).ok().is_some_and(|source| {
            source.contains("mise upgrade") && source.contains("exec mise x") && source.contains("codex")
        });
    if mise || shell_mise {
        // A Codex shim may itself be the Mise ELF dispatcher. Never execute
        // the configured upgrade wrapper merely to locate its installed tool.
        let mut lookup = request.clone();
        lookup.program = if mise {
            program.to_string_lossy().into_owned()
        } else {
            executable("mise")?.to_string_lossy().into_owned()
        };
        lookup.context.clear();
        let (success, output) =
            lookup.run(&["which".into(), "codex".into()], deadline, stop).map_err(anyhow::Error::msg)?;
        ensure!(success, "could not resolve the installed Codex binary");
        program = PathBuf::from(output.trim()).canonicalize()?;
        owned_file(&program)?;
    }
    ensure!(
        native_binary(&program) && program.file_name().is_some_and(|name| name == "codex"),
        "native New requires a recognized installed Codex executable"
    );
    Ok(Located { program, version: None, environment: Vec::new() })
}

fn thread(result: &Value, conversation: &str, cwd: &Path, settings: &StartModel) -> Result<()> {
    let thread = &result["thread"];
    ensure!(thread["id"].as_str() == Some(conversation), "native New returned another thread");
    ensure!(
        matches!(thread.pointer("/status/type").and_then(Value::as_str), Some("idle" | "active")),
        "native New thread is not loaded"
    );
    ensure!(
        std::fs::canonicalize(thread["cwd"].as_str().context("native New has no directory")?)? == cwd,
        "native New directory changed"
    );
    for (key, expected) in [("model", &settings.model), ("reasoningEffort", &settings.effort)] {
        if let Some(expected) = expected {
            ensure!(
                thread[key].as_str().or_else(|| result[key].as_str()) == Some(expected),
                "native New {key} was not preserved"
            );
        }
    }
    Ok(())
}

pub fn prepare(
    config: &Config,
    cwd: &Path,
    home: &Path,
    settings: &StartModel,
    name: &str,
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<Prepared> {
    let cwd = cwd.canonicalize()?;
    let mut request = request(Tool::Codex, config, "00000000-0000-4000-8000-000000000000", "", &cwd, home)
        .context("this Codex launch configuration cannot initialize native New")?;
    ensure!(
        !request.context.iter().any(|arg| ["--profile", "-p", "--remote-auth-token-env"].contains(&arg.as_str())),
        "native New does not support this profile or authenticated endpoint yet"
    );
    let remote = request.context.windows(2).find(|pair| pair[0] == "--remote").map(|pair| pair[1].clone());
    let path = if let Some(remote) = &remote {
        PathBuf::from(remote.strip_prefix("unix://").context("native New requires a Unix endpoint")?)
    } else {
        home.join("app-server-control/app-server-control.sock")
    };
    ensure!(path.is_absolute(), "native New requires an absolute endpoint");
    let mut connection = match std::fs::symlink_metadata(&path) {
        Ok(_) => match connect(&path, deadline, stop) {
            Ok(connection) => Some(connection),
            Err(error)
                if remote.is_none()
                    && error.chain().any(|cause| {
                        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                            matches!(error.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound)
                        })
                    }) =>
            {
                None
            }
            Err(error) => return Err(error),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(remote.is_none(), "the configured native endpoint is unavailable");
            None
        }
        Err(error) => return Err(error.into()),
    };
    let located = if remote.is_some() {
        Located { program: executable(&request.program)?, version: None, environment: Vec::new() }
    } else if let Some(connection) = &connection {
        let program = std::fs::read_link(format!("/proc/{}/exe", connection.pid))?;
        owned_file(&program)?;
        ensure!(
            native_binary(&program) && program.file_name().is_some_and(|name| name == "codex"),
            "the loaded native Codex executable is unavailable"
        );
        Located { program, version: None, environment: super::managed_environment(connection.pid, connection.start)? }
    } else {
        locate_native(&request, deadline, stop)?
    };
    let program = located.program;
    request.program = program.to_string_lossy().into_owned();
    request.environment = located.environment;
    if remote.is_none() {
        let (success, version) = request.run(&["--version".into()], deadline, stop).map_err(anyhow::Error::msg)?;
        let mut fields = version.split_whitespace();
        ensure!(
            success
                && fields.next() == Some("codex-cli")
                && fields
                    .next()
                    .is_some_and(|version| located.version.as_deref().is_none_or(|expected| expected == version))
                && fields.next().is_none(),
            "the installed native Codex version could not be verified"
        );
    }
    if connection.is_none() {
        let (success, _) = request
            .run(&["app-server".into(), "daemon".into(), "start".into()], deadline, stop)
            .map_err(anyhow::Error::msg)?;
        ensure!(success, "native Codex daemon could not start; no client was launched");
        connection = Some(connect(&path, deadline, stop)?);
    }
    let mut connection = connection.unwrap();
    let mut params = json!({"cwd":cwd,"ephemeral":false});
    let args = &config.codex_command;
    if args.iter().any(|arg| ["--yolo", "--dangerously-bypass-approvals-and-sandbox"].contains(&arg.as_str())) {
        params["approvalPolicy"] = json!("never");
        params["sandbox"] = json!("danger-full-access");
    }
    if args.iter().any(|arg| arg == "--search") {
        params["config"]["web_search"] = json!("live");
    }
    if let Some(model) = &settings.model {
        params["model"] = json!(model);
    }
    if let Some(effort) = &settings.effort {
        params["config"]["model_reasoning_effort"] = json!(effort);
    }
    let started = connection.rpc("thread/start", params, deadline, stop)?;
    let conversation = started["thread"]["id"]
        .as_str()
        .filter(|id| uuid(id))
        .context("native New returned no valid thread identity")?
        .to_owned();
    let selected = StartModel {
        model: started["model"].as_str().or_else(|| started["thread"]["model"].as_str()).map(str::to_owned),
        effort: started["reasoningEffort"]
            .as_str()
            .or_else(|| started["thread"]["reasoningEffort"].as_str())
            .map(str::to_owned),
    };
    ensure!(selected.model.is_some(), "native New returned no effective model");
    thread(&started, &conversation, &cwd, settings)?;
    // Codex persists a named empty thread before its first turn. This saves
    // the actual Deck display name through the supported provider operation,
    // so the first client can resume it after this preparation socket closes.
    connection.rpc("thread/name/set", json!({"threadId":conversation,"name":name}), deadline, stop)?;
    let read = connection.rpc("thread/read", json!({"threadId":conversation,"includeTurns":false}), deadline, stop)?;
    thread(&read, &conversation, &cwd, &selected)?;
    ensure!(read["thread"]["name"].as_str() == Some(name), "native New name was not preserved");
    request.conversation = conversation.clone();
    let mut saved = json!({"threadId":conversation,"model":selected.model});
    if let Some(effort) = &selected.effort {
        saved["effort"] = json!(effort);
    }
    connection.rpc("thread/settings/update", saved, deadline, stop)?;
    connection.wait_settings(&read["thread"], &request, &selected, deadline, stop)?;
    Ok(Prepared {
        conversation,
        program,
        name: name.to_owned(),
        settings: selected,
        environment: request.environment.clone(),
        request,
        connection,
    })
}

impl Prepared {
    pub fn verify(mut self, pid: u32, deadline: Instant, stop: &AtomicBool) -> Result<()> {
        self.request = self.request.for_new(pid);
        // A new client's argv/environment can still settle after spawn. Wait
        // for this exact process and prepared thread, never adopt another PID
        // or refresh its start identity while waiting.
        while !self.request.runtime_current() {
            self.connection.current(deadline, stop)?;
            ensure!(
                self.request.runtime_start.is_some_and(|start| crate::procs::alive_with_start(pid, Some(start))),
                "the first native client does not match its prepared thread"
            );
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(20)));
        }
        let read = self.connection.rpc(
            "thread/read",
            json!({"threadId":self.conversation,"includeTurns":false}),
            deadline,
            stop,
        )?;
        thread(&read, &self.conversation, &self.request.cwd, &self.settings)?;
        ensure!(
            read["thread"]["name"].as_str() == Some(self.name.as_str()),
            "native New name changed during attachment"
        );
        ensure!(self.request.runtime_current(), "the first native client changed during verification");
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../src/tests/native_launchers.rs"]
mod tests;
