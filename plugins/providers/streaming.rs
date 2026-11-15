//! A passive view of text being written in an existing Codex daemon thread.
//! Rollouts contain completed items only. This connection never starts a turn,
//! changes settings, or loads a thread in another agent runtime.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::{ffi::OsStrExt, net::UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

use super::chat::{clip, Item, TEXT_MAX};
use crate::protocol::{SessionInfo, Tool};

const WAIT: Duration = Duration::from_millis(200);
const DEADLINE: Duration = Duration::from_secs(8);
const MESSAGES_MAX: usize = 128;

#[derive(Clone, Debug, PartialEq)]
struct Runtime {
    pid: u32,
    socket: PathBuf,
    thread: String,
    cwd: Option<PathBuf>,
}

impl Runtime {
    fn of(info: &SessionInfo) -> Option<Self> {
        if info.tool != Tool::Codex || !info.alive || info.host.is_some() {
            return None;
        }
        let pid = info.pid?;
        let thread = info.conversation.as_ref()?;
        if !crate::service::valid_conversation_id(thread) {
            return None;
        }
        let args = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let env = fs::read(format!("/proc/{pid}/environ")).ok()?;
        let runtime = Self::parse(pid, thread, &args, &env)?;
        if runtime
            .cwd
            .as_ref()
            .is_some_and(|cwd| info.cwd.as_ref().and_then(|p| Path::new(p).canonicalize().ok()).as_ref() != Some(cwd))
        {
            return None;
        }
        Some(runtime)
    }

