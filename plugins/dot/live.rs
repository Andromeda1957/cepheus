//! Native room activity: the app's own room subscription over its WebSocket,
//! bootstrapped by the room's live event stream. Room, subscription and an
//! increasing revision must match; only attached turn state says Dot is
//! working or done, and message arrival never implies completion. The token
//! stays in this process; the signed WebSocket URL is never logged.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::Message;

use super::net::{self, Body, Stream, Target};
use super::state::quote;

const RESPONSE_MAX: u64 = 1 << 20;
const FRAME_MAX: usize = 8 << 20;
const EVENT_MAX: usize = 1 << 20;
#[cfg(test)]
#[path = "../../src/tests/dot_live.rs"]
mod tests;
pub const UNAVAILABLE: &str = "Live activity unavailable";

/// Hears that the activity changed; the listener asks for the new state.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// Where and how fast the consumer works.
#[derive(Clone)]
pub struct Settings {
    pub api: String,
    /// Connecting until the first known state, and the wait for the user feed.
    pub connect: Duration,
    /// The pause after a failure before starting again.
    pub retry: Duration,
    /// Tests only: plain local WebSocket and stream servers.
    pub loopback: bool,
}

impl Settings {
    pub fn production(api: &str) -> Settings {
        Settings { api: api.into(), connect: Duration::from_secs(25), retry: Duration::from_secs(5), loopback: false }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub known: bool,
    pub working: bool,
    pub label: String,
    pub turn_id: Value,
    pub revision: Value,
}

impl Status {
    pub fn unknown() -> Status {
        Status { known: false, working: false, label: UNAVAILABLE.into(), turn_id: Value::Null, revision: Value::Null }
    }
}

#[derive(Clone)]
struct Config {
    token: String,
    account_id: String,
    room_id: String,
    app_version: String,
}

struct Shared {
    config: Mutex<Config>,
    status: Mutex<Status>,
    /// Bumped by a new token or a stop; a cycle of another generation ends.
    generation: AtomicU64,
    stopped: AtomicBool,
    wake: Condvar,
    notify: Option<Notify>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The consumer, running on its own thread until closed.
pub struct Activity {
    shared: Arc<Shared>,
}

impl Activity {
    pub fn start(
        settings: Settings,
        token: &str,
        account_id: &str,
        room_id: &str,
        app_version: &str,
        notify: Option<Notify>,
    ) -> Activity {
        let shared = Arc::new(Shared {
            config: Mutex::new(Config {
                token: token.into(),
                account_id: account_id.into(),
                room_id: room_id.into(),
                app_version: app_version.into(),
            }),
            status: Mutex::new(Status::unknown()),
            generation: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            wake: Condvar::new(),
            notify,
        });
        let worker = Arc::clone(&shared);
        let _ = std::thread::Builder::new().name("dot-live".into()).spawn(move || consume(&worker, &settings));
        Activity { shared }
    }

    /// Use a renewed token: the subscription starts again with it.
    pub fn update(&self, token: &str) {
        let mut config = lock(&self.shared.config);
        if config.token != token {
            config.token = token.into();
            self.shared.generation.fetch_add(1, Ordering::SeqCst);
            self.shared.wake.notify_all();
        }
    }

    pub fn status(&self) -> Status {
        lock(&self.shared.status).clone()
    }

