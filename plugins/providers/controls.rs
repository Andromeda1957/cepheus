//! Thread-bound controls for an already running Codex client. The terminal's
//! composer is never read, changed or used as a transport.
use super::Request;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

pub enum Control {
    Capabilities,
    Settings { model: Option<String>, effort: Option<String> },
    Question { message: String },
    OwnerInput { text: String, paths: Vec<std::path::PathBuf> },
}

struct Connection<'a> {
    socket: WebSocket<UnixStream>,
    request: &'a Request,
    pid: u32,
    deadline: Instant,
    stop: &'a AtomicBool,
    next: u64,
}

impl Connection<'_> {
    fn current(&self) -> Result<()> {
        ensure!(
            !self.stop.load(Ordering::Relaxed) && Instant::now() < self.deadline,
            "native control canceled or expired"
        );
        ensure!(
            self.request.runtime_current() && self.request.matches_runtime(self.pid),
            "the hosted runtime changed before native control"
        );
        ensure!(
            !self.stop.load(Ordering::SeqCst) && Instant::now() < self.deadline,
            "native control canceled or expired"
        );
        Ok(())
    }

    fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.current()?;
        self.next += 1;
        let id = self.next;
        self.socket.send(Message::text(json!({"id":id,"method":method,"params":params}).to_string()))?;
        while Instant::now() < self.deadline && !self.stop.load(Ordering::Relaxed) {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let reply: Value = serde_json::from_str(&text)?;
                    if reply.get("method").is_none() && reply["id"].as_u64() == Some(id) {
                        self.current()?;
                        return Ok(reply);
                    }
                }
                Ok(Message::Close(_)) => {
                    bail!("native control connection closed; check the conversation before retrying")
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {}
                Err(error) => return Err(error.into()),
            }
        }
        bail!("native control outcome unknown; check the conversation before retrying")
    }
}

fn result(reply: &Value) -> Result<&Value> {
    reply.get("result").context("native control was refused; nothing was retried through the terminal")
}

fn unsupported(reply: &Value) -> bool {
    // A JSON-RPC method-not-found rejection cannot have executed the operation.
    // Network failures, ordinary rejections and malformed receipts are never a
    // reason to attempt the same mutation through the terminal.
    reply.pointer("/error/code").and_then(Value::as_i64) == Some(-32601)
}

