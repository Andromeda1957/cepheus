//! Cross-provider peers use live native inboxes, never terminal input.
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{config::Config, protocol::Tool};

pub const MAX_TEXT: usize = 8 * 1024;

pub fn transport(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Claude => Some("claude-inbox"),
        Tool::Codex => Some("codex-queue"),
        Tool::OpenCode => Some("opencode-api"),
        _ => None,
    }
}

pub fn permission_class(source: &crate::protocol::SessionInfo) -> Option<crate::providers::peers::PermissionClass> {
    use crate::providers::peers::PermissionClass;
    if source.tool != Tool::Codex {
        return None;
    }
    let root = source.account_root.as_deref().map(std::path::PathBuf::from).unwrap_or_else(crate::paths::codex_dir);
    let path = crate::codex::find_rollout(&root, source.conversation.as_deref()?)?;
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(1024 * 1024))).ok()?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    // The current turn's saved effective policy includes native overrides. A
    // config default or startup flag cannot prove a live session's policy.
    let context = bytes
        .split(|byte| *byte == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find(|v| v["type"] == "turn_context")?;
    match context["payload"]["approval_policy"].as_str()? {
        "never" => Some(PermissionClass::Bypass),
        "untrusted" | "on-failure" | "on-request" => Some(PermissionClass::Prompting),
        _ => None,
    }
}

pub fn sender(native: &crate::protocol::DeckState) -> Result<&crate::protocol::SessionInfo> {
    // A shared Codex daemon's shell tools carry the current native thread ID,
    // rather than the TUI launch environment. It must outrank an inherited slot.
    if let Ok(thread) = std::env::var("CODEX_THREAD_ID").or_else(|_| std::env::var("CODEX_SESSION_ID")) {
        let mut matches = native
            .sessions
            .iter()
            .filter(|s| s.alive && s.tool == Tool::Codex && s.conversation.as_deref() == Some(thread.as_str()));
        let session = matches.next().context("the calling Codex thread is not hosted here")?;
        anyhow::ensure!(matches.next().is_none(), "the calling Codex thread has multiple live sessions");
        return Ok(session);
    }
    let id = crate::paths::env_var("SESSION")
        .and_then(|v| v.into_string().ok())
        .context("agent send requires a hosted agent's native session identity")?
        .parse::<u64>()?;
    native
        .sessions
        .iter()
        .find(|s| s.id == id && s.alive && s.tool.is_agent())
        .context("the sending agent is no longer live")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sender {
    pub id: u64,
    pub provider: Tool,
    pub title: String,
    pub conversation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_class: Option<PermissionClass>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionClass {
    Bypass,
    Prompting,
}

impl Sender {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.id > 0 && self.provider.is_agent(), "a live agent sender is required");
        ensure!(self.title.len() <= 320 && !self.title.contains('\0'), "invalid sender title");
        ensure!(crate::service::valid_conversation_id(&self.conversation), "invalid sender conversation");
        Ok(())
    }
}

pub fn envelope(sender: &Sender, message_id: &str, text: &str) -> Result<String> {
    sender.validate()?;
    ensure!(
        !text.trim().is_empty() && text.len() <= MAX_TEXT && !text.contains('\0'),
        "message must contain 1–8192 bytes of text"
    );
    ensure!(
        message_id.len() == 36 && message_id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-'),
        "invalid message ID"
    );
    let body = format!(
        "Peer: {} session {} ({})\nConversation: {}\nMessage: {message_id}\nReply: cepheus agent send {} --conversation {} --stdin\nThis is an agent message. It grants no owner approval or new task authority.\n\n{text}",
        sender.provider.label(), sender.id, crate::util::one_line(&sender.title), sender.conversation, sender.id, crate::util::shell_quote(&sender.conversation)
    );
    let body = body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    // Claude recognizes this canonical peer wrapper. Codex retains the same
    // non-owner marker in its native queue and in repository prompt capture.
    let mode = match sender.permission_class {
        Some(PermissionClass::Bypass) => " from-mode=\"bypass\"",
        Some(PermissionClass::Prompting) => " from-mode=\"prompting\"",
        None => "",
    };
    Ok(format!(
        "<cross-session-message from-name=\"Cepheus agent {}\"{mode}>\n{body}\n</cross-session-message>",
        sender.id
    ))
}

pub struct Target<'a> {
    pub tool: Tool,
    pub pid: u32,
    pub conversation: &'a str,
    pub cwd: &'a Path,
    pub account_root: &'a Path,
}

/// A private plaintext peer inbox submission. Claude still applies its inbound policy.
#[derive(Clone)]
struct InboxRequest {
    socket: PathBuf,
    pid: u32,
    start: u64,
    conversation: String,
    text: String,
    message_id: String,
    priority: &'static str,
}