    pub fn close(&self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        self.shared.wake.notify_all();
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        self.close();
    }
}

/// Keep the new state. A change the chat shows (known, working, label or
/// turn) is announced while the room is open; a revision alone is not.
fn publish(shared: &Shared, status: Status) {
    let changed = {
        let mut current = lock(&shared.status);
        let changed = (current.known, current.working, &current.label, &current.turn_id)
            != (status.known, status.working, &status.label, &status.turn_id);
        *current = status;
        changed
    };
    if changed && !shared.stopped.load(Ordering::SeqCst) {
        if let Some(notify) = &shared.notify {
            notify();
        }
    }
}

fn consume(shared: &Arc<Shared>, settings: &Settings) {
    while !shared.stopped.load(Ordering::SeqCst) {
        let generation = shared.generation.load(Ordering::SeqCst);
        let config = lock(&shared.config).clone();
        publish(shared, Status::unknown());
        let alive = Arc::new(AtomicBool::new(true));
        let wanted = {
            let shared = Arc::clone(shared);
            let alive = Arc::clone(&alive);
            move || {
                alive.load(Ordering::SeqCst)
                    && !shared.stopped.load(Ordering::SeqCst)
                    && shared.generation.load(Ordering::SeqCst) == generation
            }
        };
        let wanted: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(wanted);
        let failed = Cycle::new(shared, settings, &config, Arc::clone(&wanted)).run();
        alive.store(false, Ordering::SeqCst);
        if !failed || shared.generation.load(Ordering::SeqCst) != generation {
            continue;
        }
        publish(shared, Status::unknown());
        let deadline = Instant::now() + settings.retry;
        let mut config = lock(&shared.config);
        while !shared.stopped.load(Ordering::SeqCst) && shared.generation.load(Ordering::SeqCst) == generation {
            let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else { break };
            config = shared.wake.wait_timeout(config, left).unwrap_or_else(|p| p.into_inner()).0;
        }
    }
    publish(shared, Status::unknown());
}

/// What the room's attached turn says, or `None` when it says nothing usable.
pub fn working_state(state: &Value) -> Option<(bool, &'static str)> {
    let thread = state.get("threads")?.get(0)?;
    if thread.get("stream_state")? != "attached" {
        return None;
    }
    let turn = thread.get("turn_status").unwrap_or(&Value::Null);
    let turn = match turn {
        Value::Null => None,
        Value::String(s) if ["in_progress", "completed", "failed", "interrupted"].contains(&s.as_str()) => {
            Some(s.as_str())
        }
        _ => return None,
    };
    let kind = match thread.get("activity_status").and_then(|a| a.get("status_type")) {
        Some(Value::String(s)) => s.to_lowercase().replace('_', " "),
        _ => String::new(),
    };
    if turn == Some("failed") {
        return Some((false, "Turn failed"));
    }
    if kind == "ready" || (turn != Some("in_progress") && thread.get("is_active") != Some(&Value::Bool(true))) {
        return Some((false, "Ready"));
    }
    let label = match kind.as_str() {
        "working" => "Working",
        "searching" => "Searching",
        "creating" => "Creating",
        "payment" | "needs input" => "Needs input",
        "paused" => "Paused",
        _ => "Thinking",
    };
    Some((true, label))
}

fn number_is(value: Option<&Value>, expected: f64) -> bool {
    value.and_then(Value::as_f64) == Some(expected)
}

/// A JavaScript safe integer.
fn safe_integer(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(n) = value.as_i64() {
        return (n.unsigned_abs() < (1u64 << 53)).then_some(n);
    }
    let f = value.as_f64()?;
    (f.fract() == 0.0 && f.abs() <= ((1u64 << 53) - 1) as f64).then_some(f as i64)
}

/// The WebSocket URL must be the app's own service, over TLS on its usual port.
pub fn socket_allowed(url: &str, loopback: bool) -> bool {
    let Ok(uri) = url.parse::<tungstenite::http::Uri>() else { return false };
    let Some(authority) = uri.authority() else { return false };
    if authority.as_str().contains('@') {
        return false;
    }
    let host = uri.host().unwrap_or("");
    if loopback && uri.scheme_str() == Some("ws") && host == "127.0.0.1" {
        return true;
    }
    uri.scheme_str() == Some("wss")
        && uri.port_u16().is_none_or(|port| port == 443)
        && (host == "chatgpt.com"
            || host.ends_with(".chatgpt.com")
            || host == "openai.com"
            || host.ends_with(".openai.com"))
}

enum Event {
    Ready(String),
    Failed,
}

struct Cycle<'a> {
    shared: &'a Arc<Shared>,
    settings: &'a Settings,
    config: &'a Config,
    wanted: Arc<dyn Fn() -> bool + Send + Sync>,
    ready_id: Option<String>,
    revision: i64,
    pending: HashMap<String, Value>,
    /// Until the first known state; then cleared.
    connect_by: Option<Instant>,
    live: Option<Receiver<Event>>,
}

impl<'a> Cycle<'a> {
    fn new(
        shared: &'a Arc<Shared>,
        settings: &'a Settings,
        config: &'a Config,
        wanted: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Cycle {
            shared,
            settings,
            config,
            wanted,
            ready_id: None,
            revision: -1,
            pending: HashMap::new(),
            connect_by: None,
            live: None,
        }
    }

    fn topic(&self) -> String {
        format!("calpico-room-live:{}", self.config.room_id)
    }