    fn parse(pid: u32, thread: &str, args: &[u8], env: &[u8]) -> Option<Self> {
        if args.len() > 65_536 || env.len() > 1_048_576 {
            return None;
        }
        let mut args = args.split(|b| *b == 0).filter(|s| !s.is_empty());
        if Path::new(std::ffi::OsStr::from_bytes(args.next()?)).file_name()? != "codex" {
            return None;
        }
        let mut socket = None;
        let mut cwd = None;
        while let Some(arg) = args.next() {
            match arg {
                // Only local Unix transport is used. An embedded client or an
                // unknown wrapper/config override must keep transcript behavior.
                b"--remote" => {
                    let endpoint = std::str::from_utf8(args.next()?).ok()?;
                    let path = endpoint.strip_prefix("unix://")?;
                    if !path.is_empty() {
                        let path = PathBuf::from(path);
                        if !path.is_absolute() {
                            return None;
                        }
                        socket = Some(path);
                    }
                }
                b"--profile" | b"-p" | b"--model" | b"-m" => {
                    args.next()?;
                }
                b"--cd" => {
                    let path = Path::new(std::ffi::OsStr::from_bytes(args.next()?));
                    if cwd.is_some() || !path.is_absolute() || !path.is_dir() {
                        return None;
                    }
                    cwd = Some(path.canonicalize().ok()?);
                }
                b"resume" => {
                    if args.next()? != thread.as_bytes() {
                        return None;
                    }
                }
                b"--yolo" | b"--dangerously-bypass-approvals-and-sandbox" | b"--search" | b"--no-alt-screen" => {}
                // The message the launch submitted, last
                // (`codex resume ID -- MESSAGE`).
                b"--" => {
                    args.next()?;
                    if args.next().is_some() {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        let value = |prefix: &[u8]| {
            env.split(|b| *b == 0)
                .find_map(|part| part.strip_prefix(prefix))
                .map(|p| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
        };
        let home = value(b"CODEX_HOME=").or_else(|| value(b"HOME=").map(|p| p.join(".codex")))?;
        let socket = socket.unwrap_or_else(|| home.join("app-server-control/app-server-control.sock"));
        Some(Self { pid, socket, thread: thread.into(), cwd })
    }

    fn alive(&self) -> bool {
        let args = fs::read(format!("/proc/{}/cmdline", self.pid)).ok();
        let env = fs::read(format!("/proc/{}/environ", self.pid)).ok();
        args.zip(env).and_then(|(a, e)| Self::parse(self.pid, &self.thread, &a, &e)).as_ref() == Some(self)
    }
}

#[derive(Clone, Debug)]
pub(super) struct Text {
    pub id: String,
    pub item: Item,
    synced: bool,
}

#[derive(Default)]
struct State {
    texts: VecDeque<Text>,
}

impl State {
    fn entry(&mut self, id: &str) -> Option<&mut Text> {
        if id.is_empty() || id.len() > 256 {
            return None;
        }
        if !self.texts.iter().any(|m| m.id == id) {
            if self.texts.len() == MESSAGES_MAX {
                self.texts.pop_front();
            }
            self.texts.push_back(Text {
                id: id.into(),
                item: Item::Assistant { text: String::new(), questions: Vec::new(), at: None },
                synced: false,
            });
        }
        self.texts.iter_mut().find(|m| m.id == id)
    }

    fn disconnected(&mut self) {
        // A reconnect may have missed deltas. Keep the displayed prefix, but
        // never append a suffix across a gap. Completion repairs it in full.
        for text in &mut self.texts {
            text.synced = false;
        }
    }

    fn event(&mut self, event: &Value, thread: &str) {
        let Some(params) = event.get("params") else { return };
        if params.get("threadId").and_then(Value::as_str) != Some(thread) {
            return;
        }
        match event.get("method").and_then(Value::as_str) {
            Some("turn/started") => self.texts.clear(),
            Some("item/started" | "item/completed") => {
                let Some(item) = params.get("item") else { return };
                if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
                    return;
                }
                let Some(text) = self.entry(item.get("id").and_then(Value::as_str).unwrap_or("")) else { return };
                if let Item::Assistant { text: current, .. } = &mut text.item {
                    *current = clip(item.get("text").and_then(Value::as_str).unwrap_or(""), TEXT_MAX);
                    text.synced = true;
                }
            }
            Some("item/agentMessage/delta") => {
                let Some(text) = self.entry(params.get("itemId").and_then(Value::as_str).unwrap_or("")) else { return };
                if text.synced {
                    if let Item::Assistant { text: current, .. } = &mut text.item {
                        // Keep the same character bound as durable chat items.
                        let remaining = TEXT_MAX.saturating_sub(current.chars().count());
                        current
                            .extend(params.get("delta").and_then(Value::as_str).unwrap_or("").chars().take(remaining));
                    }
                }
            }
            _ => {}
        }
    }

    fn latest(&self, completed: &VecDeque<String>) -> Option<String> {
        self.texts.iter().rev().filter(|text| !completed.contains(&text.id)).find_map(|text| match &text.item {
            Item::Assistant { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
    }
}

pub(super) struct Observer {
    runtime: Runtime,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Observer {
    pub fn matches(&self, info: &SessionInfo) -> bool {
        Runtime::of(info).as_ref() == Some(&self.runtime)
    }

    pub fn start(info: &SessionInfo) -> Option<Self> {
        let runtime = Runtime::of(info)?;
        Some(Self::spawn(runtime))
    }

    fn spawn(runtime: Runtime) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, s, done) = (runtime.clone(), state.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !done.load(Ordering::Relaxed) && r.alive() {
                let _ = observe(&r, &s, &done);
                s.lock().unwrap_or_else(|p| p.into_inner()).disconnected();
                // Bound retry work and make cache eviction stop it promptly.
                for _ in 0..10 {
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(WAIT);
                }
            }
        });
        Self { runtime, state, stop, worker: Some(worker) }
    }

    pub fn acknowledge(&self, ids: &VecDeque<String>) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).texts.retain(|text| !ids.contains(&text.id));
    }

    pub fn latest(&self, completed: &VecDeque<String>) -> Option<String> {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).latest(completed)
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

type Socket = WebSocket<UnixStream>;

fn read(socket: &mut Socket, stop: &AtomicBool) -> Result<Option<Value>> {
    if stop.load(Ordering::Relaxed) {
        bail!("observer stopped");
    }
    match socket.read() {
        Ok(Message::Text(text)) => Ok(Some(serde_json::from_str(&text)?)),
        Ok(Message::Close(_)) => bail!("observer closed"),
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(e))
            if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

fn rpc(
    socket: &mut Socket,
    id: u64,
    method: &str,
    params: Value,
    state: &Mutex<State>,
    runtime: &Runtime,
    stop: &AtomicBool,
) -> Result<Value> {
    socket.send(Message::Text(json!({"id": id, "method": method, "params": params}).to_string().into()))?;
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if let Some(event) = read(socket, stop)? {
            if event.get("id").and_then(Value::as_u64) == Some(id) {
                if event.get("error").is_some() {
                    bail!("observer request refused");
                }
                return event.get("result").cloned().context("missing observer response");
            }
            state.lock().unwrap_or_else(|p| p.into_inner()).event(&event, &runtime.thread);
        }
    }
    bail!("observer request timed out")
}

/// A local daemon may be paused with its accept queue full. Nonblocking
/// connect keeps both the caller's deadline and observer cancellation effective
/// before any stream read/write timeout can be installed.
pub(crate) fn connect_local(path: &Path, deadline: Instant, stop: &AtomicBool) -> io::Result<UnixStream> {
    // SAFETY: an all-zero sockaddr_un is valid storage; its fields are filled
    // below and its pathname remains NUL-terminated.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    let budget = || {
        if stop.load(Ordering::Relaxed) {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .map(|left| left.min(Duration::from_millis(20)))
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    };
    loop {
        budget()?;
        // SAFETY: socket takes no pointers and returns a new descriptor we own.
        let fd =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd was just created and has exactly one owner.
        let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
        // SAFETY: address points to initialized sockaddr_un storage of length
        // `length`; stream owns a valid nonblocking Unix socket descriptor.
        let connected =
            unsafe { libc::connect(stream.as_raw_fd(), (&address as *const libc::sockaddr_un).cast(), length) } == 0;
        if connected {
            stream.set_nonblocking(false)?;
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINPROGRESS) {
            loop {
                let wait = budget()?;
                let mut ready = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                // SAFETY: poll receives one initialized pollfd and a bounded
                // timeout; no other thread owns or closes this descriptor.
                let result = unsafe { libc::poll(&mut ready, 1, wait.as_millis().max(1) as libc::c_int) };
                if result > 0 {
                    if let Some(error) = stream.take_error()? {
                        return Err(error);
                    }
                    stream.peer_addr()?;
                    stream.set_nonblocking(false)?;
                    return Ok(stream);
                }
                if result < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
        }
        if !matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) {
            return Err(error);
        }
        // Linux Unix sockets report EAGAIN when the accept queue is full; no
        // connection is pending. Drop this socket and retry with a short wait.
        drop(stream);
        thread::sleep(budget()?);
    }
}

fn observe(runtime: &Runtime, state: &Mutex<State>, stop: &AtomicBool) -> Result<()> {
    let stream = connect_local(&runtime.socket, Instant::now() + DEADLINE, stop)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(32 << 20))
        .max_frame_size(Some(32 << 20));
    let (mut socket, _) = tungstenite::client::client_with_config("ws://localhost/", stream, Some(config))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    socket.get_ref().set_read_timeout(Some(WAIT))?;
    rpc(
        &mut socket,
        1,
        "initialize",
        json!({"clientInfo": {"name": "cepheus", "version": "0.1"}, "capabilities": {"experimentalApi": true}}),
        state,
        runtime,
        stop,
    )?;
    socket.send(Message::Text(json!({"method": "initialized"}).to_string().into()))?;
    // The exact thread must already be loaded in the very same daemon. Reading
    // its status avoids paging through unrelated private thread histories.
    let current = rpc(&mut socket, 2, "thread/read", json!({"threadId": runtime.thread}), state, runtime, stop)?;
    if current.pointer("/thread/id").and_then(Value::as_str) != Some(&runtime.thread)
        || !matches!(
            current.pointer("/thread/status/type").and_then(Value::as_str),
            Some("active" | "idle" | "systemError")
        )
    {
        bail!("thread is not loaded in this runtime");
    }
    rpc(
        &mut socket,
        3,
        "thread/resume",
        json!({"threadId": runtime.thread, "excludeTurns": true}),
        state,
        runtime,
        stop,
    )?;
    let mut checked = Instant::now();
    loop {
        if checked.elapsed() >= Duration::from_secs(1) {
            if !runtime.alive() {
                bail!("hosted runtime changed");
            }
            checked = Instant::now();
        }
        if let Some(event) = read(&mut socket, stop)? {
            state.lock().unwrap_or_else(|p| p.into_inner()).event(&event, &runtime.thread);
        }
    }
}

#[cfg(test)]
#[path = "../../src/tests/remote_streaming.rs"]
mod tests;