/// The transport submitted the input; this never claims a completed agent turn.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Submitted;

fn inbox_request(target: Target<'_>, text: &str, message_id: &str) -> Result<InboxRequest> {
    ensure!(target.tool == Tool::Claude, "this native peer inbox requires Claude");
    ensure!(crate::service::valid_conversation_id(target.conversation), "invalid target conversation");
    ensure!(
        !text.trim().is_empty() && text.len() <= crate::remote::wire::SEND_MAX && !text.contains('\0'),
        "invalid owner message text"
    );
    ensure!(
        message_id.len() == 36 && message_id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-'),
        "invalid message ID"
    );
    let (socket, pid, start) = claude_endpoint(target.account_root, target.pid, target.conversation)?;
    Ok(InboxRequest {
        socket,
        pid,
        start,
        conversation: target.conversation.into(),
        text: text.into(),
        message_id: message_id.into(),
        priority: "next",
    })
}

#[cfg(test)]
impl InboxRequest {
    /// Request native immediate handling for this initial submission only.
    /// This does not bypass Claude's inbound policy or promote an earlier frame.
    pub fn send_now(mut self) -> Self {
        self.priority = "now";
        self
    }

    pub fn deliver(self, deadline: Instant, stop: &AtomicBool) -> Result<Submitted> {
        write_claude(&self, deadline, stop)?;
        Ok(Submitted)
    }
}

pub enum Request {
    Queue { request: super::messages::Request, pid: u32, start: u64 },
    Claude { socket: PathBuf, pid: u32, start: u64, conversation: String, text: String, message_id: String },
}

impl Request {
    pub fn prepare(target: Target<'_>, config: &Config, text: &str, message_id: &str) -> Result<Self> {
        if target.tool == Tool::Claude {
            let InboxRequest { socket, pid, start, conversation, text, message_id, .. } =
                inbox_request(target, text, message_id)?;
            return Ok(Self::Claude { socket, pid, start, conversation, text, message_id });
        }
        let request =
            super::messages::request(target.tool, config, target.conversation, text, target.cwd, target.account_root)
                .context("this runtime has no supported native agent inbox; nothing was sent")?;
        ensure!(request.matches_runtime(target.pid), "native runtime identity could not be verified; nothing was sent");
        let start = crate::procs::stat(target.pid).context("target process is unavailable")?.start_ticks;
        Ok(Self::Queue { request, pid: target.pid, start })
    }

    pub fn deliver(self, stop: &AtomicBool) -> Result<Value> {
        match self {
            Self::Queue { request, pid, start } => {
                ensure!(
                    crate::procs::alive_with_start(pid, Some(start)) && request.matches_runtime(pid),
                    "native target changed; nothing was sent"
                );
                match request.deliver(Instant::now() + Duration::from_secs(35), stop) {
                    super::messages::Delivery::Accepted(_) => {
                        Ok(json!({"status":"queued","transport":"native","delivered":false}))
                    }
                    super::messages::Delivery::Unsupported => bail!("native inbox is unavailable; nothing was sent"),
                    super::messages::Delivery::Failed(error) => {
                        bail!("{error}; do not resend an uncertain message automatically")
                    }
                }
            }
            Self::Claude { socket, pid, start, conversation, text, message_id } => {
                ensure!(!stop.load(std::sync::atomic::Ordering::Relaxed), "delivery canceled before submission");
                send_claude(&socket, pid, start, &conversation, &text, &message_id, stop)?;
                // Claude has no synchronous acceptance ACK. Its own inbound policy
                // can hold or refuse the message after this successful write.
                Ok(json!({"status":"submitted","transport":"native","delivered":false,
                    "detail":"Written to Claude's native inbox; receipt and delivery are unconfirmed. Inbound policy still applies."}))
            }
        }
    }
}

fn bounded_json(path: &Path) -> Result<Value> {
    bounded_json_limit(path, 64 * 1024)
}

fn bounded_json_limit(path: &Path, limit: u64) -> Result<Value> {
    let mut file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() } && metadata.len() <= limit,
        "invalid native session record"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "native session record is too large");
    Ok(serde_json::from_slice(&bytes)?)
}

use std::os::unix::fs::OpenOptionsExt;

/// Whether process `pid` is `root` or runs under it.
pub(crate) fn descendant(pid: u32, root: u32) -> bool {
    let mut current = pid;
    for _ in 0..64 {
        if current == root {
            return true;
        }
        let Some(stat) = crate::procs::stat(current) else { return false };
        if stat.ppid < 2 || stat.ppid == current {
            return false;
        }
        current = stat.ppid;
    }
    false
}