    /// Run until the subscription fails (true) or is no longer wanted (false).
    fn run(&mut self) -> bool {
        let Some(url) = self.socket_url() else { return (self.wanted)() };
        if !(self.wanted)() {
            return false;
        }
        let deadline = Instant::now() + self.settings.connect;
        self.connect_by = Some(deadline);
        let Some(mut socket) = self.open_socket(&url, deadline) else { return (self.wanted)() };
        let frame = json!([
            {"id": 1, "command": {"type": "connect", "presence": {"type": "presence", "state": "background"}}},
            {"id": 2, "command": {"type": "subscribe", "topic_id": self.topic()}},
        ]);
        let mut failed = socket.send(Message::text(frame.to_string())).is_err();
        while !failed {
            if !(self.wanted)() {
                break;
            }
            if self.connect_by.is_some_and(|by| Instant::now() >= by) {
                failed = true;
                break;
            }
            while let Some(event) = self.live.as_ref().and_then(|rx| rx.try_recv().ok()) {
                match event {
                    Event::Ready(id) => {
                        self.ready_id = Some(id.clone());
                        self.revision = -1;
                        if let Some(state) = self.pending.remove(&id) {
                            self.apply(&state);
                        }
                        self.pending.clear();
                    }
                    Event::Failed => failed = true,
                }
            }
            if failed {
                break;
            }
            match socket.read() {
                Ok(Message::Text(text)) => failed = self.frame(text.as_str()).is_none(),
                Ok(Message::Binary(_) | Message::Close(_)) => failed = true,
                Ok(_) => {}
                Err(tungstenite::Error::Io(err)) if net::is_timeout(&err) => {}
                Err(_) => failed = true,
            }
        }
        // Leave the topic and close, as the app does when it stops listening.
        if socket.can_write() {
            let leave = json!([{"id": 3, "command": {"type": "unsubscribe", "topic_id": self.topic()}}]);
            let _ = socket.send(Message::text(leave.to_string()));
            let _ = socket.close(None);
            let _ = socket.flush();
        }
        socket.get_ref().shutdown();
        failed && (self.wanted)()
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Authorization", format!("Bearer {}", self.config.token)),
            ("ChatGPT-Account-Id", self.config.account_id.clone()),
            ("originator", "Codex Desktop".into()),
            ("User-Agent", format!("Codex Desktop/{} (X11; Linux; x64)", self.config.app_version)),
            ("x-openai-web-frontend", "codex_webview".into()),
            ("x-openai-codex-window-type", "electron".into()),
            ("OAI-Language", "en".into()),
        ]
    }

    /// The signed user feed URL. Never logged: it is a credential.
    fn socket_url(&self) -> Option<String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(self.settings.connect))
            .build()
            .new_agent();
        let mut request = agent.get(format!("{}/celsius/ws/user", self.settings.api));
        for (name, value) in self.headers() {
            request = request.header(name, value);
        }
        let response = request.call().ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.into_body().with_config().limit(RESPONSE_MAX).read_to_vec().ok()?;
        let value: Value = serde_json::from_slice(&body).ok()?;
        let url = value.get("websocket_url")?.as_str()?.to_string();
        socket_allowed(&url, self.settings.loopback).then_some(url)
    }

    fn open_socket(&self, url: &str, deadline: Instant) -> Option<tungstenite::WebSocket<Stream>> {
        let target = Target::parse(url)?;
        let stream = net::connect(&target.host, target.port, target.tls, deadline).ok()?;
        let limits = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(FRAME_MAX))
            .max_frame_size(Some(FRAME_MAX));
        let (socket, _) = tungstenite::client::client_with_config(url, stream, Some(limits)).ok()?;
        socket.get_ref().set_read_timeout(net::POLL).ok()?;
        Some(socket)
    }

    /// One WebSocket text frame; `None` when it is not a frame of this protocol.
    fn frame(&mut self, text: &str) -> Option<()> {
        if text.len() > FRAME_MAX {
            return None;
        }
        let events: Value = serde_json::from_str(text).ok()?;
        let events = events.as_array().filter(|events| events.len() <= 1000)?;
        let topic = self.topic();
        for item in events {
            let reply = item.get("reply");
            if number_is(item.get("id"), 2.0)
                && reply.and_then(|r| r.get("type")).is_some_and(|t| t == "subscribe")
                && reply.and_then(|r| r.get("topic_id")).and_then(Value::as_str) == Some(topic.as_str())
            {
                self.start_live();
            }
            let payload = item.get("payload");
            if item.get("type").is_some_and(|t| t == "message")
                && item.get("topic_id").and_then(Value::as_str) == Some(topic.as_str())
                && payload.and_then(|p| p.get("type")).is_some_and(|t| t == "calpico-room-live")
            {
                if let Some(state) = payload.and_then(|p| p.get("payload")) {
                    self.apply(state);
                }
            }
        }
        Some(())
    }

    fn apply(&mut self, state: &Value) {
        if !(self.wanted)()
            || state.get("type").and_then(Value::as_str) != Some("calpico-room-live-state")
            || !number_is(state.get("version"), 1.0)
            || state.get("room_id").and_then(Value::as_str) != Some(self.config.room_id.as_str())
        {
            return;
        }
        let Some(subscription) = state.get("subscription_id").and_then(Value::as_str) else { return };
        let Some(revision) = safe_integer(state.get("revision")) else { return };
        if state.get("threads").and_then(Value::as_array).is_none_or(|threads| threads.len() > 100) {
            return;
        }
        if self.ready_id.as_deref() != Some(subscription) {
            if self.pending.len() >= 4 {
                self.pending.clear();
            }
            self.pending.insert(subscription.to_string(), state.clone());
            return;
        }
        if revision <= self.revision {
            return;
        }
        self.revision = revision;
        match working_state(state) {
            Some((working, label)) => {
                self.connect_by = None;
                let turn_id = state["threads"][0].get("turn_id").cloned().unwrap_or(Value::Null);
                publish(
                    self.shared,
                    Status { known: true, working, label: label.into(), turn_id, revision: json!(revision) },
                );
            }
            None => publish(self.shared, Status::unknown()),
        }
    }

    /// Start the room's event stream once, on its own thread.
    fn start_live(&mut self) {
        if self.live.is_some() || !(self.wanted)() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.live = Some(rx);
        let url = format!("{}/messaging/rooms/{}/live", self.settings.api, quote(&self.config.room_id, "-_.!~*'()"));
        let headers = self.headers();
        let room = self.config.room_id.clone();
        let wanted = Arc::clone(&self.wanted);
        let _ = std::thread::Builder::new().name("dot-live-stream".into()).spawn(move || {
            let failed = stream(&url, &headers, &room, &tx, wanted.as_ref()).is_err();
            if failed && wanted() {
                let _ = tx.send(Event::Failed);
            }
        });
    }
}