impl Request {
    pub fn control_until(&self, pid: u32, action: Control, deadline: Instant, stop: &AtomicBool) -> Result<Value> {
        ensure!(!stop.load(Ordering::SeqCst) && Instant::now() < deadline, "native control canceled or expired");
        // This adapter deliberately implements only local unauthenticated Unix
        // transport. Do not probe an authenticated endpoint without its token,
        // and never mistake a failed authentication for an absent method.
        if self.context.iter().any(|arg| arg == "--remote-auth-token-env") {
            return Ok(json!({"supported":false}));
        }
        let endpoint = self.context.windows(2).find(|pair| pair[0] == "--remote").map(|pair| pair[1].as_str());
        let socket = if let Some(endpoint) = endpoint {
            let Some(path) = endpoint.strip_prefix("unix://").filter(|p| std::path::Path::new(p).is_absolute()) else {
                return Ok(json!({"supported":false}));
            };
            std::path::PathBuf::from(path)
        } else {
            self.codex_home.join("app-server-control/app-server-control.sock")
        };
        let stream = if self.owner {
            super::owned_socket(&socket, deadline, stop)?.0
        } else {
            crate::remote::streaming::connect_local(&socket, deadline, stop)?
        };
        stream.set_read_timeout(Some(Duration::from_millis(200)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        let config = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(1 << 20))
            .max_frame_size(Some(1 << 20));
        let (socket, _) = tungstenite::client::client_with_config("ws://localhost/", stream, Some(config))?;
        let mut connection = Connection { socket, request: self, pid, deadline, stop, next: 0 };
        result(&connection.rpc(
            "initialize",
            json!({
                "clientInfo":{"name":"cepheus-controls","version":env!("CARGO_PKG_VERSION")},
                "capabilities":{"experimentalApi":true}
            }),
        )?)?;
        connection.socket.send(Message::text(json!({"method":"initialized"}).to_string()))?;
        let read = connection.rpc("thread/read", json!({"threadId":self.conversation,"includeTurns":false}))?;
        if unsupported(&read) {
            return Ok(json!({"supported":false}));
        }
        let current = &result(&read)?["thread"];
        ensure!(current["id"].as_str() == Some(self.conversation.as_str()), "native thread identity changed");
        let status = current.pointer("/status/type").and_then(Value::as_str);
        ensure!(matches!(status, Some("active" | "idle")), "the thread is not loaded in this runtime");
        if self.owner {
            let cwd = current["cwd"].as_str().context("native thread directory is unavailable; nothing was sent")?;
            ensure!(
                std::fs::canonicalize(cwd)? == std::fs::canonicalize(&self.cwd)?,
                "native thread directory changed; nothing was sent"
            );
        }
        match action {
            Control::Capabilities => Ok(json!({"supported":true,"transport":"codex-app-server",
                "conversation":self.conversation,"controls":["settings","question_reply"]})),
            Control::Settings { model, effort } => {
                let mut params = json!({"threadId":self.conversation});
                if let Some(model) = model {
                    params["model"] = json!(model);
                }
                if let Some(effort) = effort {
                    params["effort"] = json!(effort);
                }
                let reply = connection.rpc("thread/settings/update", params)?;
                if unsupported(&reply) {
                    return Ok(json!({"supported":false}));
                }
                ensure!(result(&reply)?.is_object(), "native settings has no acceptance receipt");
                Ok(json!({"supported":true,"accepted":true,"applies_to":"subsequent_turns"}))
            }
            Control::OwnerInput { text, paths } => {
                ensure!(
                    self.owner && (!text.trim().is_empty() || !paths.is_empty()),
                    "native owner input requires an owned target"
                );
                let client_id = self.client_id.clone().map(Ok).unwrap_or_else(crate::util::uuid_v4)?;
                let mut input = Vec::with_capacity(paths.len() + 1);
                if !text.trim().is_empty() {
                    input.push(json!({"type":"text","text":text,"text_elements":[]}));
                }
                for path in paths {
                    input.push(json!({"type":"localImage","path":path}));
                }
                let active = if status == Some("active") {
                    let turns = connection.rpc(
                        "thread/turns/list",
                        json!({"threadId":self.conversation,
                        "limit":1,"sortDirection":"desc","itemsView":"summary"}),
                    )?;
                    let turn = &result(&turns)?["data"][0];
                    (turn["status"] == "inProgress").then(|| turn["id"].as_str().map(str::to_owned)).flatten()
                } else {
                    None
                };
                // The native start endpoint atomically starts an idle thread or
                // steers its active turn. Unlike queue/add followed by a wake,
                // it cannot strand a follow-up when the turn changes mid-send.
                // Never retry an uncertain admission or fall back to the terminal.
                let reply = connection
                    .rpc(
                        "turn/start",
                        json!({"threadId":self.conversation,
                    "clientUserMessageId":client_id,"input":input}),
                    )
                    .context("input submission status is unknown; check the conversation before retrying")?;
                if unsupported(&reply) {
                    return Ok(json!({"supported":false}));
                }
                let turn = &result(&reply)?["turn"];
                let turn_id = turn["id"].as_str().context("native input has no turn receipt; check before retrying")?;
                ensure!(
                    turn["status"] == "inProgress",
                    "native input has no active-turn receipt; check before retrying"
                );
                // Only name steering when the receipt identifies the turn we
                // observed active. Admission is still confirmed across a race.
                let delivered = if active.as_deref() == Some(turn_id) { "steered" } else { "submitted" };
                Ok(json!({"supported":true,"accepted":true,"delivered":delivered}))
            }
            Control::Question { message } => {
                let input = json!([{"type":"text","text":message,"text_elements":[]}]);
                let client_id = crate::util::uuid_v4()?;
                if status == Some("active") {
                    // Read only one turn summary, never hydrate the conversation.
                    let turns = connection.rpc(
                        "thread/turns/list",
                        json!({"threadId":self.conversation,
                        "limit":1,"sortDirection":"desc","itemsView":"summary"}),
                    )?;
                    if unsupported(&turns) {
                        return Ok(json!({"supported":false}));
                    }
                    let turns = result(&turns)?;
                    let turn = &turns["data"][0];
                    ensure!(
                        turn["status"] == "inProgress",
                        "the active turn changed; retry the question after refreshing"
                    );
                    let turn_id = turn["id"].as_str().context("missing active turn identity")?;
                    let reply = connection.rpc(
                        "turn/steer",
                        json!({"threadId":self.conversation,
                        "expectedTurnId":turn_id,"clientUserMessageId":client_id,"input":input}),
                    )?;
                    if unsupported(&reply) {
                        return Ok(json!({"supported":false}));
                    }
                    ensure!(
                        result(&reply)?["turnId"].as_str() == Some(turn_id),
                        "native question receipt has the wrong turn"
                    );
                } else {
                    let reply = connection.rpc(
                        "thread/queue/add",
                        json!({"threadId":self.conversation,
                        "clientUserMessageId":client_id,"input":input}),
                    )?;
                    if unsupported(&reply) {
                        return Ok(json!({"supported":false}));
                    }
                    ensure!(
                        result(&reply)?["queuedSubmission"]["clientUserMessageId"] == client_id,
                        "native question has no matching admission receipt"
                    );
                    // From this point the answer may have been delivered. Even an
                    // unsupported activation is an error, never terminal fallback.
                    let start = connection.rpc("thread/queue/start", json!({"threadId":self.conversation}))?;
                    if start.pointer("/result/turn/status").and_then(Value::as_str) != Some("inProgress") {
                        // Automatic idle dispatch may already have started it.
                        let read = connection
                            .rpc("thread/read", json!({"threadId":self.conversation,"includeTurns":false}))?;
                        ensure!(
                            result(&read)?["thread"]["id"] == self.conversation
                                && result(&read)?["thread"]["status"]["type"] == "active",
                            "answer queued, but its start is unconfirmed; check the conversation before retrying"
                        );
                    }
                }
                Ok(json!({"supported":true,"accepted":true}))
            }
        }
    }
}