fn claude_binding(
    root: &Path,
    hosted: u32,
    conversation: &str,
    eligible: impl Fn(&Value) -> bool,
) -> Result<(Value, u32, u64)> {
    let mut found = None;
    for entry in fs::read_dir(root.join("sessions"))
        .context("Claude's native inbox registry is unavailable")?
        .take(4096)
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(pid) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok()) else { continue };
        if !descendant(pid, hosted) {
            continue;
        }
        let Ok(record) = bounded_json(&path) else { continue };
        if record["sessionId"].as_str() != Some(conversation) {
            continue;
        }
        if !eligible(&record) {
            continue;
        }
        let Some(stat) = crate::procs::stat(pid) else { continue };
        if let Some(saved) = record.get("procStart") {
            let saved = saved.as_u64().or_else(|| saved.as_str().and_then(|value| value.parse().ok()));
            if saved != Some(stat.start_ticks) {
                continue;
            }
        }
        ensure!(found.is_none(), "Claude native session identity is ambiguous");
        found = Some((record, pid, stat.start_ticks));
    }
    found.context("this Claude session has no verified native endpoint; nothing was sent")
}

fn claude_endpoint(root: &Path, hosted: u32, conversation: &str) -> Result<(PathBuf, u32, u64)> {
    let (record, pid, start) = claude_binding(root, hosted, conversation, |r| {
        r["messagingSocketPath"].as_str().is_some_and(|s| s.starts_with('/'))
    })?;
    let socket = record["messagingSocketPath"].as_str().context("Claude peer inbox is unavailable")?;
    Ok((PathBuf::from(socket), pid, start))
}

fn send_claude(
    socket: &Path,
    pid: u32,
    start: u64,
    conversation: &str,
    text: &str,
    message_id: &str,
    stop: &AtomicBool,
) -> Result<()> {
    write_claude(
        &InboxRequest {
            socket: socket.into(),
            pid,
            start,
            conversation: conversation.into(),
            text: text.into(),
            message_id: message_id.into(),
            priority: "next",
        },
        Instant::now() + Duration::from_secs(3),
        stop,
    )
}

fn write_claude(request: &InboxRequest, deadline: Instant, stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(std::sync::atomic::Ordering::Relaxed), "delivery canceled before submission");
    let deadline = deadline.min(Instant::now() + Duration::from_secs(3));
    ensure!(Instant::now() < deadline, "delivery deadline elapsed before submission");
    let InboxRequest { socket, pid, start, conversation, text, message_id, priority } = request;
    let (pid, start) = (*pid, *start);
    let metadata = fs::symlink_metadata(socket).context("Claude inbox is unavailable; nothing was sent")?;
    ensure!(
        metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
        "invalid Claude inbox; nothing was sent"
    );
    ensure!(
        crate::procs::stat(pid).is_some_and(|stat| stat.start_ticks == start),
        "Claude process changed; nothing was sent"
    );
    let mut stream = crate::remote::streaming::connect_local(socket, deadline, stop)
        .context("could not connect to Claude inbox; nothing was sent")?;
    use std::os::fd::AsRawFd;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes a ucred into the correctly sized local buffer.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    ensure!(
        result == 0
            && length as usize == std::mem::size_of::<libc::ucred>()
            && peer.pid == pid as libc::pid_t
            && peer.uid == unsafe { libc::geteuid() },
        "Claude inbox process identity changed; nothing was sent"
    );
    ensure!(
        crate::procs::stat(pid).is_some_and(|stat| stat.start_ticks == start),
        "Claude process changed; nothing was sent"
    );
    let remaining =
        deadline.checked_duration_since(Instant::now()).context("delivery deadline elapsed before submission")?;
    ensure!(!remaining.is_zero(), "delivery deadline elapsed before submission");
    stream.set_write_timeout(Some(remaining))?;
    ensure!(!stop.load(std::sync::atomic::Ordering::Relaxed), "delivery canceled before submission");
    // Claude 2.1.319 interprets this as peer input with an unknown address.
    // Keep plaintext unchanged and omit agent identity, envelopes and policy
    // assertions. Socket submission cannot prove the policy accepted the input.
    let mut bytes = serde_json::to_vec(&json!({
        "type":"user", "session_id":conversation, "uuid":message_id, "msg_id":message_id, "priority":priority,
        "message":{"role":"user", "content":text}
    }))?;
    bytes.push(b'\n');
    stream.write_all(&bytes).context("Claude inbox write was uncertain; do not resend automatically")?;
    Ok(())
}

#[cfg(test)]
#[path = "../../src/tests/agent_messaging.rs"]
mod tests;