/// Read the room's event stream, reporting each ready event, until it ends,
/// fails or is no longer wanted.
fn stream(
    url: &str,
    headers: &[(&'static str, String)],
    room: &str,
    tx: &Sender<Event>,
    wanted: &(dyn Fn() -> bool + Send + Sync),
) -> Result<(), ()> {
    let target = Target::parse(url).ok_or(())?;
    // The request itself is bounded like the app's; the stream then lasts.
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut socket = net::connect(&target.host, target.port, target.tls, deadline).map_err(|_| ())?;
    let head = net::request(&mut socket, "POST", &target, headers, deadline, wanted).map_err(|_| ())?;
    let content_type = head.header("content-type").unwrap_or("");
    if !(200..300).contains(&head.status) || !content_type.contains("text/event-stream") {
        socket.shutdown();
        return Err(());
    }
    let mut body = Body::for_head(&head);
    let mut decoder = super::terminal::Decoder::default();
    let mut text = String::new();
    let mut data: Vec<String> = Vec::new();
    let mut raw = head.rest.clone();
    let mut buf = [0u8; 16 * 1024];
    let result = loop {
        let mut bytes = Vec::new();
        if body.feed(&raw, &mut bytes).is_err() {
            break Err(());
        }
        text.push_str(&decoder.decode(&bytes));
        if text.len() > EVENT_MAX {
            break Err(());
        }
        while let Some(at) = text.find('\n') {
            let line: String = text.drain(..=at).collect();
            let line = line.trim_end_matches('\n');
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(value) = line.strip_prefix("data:") {
                data.push(value.trim_start().to_string());
            }
            if data.iter().map(|d| d.len() + 1).sum::<usize>() > EVENT_MAX {
                return Err(());
            }
            if line.is_empty() && !data.is_empty() {
                let event: Value = serde_json::from_str(&data.join("\n")).map_err(|_| ())?;
                data.clear();
                if event.get("type").and_then(Value::as_str) == Some("calpico-room-live-ready")
                    && number_is(event.get("version"), 1.0)
                    && event.get("room_id").and_then(Value::as_str) == Some(room)
                {
                    if let Some(id) = event.get("subscription_id").and_then(Value::as_str) {
                        let _ = tx.send(Event::Ready(id.to_string()));
                    }
                }
            }
        }
        if body.done() || !wanted() {
            break Err(());
        }
        raw.clear();
        match std::io::Read::read(&mut socket, &mut buf) {
            Ok(0) => break Err(()),
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(err) if net::is_timeout(&err) => {}
            Err(_) => break Err(()),
        }
    };
    socket.shutdown();
    result
}
