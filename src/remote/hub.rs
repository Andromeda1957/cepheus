//! What the gateway shares between its threads: the service connection, the
//! deck state, alerts, the phones connected and the requests they make.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::BufReader;
use std::net::IpAddr;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::config::{Config, HostConfig};
use crate::hosts::wire::{HostReq, HostResp};
use crate::prompt;
use crate::protocol::{
    read_msg, split_id, write_msg, ClientMsg, ClientRole, DeckState, HostInfo, QuestionStale, ServerMsg, SessionInfo,
    StatePatch, Status, Tool, PROTOCOL_VERSION,
};
use crate::providers::launch;
use crate::remote::auth::{self, Limiter};
use crate::remote::chat::{self, Item, Source};
use crate::remote::links::{self, Links};
use crate::remote::store::{self, Device};
use crate::remote::uploads::{self, Uploads};
use crate::remote::wire::{Piece, Req, SEND_MAX};
use crate::remote::{commands, models, search};
use crate::resources::Sampler;
use crate::{codexpick, handover, service, util};

#[path = "control.rs"]
pub(crate) mod control;
#[path = "target.rs"]
mod target;

/// How long a request to the service may take.
const SERVICE_REPLY: Duration = Duration::from_secs(5);
/// The resume list reads every program's logs, Codex's from 400 day folders.
const LIST_REPLY: Duration = Duration::from_secs(30);
/// Alerts kept to send a phone that was away.
const ALERTS_KEPT: usize = 100;

/// Whether a kept alert reports the listed error `text`, in its own words
/// or, for a host's error, under that host's label.
fn reports(alert: &Value, text: &str) -> bool {
    alert.get("kind").and_then(Value::as_str) == Some("error")
        && alert.get("message").is_none()
        && alert.get("reason").and_then(Value::as_str).is_some_and(|reason| {
            text == reason || text.strip_suffix(reason).is_some_and(|label| label.ends_with(": "))
        })
}

/// Whether a kept alert still describes its session, for a phone that connects after it: a finished turn or a
/// question only while the session is still at that turn or question. A phone that reconnects is never told
/// again of a turn the session has moved on from.
fn still_current(state: Option<&DeckState>, alert: &Value) -> bool {
    let status = match alert.get("kind").and_then(Value::as_str) {
        Some("finished") => Status::Finished,
        Some("needs_you") => Status::NeedsYou,
        _ => return true,
    };
    let Some(id) = alert.get("id").and_then(Value::as_u64) else { return false };
    let at = alert.get("at").and_then(Value::as_u64).unwrap_or(0) / 1000;
    state
        .and_then(|state| state.sessions.iter().find(|s| s.id == id))
        .is_some_and(|s| s.alive && s.status == status && s.status_since <= at)
}
/// Items in a chat page unless the phone asks for another number.
const PAGE: usize = 60;
const PAGE_MAX: usize = 200;

pub struct Options {
    pub socket: PathBuf,
    pub dir: PathBuf,
    pub listen: String,
    /// The phone app's download address (`apk.rs`); empty: off.
    pub apk_listen: String,
    pub allow: Vec<IpAddr>,
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
    pub opencode_db: PathBuf,
    /// Antigravity's state directory, whose conversations the phone reads.
    pub antigravity_dir: PathBuf,
    pub sqlite3: Vec<String>,
    pub order_file: PathBuf,
    /// Where files the phone attaches are kept ([`uploads`]).
    pub uploads_dir: PathBuf,
    /// How often each phone is pinged. A sleeping phone's own timers stop;
    /// a ping wakes its network stack, which answers it without the app.
    pub ping_every: Duration,
    /// Exit when this config turns `remote` off or changes the address
    /// (the service starts the gateway again); none in tests.
    pub config_file: Option<PathBuf>,
    /// Exit when this program's file is replaced, so the new build runs.
    pub watch_exe: bool,
    /// Exit after the service has been gone this long.
    pub service_grace: Duration,
    /// How the phone heads this machine's sessions when there are others.
    pub here: String,
    /// The hosts the hub drives, and the key it proves itself with: a
    /// remote session's chat, commands and uploads go to its host.
    pub hosts: Vec<HostConfig>,
    pub hub_key: Option<String>,
}

pub struct Conn {
    pub device: String,
    pub tx: mpsc::Sender<String>,
    pub open: Option<u64>,
    sent_total: usize,
    sent_generation: u64,
    sent_screen: Option<String>,
    sent_model: chat::ModelSeen,
    sent_stream: Option<String>,

    sent_questions: Vec<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Want {
    ErrorsCleared,
    Created,
    Reopened(u64),
    Resumed,
    Conversations,
    Sent,
    MessageSent(u64, u64),
    Plugin(u64),
    TargetMessage(u64, u64),
    Screen(u64),
    Answered(u64),
    SentNow(u64),
}

struct Waiter {
    id: u64,
    want: Want,
    source: ReplySource,
    tx: mpsc::Sender<ServerMsg>,
}

#[derive(Default)]
struct Inner {
    state: Option<DeckState>,
    /// Cached state is readable after a loss, but cannot authorize another action.
    service_ready: bool,
    service_generation: u64,
    state_revision: u64,
    service_pid: Option<u32>,
    start_dir: Option<PathBuf>,
    /// The protocol the service welcomed this gateway with: a newer gateway
    /// started beside an older service must not send it what it cannot read.
    service_version: u32,
    alerts: VecDeque<Value>,
    next_seq: u64,
    conns: HashMap<u64, Conn>,
    /// The prompt last sent per waiting session.
    prompts: HashMap<u64, Value>,
    service_lost: Option<Instant>,
    answered_questions: HashSet<(u64, String)>,
    /// Question cards the session's native transcript refused as answered or
    /// not its own; phones are no longer offered them.
    stale_questions: HashSet<(u64, String)>,
    devices: Vec<Device>,
    devices_stamp: Option<u64>,
    limiter: Limiter,
    /// The order file as the phones were last sent it, to notice the deck
    /// moving a session.
    order_stamp: Option<(u64, i64, i64)>,
    /// This machine's CPU, memory and disk use, for the Stats screen.
    resources: Sampler,
    /// The other hosts as the service last sent them (`ServerMsg::Hosts`).
    hosts: Vec<HostInfo>,
    /// The state message the phones were last sent.
    state_text: String,
}

pub struct Hub {
    pub opts: Options,
    pub epoch: String,
    inner: Mutex<Inner>,
    service: Mutex<Option<UnixStream>>,
    request_lock: Mutex<()>,
    control_jobs: AtomicUsize,
    question_lock: Mutex<()>,
    waiters: Mutex<Vec<Waiter>>,
    next_id: AtomicU64,
    pub chats: Mutex<chat::Cache>,
    /// The logs the phone's searches read, whole ([`search`]).
    search: Mutex<search::Index>,
    uploads: Mutex<Uploads>,
    links: Links,
}

impl Inner {
    /// The paired phones as the list on disk has them, written at `stamp`;
    /// the connections of any others end.
    fn adopt_devices(&mut self, devices: Vec<Device>, stamp: Option<u64>) {
        self.devices = devices;
        self.devices_stamp = stamp;
        let devices = &self.devices;
        self.conns.retain(|_, c| devices.iter().any(|d| d.id == c.device));
    }
}

/// Where a host's sessions come in the phone's list: this machine's first,
/// then the hosts in the deck's order, then any it no longer names.
fn host_rank(hosts: &[HostInfo], host: Option<&str>) -> usize {
    match host {
        None => 0,
        Some(name) => hosts.iter().position(|h| h.name == name).map_or(hosts.len() + 1, |i| i + 1),
    }
}

fn host_label(hosts: &[HostInfo], host: Option<&str>) -> Option<String> {
    let name = host?;
    Some(hosts.iter().find(|h| h.name == name).map_or_else(|| name.to_string(), |h| h.label.clone()))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct ReplySource {
    tag: Option<u16>,
    notice_prefix: Option<String>,
}

fn matches(want: Want, source: &ReplySource, msg: &ServerMsg) -> bool {
    if let ServerMsg::Created { id, .. } = msg {
        if source.tag.is_some_and(|tag| split_id(*id).0 != tag) {
            return false;
        }
    }
    let notice = |text: &str| match &source.notice_prefix {
        Some(prefix) => text.strip_prefix(prefix).map(str::to_string),
        None if source.tag.is_none() => {
            let candidate = text
                .split_once(": ")
                .map(|(_, tail)| tail)
                .filter(|tail| tail.starts_with("Could not start") || tail.starts_with("Could not resume"));
            Some(candidate.unwrap_or(text).to_string())
        }
        None => Some(text.to_string()),
    };
    match (want, msg) {
        (Want::Plugin(id), ServerMsg::PluginResult { request, .. }) => id == *request,
        (Want::TargetMessage(id, token), ServerMsg::MessageSent { id: got, request, .. }) => {
            id == *got && token == *request
        }
        (Want::TargetMessage(_, token), ServerMsg::PluginResult { request, error: Some(_), .. }) => token == *request,
        (Want::ErrorsCleared, ServerMsg::ErrorsCleared) => true,
        (Want::Created, ServerMsg::Created { .. }) => true,
        (Want::Created, ServerMsg::Notice { text, error: true }) => {
            notice(text).is_some_and(|t| t.starts_with("Could not start"))
        }
        (Want::Reopened(id), ServerMsg::Created { id: got, .. }) => *got == id,
        (Want::Reopened(_), ServerMsg::Notice { text, error: true }) => {
            notice(text).is_some_and(|t| t.starts_with("Could not reopen"))
        }
        (Want::Resumed, ServerMsg::Created { .. }) => true,
        (Want::Resumed, ServerMsg::Notice { text, error: true }) => {
            notice(text).is_some_and(|t| t.starts_with("Could not resume"))
        }
        (Want::Conversations, ServerMsg::Conversations { .. }) => true,
        (Want::Sent, ServerMsg::Notice { text, .. }) => {
            notice(text).is_some_and(|t| t.starts_with("Sent to") || t.starts_with("Type a message"))
        }
        (Want::Screen(id), ServerMsg::ScreenText { id: got, .. }) => *got == id,
        (Want::Answered(id), ServerMsg::Answered { id: got, .. }) => *got == id,
        (Want::MessageSent(id, request), ServerMsg::MessageSent { id: got, request: token, .. }) => {
            *got == id && *token == request
        }
        (Want::SentNow(id), ServerMsg::SentNow { id: got, .. }) => *got == id,
        _ => false,
    }
}

/// Sessions in the shared saved order, independent of execution host.
pub fn phone_sessions(state: &DeckState, order: &[u64], hosts: &[HostInfo]) -> Vec<Value> {
    // A terminal (the scratch one, `:term`) is the deck's own: the phone
    // reads agents' chats.
    let mut sessions: Vec<_> = state.sessions.iter().filter(|s| s.tool.is_agent()).collect();
    let place = |id: u64| order.iter().position(|o| *o == id).unwrap_or(usize::MAX);
    let cloud = |s: &SessionInfo| state.providers.iter().any(|p| p.cloud && p.id == s.tool.key() && p.host == s.host);
    sessions.sort_by_key(|s| (!cloud(s), !s.alive, place(s.id), s.created_at, s.id));
    sessions
        .into_iter()
        .map(|s| {
            json!({
                "host": s.host,
                "host_label": if cloud(s) { None } else { Some(host_label(hosts, s.host.as_deref())) },
                "id": s.id,
                "tool": s.tool,
                "title": s.title,
                "status": s.status,
                "detail": s.detail,
                "alive": s.alive,
                "branch": s.worktree.as_ref().and_then(|w| w.branch.clone()),
                "worktree": s.worktree.as_ref().map(|w| w.path.clone()),
                "task": s.worktree.as_ref().and_then(|w| w.task.clone()),
                "review": s.review,
                "spawned_ms": s.spawned_ms,
                "pid": s.pid,
                "conversation": s.conversation,
                "created_at": s.created_at,
                "status_since": s.status_since,
                "subagents": s.subagents,
            })
        })
        .collect()
}

/// Agents running outside the deck, which its Outside group lists; the phone
/// can move supported processes into the deck on their owning host.
pub fn phone_outside(state: &DeckState, hosts: &[HostInfo]) -> Vec<Value> {
    let mut outside: Vec<_> = state.outside.iter().collect();
    outside.sort_by_key(|o| host_rank(hosts, o.host.as_deref()));
    outside
        .into_iter()
        .map(|o| {
            json!({
                "host": o.host,
                "host_label": host_label(hosts, o.host.as_deref()),
                "pid": o.pid,
                "tool": o.tool,
                "title": o.title,
                "busy": o.busy,
                "cwd": o.cwd,
                "branch": o.worktree.as_ref().and_then(|w| w.branch.clone()),
                "worktree": o.worktree.as_ref().map(|w| w.path.clone()),
                "task": o.worktree.as_ref().and_then(|w| w.task.clone()),
                "started_at": o.started_at,
            })
        })
        .collect()
}

impl Hub {
    pub fn new(opts: Options, epoch: String) -> Hub {
        let devices = store::load_devices(&opts.dir);
        let devices_stamp = store::devices_stamp(&opts.dir);
        let uploads_dir = opts.uploads_dir.clone();
        let links = Links::gateway(opts.hosts.clone(), opts.hub_key.clone(), &opts.socket, opts.config_file.as_deref());
        Hub {
            links,
            opts,
            epoch,
            inner: Mutex::new(Inner { devices, devices_stamp, next_seq: 1, ..Inner::default() }),
            service: Mutex::new(None),
            request_lock: Mutex::new(()),
            control_jobs: AtomicUsize::new(0),
            question_lock: Mutex::new(()),
            waiters: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            chats: Mutex::new(chat::Cache::default()),
            search: Mutex::new(search::Index::default()),
            uploads: Mutex::new(Uploads::new(uploads_dir)),
        }
    }

    fn next(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    // ---- the service ----

    /// Keep a connection to the service, reading what it sends.
    pub fn bridge(self: &Arc<Self>) {
        loop {
            if let Ok(stream) = crate::client::connect_public_socket(&self.opts.socket) {
                if let Ok(writer) = stream.try_clone() {
                    let mut w = writer;
                    if write_msg(&mut w, &ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Remote })
                        .is_ok()
                    {
                        *lock(&self.service) = Some(w);
                        lock(&self.inner).service_lost = None;
                        let mut reader = BufReader::new(stream);
                        while let Ok(Some(msg)) = read_msg::<_, ServerMsg>(&mut reader) {
                            self.on_service(msg);
                        }
                        *lock(&self.service) = None;
                        self.service_unavailable();
                    }
                }
            }
            {
                let mut inner = lock(&self.inner);
                let lost = *inner.service_lost.get_or_insert_with(Instant::now);
                if lost.elapsed() >= self.opts.service_grace {
                    eprintln!("cepheus remote: the service is gone; exiting");
                    std::process::exit(0);
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
    }

    /// What the service sent: a reply someone waits for, or news for phones.
    pub(crate) fn on_service(&self, msg: ServerMsg) {
        {
            let mut waiters = lock(&self.waiters);
            if let Some(pos) = waiters.iter().position(|w| matches(w.want, &w.source, &msg)) {
                let w = waiters.remove(pos);
                let _ = w.tx.send(msg);
                return;
            }
        }
        match msg {
            ServerMsg::Welcome { start_dir, version, pid } => {
                self.service_unavailable();
                let mut inner = lock(&self.inner);
                inner.start_dir = Some(PathBuf::from(start_dir));
                inner.service_version = version;
                inner.service_pid = Some(pid);
            }
            ServerMsg::State(state) => self.take_state(StatePatch::default(), Some(state)),
            ServerMsg::StatePatch(patch) => self.take_state(patch, None),
            // A host came, went or changed its label: the headings follow.
            ServerMsg::Hosts { hosts } => {
                let mut inner = lock(&self.inner);
                inner.hosts = hosts;
                if let Some(text) = inner
                    .state
                    .as_ref()
                    .map(|state| self.state_message(state, &inner.hosts, inner.service_version, inner.service_ready))
                {
                    for c in inner.conns.values() {
                        let _ = c.tx.send(text.clone());
                    }
                    inner.state_text = text;
                }
            }
            ServerMsg::Alert(alert) => {
                let mut inner = lock(&self.inner);
                let seq = inner.next_seq;
                inner.next_seq += 1;
                let mut value = json!({
                    "t": "alert",
                    "seq": seq,
                    "epoch": self.epoch,
                    "kind": alert.kind,
                    "id": alert.id,
                    "title": alert.title,
                    "reason": alert.reason,
                    "desk_focused": alert.desk_focused,
                    "at": util::now_ms(),
                });
                // The phone's copy of an owner message the provider refused.
                if let Some(message) = &alert.message {
                    value["message"] = json!(message);
                }
                inner.alerts.push_back(value.clone());
                while inner.alerts.len() > ALERTS_KEPT {
                    inner.alerts.pop_front();
                }
                let text = value.to_string();
                for c in inner.conns.values() {
                    let _ = c.tx.send(text.clone());
                }
            }
            ServerMsg::Notice { text, error } => {
                self.push_all(&json!({"t": "notice", "text": text, "error": error}).to_string());
            }
            _ => {}
        }
    }

    /// A state from the service: whole, or the parts that changed since the
    /// last one, applied in place. The phones are sent their state message
    /// only when it changed.
    fn take_state(&self, patch: StatePatch, whole: Option<DeckState>) {
        if let Some(sessions) = whole.as_ref().map(|state| &state.sessions).or(patch.sessions.as_ref()) {
            lock(&self.chats).watch_sessions(sessions);
        }
        if let Some(providers) = whole.as_ref().map(|state| &state.providers).or(patch.providers.as_ref()) {
            crate::providers::advertise(providers);
        }
        let mut guard = lock(&self.inner);
        let inner = &mut *guard;
        let listed = |state: &DeckState| state.errors.iter().map(|entry| entry.text.clone()).collect::<HashSet<_>>();
        let listed_before = inner.state.as_ref().filter(|_| whole.is_some() || patch.errors.is_some()).map(listed);
        let cleared = match whole {
            Some(state) => {
                let cleared = inner.state.as_ref().is_none_or(|previous| previous.error_clears != state.error_clears);
                inner.state = Some(state);
                cleared
            }
            None => {
                // A patch only follows a whole state.
                let Some(state) = inner.state.as_mut() else { return };
                let cleared = patch.error_clears.as_ref().is_some_and(|clears| *clears != state.error_clears);
                state.apply(patch);
                cleared
            }
        };
        let Some(state) = inner.state.as_ref() else { return };
        inner.service_ready = true;
        inner.state_revision += 1;
        if cleared {
            inner.alerts.retain(|alert| alert.get("kind").and_then(Value::as_str) != Some("error"));
        } else if let Some(before) = listed_before {
            // An error the service took back, such as a login warning a
            // renewal answered, is not replayed to a phone that connects
            // later. An error not yet listed stays: its alert can arrive
            // before the state that lists it.
            let now = listed(state);
            let withdrawn: Vec<&String> = before.iter().filter(|text| !now.contains(*text)).collect();
            if !withdrawn.is_empty() {
                inner.alerts.retain(|alert| !withdrawn.iter().any(|text| reports(alert, text)));
            }
        }
        let text = self.state_message(state, &inner.hosts, inner.service_version, true);
        // A phone drops the card of a session this state does not
        // show waiting (`Deck.onState`). Forget it here too, so the
        // next scan sends it again if the question is still there:
        // a status that blipped between two scans left the phone
        // with no card while its messages were refused (2026-11-02).
        inner
            .prompts
            .retain(|id, _| state.sessions.iter().any(|s| s.id == *id && s.alive && s.status == Status::NeedsYou));
        // A missing route must not become a subscription to a later reused ID.
        for connection in inner.conns.values_mut() {
            if connection.open.is_some_and(|id| !state.sessions.iter().any(|session| session.id == id)) {
                connection.open = None;
            }
        }
        if text != inner.state_text {
            for c in inner.conns.values() {
                let _ = c.tx.send(text.clone());
            }
            inner.state_text = text;
        }
    }

    fn service_unavailable(&self) {
        let mut inner = lock(&self.inner);
        inner.service_ready = false;
        inner.service_generation += 1;
        inner.service_pid = None;
        inner.prompts.clear();
        for conn in inner.conns.values_mut() {
            conn.sent_total = 0;
            conn.sent_generation = 0;
            conn.sent_screen = None;
            conn.sent_stream = None;
        }
        if let Some(state) = inner.state.as_ref() {
            let text = self.state_message(state, &inner.hosts, inner.service_version, false);
            for conn in inner.conns.values() {
                let _ = conn.tx.send(text.clone());
            }
            inner.state_text = text;
        }
        drop(inner);
        // A reply from the replacement service must not complete an old operation.
        lock(&self.waiters).clear();
    }

    fn require_service_ready(&self) -> Result<()> {
        anyhow::ensure!(
            lock(&self.inner).service_ready,
            "The Cepheus service is not ready; waiting for its session state."
        );
        Ok(())
    }

    fn state_message(&self, state: &DeckState, hosts: &[HostInfo], service_version: u32, ready: bool) -> String {
        json!({
            "t": "state",
            "ready": ready,
            "attention":state.attention,
            "attention_capable":state.attention_capable,
            "protocol":service_version.min(PROTOCOL_VERSION),
            "sessions": phone_sessions(state, &self.order_for(state), hosts),
            "cloud_agents": state.providers.iter().filter(|p| p.cloud)
                .map(|p| json!({"id": p.id, "label": p.label, "host": p.host})).collect::<Vec<_>>(),
            "outside": phone_outside(state, hosts),
            "errors": state.errors,
            "start_dir": state.start_dir,
            "usage": state.usage,
            "here": state.native_host.as_ref().map(|host|host.label.clone()).or_else(||self.links.coordinator_label()).unwrap_or_else(|| self.opts.here.clone()),
            "hosts": hosts.iter().map(|h| json!({
                "name": h.name, "label": h.label, "online": h.online, "reason": h.reason,
                "agents": h.role.is_agents(),
            })).collect::<Vec<_>>(),
        })
        .to_string()
    }

    /// The session order the deck keeps (Super+↑/↓), oldest first when none.
    fn order(&self) -> Vec<u64> {
        std::fs::read(&self.opts.order_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn order_for(&self, state: &DeckState) -> Vec<u64> {
        if self.links.is_shared() || !state.session_order.is_empty() {
            state.session_order.clone()
        } else {
            self.order()
        }
    }

    /// Send every phone the state again, in the order the file now gives.
    fn push_order(&self) {
        let stamp = util::file_stamp(&self.opts.order_file);
        let mut inner = lock(&self.inner);
        inner.order_stamp = stamp;
        if let Some(text) = inner
            .state
            .as_ref()
            .map(|state| self.state_message(state, &inner.hosts, inner.service_version, inner.service_ready))
        {
            for c in inner.conns.values() {
                let _ = c.tx.send(text.clone());
            }
            inner.state_text = text;
        }
    }

    /// The phone's order goes in the deck's own file, so the deck follows it, and sessions the phone did not
    /// list keep their places after.
    fn reorder(&self, ids: &[u64]) -> Result<()> {
        let (version, state) = {
            let inner = lock(&self.inner);
            (inner.service_version, inner.state.clone())
        };
        if version >= 14 {
            if ids.len() > crate::protocol::MAX_SESSION_ORDER
                || ids.iter().any(|id| !state.as_ref().is_some_and(|s| s.sessions.iter().any(|s| s.id == *id)))
            {
                bail!("no such session or session order is too large");
            }
            control::exchange(
                &self.opts.socket,
                &ClientMsg::SetOrder { ids: ids.to_vec() },
                SERVICE_REPLY,
                |message| matches!(message, ServerMsg::Notice { text, error: false } if text == crate::protocol::ORDER_SAVED_NOTICE),
            )?;
            return Ok(());
        }
        if self.links.is_shared() {
            bail!("Shared session order needs an updated coordinator (protocol 14).");
        }
        {
            let inner = lock(&self.inner);
            let known: Vec<u64> =
                inner.state.as_ref().map(|s| s.sessions.iter().map(|s| s.id).collect()).unwrap_or_default();
            if ids.iter().any(|id| !known.contains(id)) {
                bail!("no such session");
            }
        }
        let mut order: Vec<u64> = Vec::new();
        for id in ids.iter().copied().chain(self.order()) {
            if !order.contains(&id) {
                order.push(id);
            }
        }
        let bytes = serde_json::to_vec(&order)?;
        if let Some(parent) = self.opts.order_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        util::write_atomic(&self.opts.order_file, &bytes).context("could not save the session order")?;
        self.push_order();
        Ok(())
    }

    /// The phone a connection signed in as.
    fn device_of(&self, conn: u64) -> Result<String> {
        lock(&self.inner).conns.get(&conn).map(|c| c.device.clone()).context("this phone is not signed in")
    }

    /// A piece of an upload, from a binary frame; the reply names the stored
    /// file once it is whole.
    pub fn piece(&self, conn: u64, piece: Piece, data: &[u8], tx: &mpsc::Sender<String>) {
        let result = self
            .device_of(conn)
            .and_then(|device| lock(&self.uploads).piece(&device, &piece.upload, piece.offset, data));
        let msg = match result {
            Ok(path) => json!({"t": "ok", "rid": piece.rid, "path": path}),
            Err(err) => json!({"t": "err", "rid": piece.rid, "error": format!("{err:#}")}),
        };
        let _ = tx.send(msg.to_string());
    }

    /// Sweep the uploads folder ([`Uploads::sweep`]); `now` skips its
    /// once-a-minute wait.
    pub fn clean_uploads(&self, now: bool) {
        let live: Option<Vec<u64>> =
            lock(&self.inner).state.as_ref().map(|s| s.sessions.iter().map(|s| s.id).collect());
        lock(&self.uploads).sweep(live.as_deref(), now);
    }

    /// The Stats screen: the agents' usage windows, this machine's use and each other host's.
    fn stats(&self) -> Value {
        let shared = self.links.is_shared();
        let published_stats = lock(&self.inner)
            .state
            .as_ref()
            .and_then(|state| state.native_host.as_ref())
            .and_then(|host| host.stats.clone());
        let source_stats = if shared {
            published_stats.or_else(|| match self.links.ask_source(None, &HostReq::PhoneStats, None) {
                Ok(HostResp::Stats { stats }) => Some(stats),
                _ => None,
            })
        } else {
            None
        };
        let mut inner = lock(&self.inner);
        if !shared && inner.resources.now.memory.is_none() {
            let dir = self.disk_dir(&inner);
            inner.resources.sample(&dir);
        }
        let now = &inner.resources.now;
        let hosts: Vec<Value> = inner
            .hosts
            .iter()
            .map(|h| {
                let s = h.stats.as_ref().filter(|_| h.online);
                let disk = s.and_then(|s| Some(json!({"used": s.disk_used?, "available": s.disk_available?})));
                json!({
                    "name": h.name, "label": h.label, "online": h.online, "reason": h.reason,
                    "cpu": s.and_then(|s| s.cpu), "memory": s.and_then(|s| s.memory), "disk": disk,
                })
            })
            .collect();
        json!({
            "t": "stats",
            "usage": inner.state.as_ref().map(|s| s.usage.clone()).unwrap_or_default(),
            "cpu": if shared { source_stats.as_ref().and_then(|s|s.cpu) } else { now.cpu },
            "memory": if shared { source_stats.as_ref().and_then(|s|s.memory) } else { now.memory },
            "disk": if shared { source_stats.as_ref().and_then(|s|Some(json!({"used":s.disk_used?,"available":s.disk_available?}))) } else { now.disk.map(|d| json!({"used": d.used, "available": d.available})) },
            "label": self.links.coordinator_label().unwrap_or_else(|| self.opts.here.clone()),
            "hosts": hosts,
            // What the deck's scripts show in its sidebar (`deck.ui.section`).
            "panels": inner.state.as_ref().map(|s| s.panels.clone()).unwrap_or_default(),
        })
    }

    fn host_online(&self, name: &str) -> bool {
        lock(&self.inner).hosts.iter().any(|h| h.name == name && h.online)
    }

    /// The session's chat as it stands, given to `f`: read here, or for a
    /// session on another host, first brought up to date from that host
    /// (kept as it was while the host is out of reach).
    fn with_chat<T>(&self, info: &SessionInfo, f: impl FnOnce(Option<&chat::Log>) -> T) -> T {
        let source = info.host.as_deref();
        if source.is_none() && !self.links.is_shared() {
            let dirs = self.dirs();
            return f(lock(&self.chats).get(info, &dirs));
        }
        let chunk = source.is_none_or(|host| self.host_online(host)).then(|| self.host_chunk(source, info)).flatten();
        let mut chats = lock(&self.chats);
        match chunk {
            Some(chunk) => f(chats.apply(info.id, chunk)),
            None => f(chats.peek(info.id)),
        }
    }

    /// What the copy of a remote session's chat lacks, from its host.
    fn host_chunk(&self, host: Option<&str>, info: &SessionInfo) -> Option<chat::Chunk> {
        let (from, generation) = lock(&self.chats)
            .peek(info.id)
            .filter(|l| l.source == Source::Remote)
            .map_or((0, 0), |l| (l.total(), l.generation));
        let session = SessionInfo { id: split_id(info.id).1, host: None, ..info.clone() };
        match self.links.ask_source(host, &HostReq::Chat { session: Box::new(session), from, generation }, None) {
            Ok(HostResp::Chat { chunk }) => Some(chunk),
            _ => None,
        }
    }

    /// Copy files the phone attached to the host a session runs on; the
    /// message names them where they are kept there. With `hashes`, each
    /// copy must still hold the bytes hashed when the owner sent it.
    fn copy_to_host(&self, host: Option<&str>, paths: &[PathBuf], hashes: &[String]) -> Result<Vec<PathBuf>> {
        anyhow::ensure!(hashes.is_empty() || hashes.len() == paths.len(), "invalid upload digests");
        paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let bytes = std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
                anyhow::ensure!(
                    hashes.is_empty() || auth::sha256_hex(&bytes) == hashes[index],
                    "uploaded image changed; nothing was sent"
                );
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                match self.links.ask_source(host, &HostReq::PutFile { name, len: bytes.len() as u64 }, Some(&bytes))? {
                    HostResp::Put { path } => Ok(PathBuf::from(path)),
                    other => Err(links::unexpected(&other)),
                }
            })
            .collect()
    }

    /// The disk the sessions start on, which the deck's header measures.
    fn disk_dir(&self, inner: &Inner) -> PathBuf {
        inner
            .state
            .as_ref()
            .map(|s| s.start_dir.clone())
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| inner.start_dir.clone())
            .unwrap_or_else(crate::paths::home)
    }

    fn send_service(&self, msg: &ClientMsg) -> Result<()> {
        let generation = self.session_state_marker()?.0;
        self.send_service_at(msg, generation)
    }

    fn send_service_at(&self, msg: &ClientMsg, generation: u64) -> Result<()> {
        let mut service = lock(&self.service);
        let inner = lock(&self.inner);
        anyhow::ensure!(
            inner.service_ready && inner.service_generation == generation,
            "The Cepheus service connection changed; check the session before retrying."
        );
        drop(inner);
        let stream = service.as_mut().context("the Cepheus service is not running")?;
        write_msg(stream, msg).context("the Cepheus service connection broke")
    }

    /// Send `msg` and wait for the service's reply to it. One request is in
    /// flight at a time, so the reply that matches is this one's.
    pub fn request(&self, msg: &ClientMsg, want: Want) -> Result<ServerMsg> {
        self.request_within(msg, want, SERVICE_REPLY)
    }

    fn request_within(&self, msg: &ClientMsg, want: Want, wait: Duration) -> Result<ServerMsg> {
        // A request queued behind another operation still belongs to its original service.
        let generation = self.session_state_marker()?.0;
        self.request_within_at(msg, want, wait, generation)
    }

    fn request_within_at(&self, msg: &ClientMsg, want: Want, wait: Duration, generation: u64) -> Result<ServerMsg> {
        let _one = (!matches!(want, Want::MessageSent(..))).then(|| lock(&self.request_lock));
        let source = self.reply_source(msg, want)?;
        let (tx, rx) = mpsc::channel();
        let id = self.next();
        lock(&self.waiters).push(Waiter { id, want, source, tx });
        let sent = self.send_service_at(msg, generation);
        let reply = sent.and_then(|()| rx.recv_timeout(wait).context("the service did not answer"));
        lock(&self.waiters).retain(|w| w.id != id);
        reply
    }

    /// Send a start, resume or reopen on a connection of its own, to the
    /// service the bridge reached at `marker`, and wait for its reply there.
    /// The service's answers to a resume or reopen, and its failure notices,
    /// carry no token: on the shared bridge one that came after its request
    /// gave up answered the next phone's start with the wrong session
    /// (ROOT-033). A private connection closes when its request gives up, and
    /// a late reply goes with it.
    fn request_alone(&self, msg: &ClientMsg, want: Want, marker: (u64, u64)) -> Result<ServerMsg> {
        let source = self.reply_source(msg, want)?;
        let wait = if matches!(msg, ClientMsg::Create { tool: Tool::Codex | Tool::OpenCode, .. }) {
            Duration::from_secs(35)
        } else {
            SERVICE_REPLY
        };
        self.exchange_private(msg, wait, marker, |reply| matches(want, &source, reply))
    }

    /// Which replies can be the service's answer to `msg`: those from the host
    /// it names or its session runs on.
    fn reply_source(&self, msg: &ClientMsg, want: Want) -> Result<ReplySource> {
        // Session IDs already carry coordinator tags. The viewer's own host
        // inventory has different tags and must never reinterpret these IDs.
        let inner = lock(&self.inner);
        let explicit = match msg {
            ClientMsg::Create { host: Some(name), .. } | ClientMsg::Resume { host: Some(name), .. }
                if name != "@here" =>
            {
                Some(name.as_str())
            }
            _ => None,
        };
        let id = match msg {
            ClientMsg::Reopen { id }
            | ClientMsg::Screen { id, .. }
            | ClientMsg::Answer { id, .. }
            | ClientMsg::SendMessage { id, .. }
            | ClientMsg::SendNow { id } => Some(*id),
            ClientMsg::Broadcast { ids, .. } if ids.len() == 1 => Some(ids[0]),
            _ => None,
        };
        let session = id.and_then(|id| inner.state.as_ref()?.sessions.iter().find(|s| s.id == id));
        let host_name = explicit.or_else(|| session.and_then(|s| s.host.as_deref()));
        let host = host_name
            .map(|name| inner.hosts.iter().find(|host| host.name == name).context("unknown host"))
            .transpose()?;
        let tag = id.map(|id| split_id(id).0).or_else(|| {
            host_name.and_then(|name| {
                inner
                    .state
                    .as_ref()?
                    .sessions
                    .iter()
                    .find(|s| s.host.as_deref() == Some(name))
                    .map(|s| split_id(s.id).0)
            })
        });
        if let Some(host) = host {
            if matches!(want, Want::Created | Want::Reopened(_) | Want::Resumed | Want::Sent)
                && inner.hosts.iter().filter(|other| other.label == host.label).count() != 1
            {
                bail!("Give each host a distinct label before controlling it from the phone");
            }
        }
        Ok(ReplySource { tag, notice_prefix: host.map(|host| format!("{}: ", host.label)) })
    }

    pub fn screen(&self, id: u64) -> Result<(Vec<String>, u64)> {
        match self.request(&ClientMsg::Screen { id, tail: None }, Want::Screen(id))? {
            ServerMsg::ScreenText { lines, inputs, .. } => Ok((lines, inputs)),
            _ => bail!("unexpected reply"),
        }
    }

    /// Deliver to the existing session through its native provider interface.
    pub fn type_message(&self, id: u64, text: &str) -> Result<()> {
        self.send_message(id, text, false)
    }

    pub fn type_broadcast(&self, id: u64, text: &str) -> Result<()> {
        if lock(&self.inner).service_version < crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL {
            bail!("Update needed: direct agent messages require Cepheus protocol 19; nothing was sent");
        }
        match self.request_within(
            &ClientMsg::Broadcast { running_only: true, ids: vec![id], text: text.into() },
            Want::Sent,
            Duration::from_secs(50),
        )? {
            ServerMsg::Notice { text, error: true } => bail!("{text}"),
            _ => Ok(()),
        }
    }

    fn send_message(&self, id: u64, text: &str, attachments: bool) -> Result<()> {
        anyhow::ensure!(
            lock(&self.inner).service_version >= crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL,
            "Update needed: direct agent messages require Cepheus protocol 19; nothing was sent"
        );
        let request = self.next();
        let message = ClientMsg::SendMessage { id, request, text: text.into(), attachments };
        match self
            .request_within(&message, Want::MessageSent(id, request), Duration::from_secs(40))
            .map_err(|_| anyhow!("delivery status is unknown; check the conversation before retrying"))?
        {
            ServerMsg::MessageSent { error: Some(error), .. } => bail!("{error}"),
            _ => Ok(()),
        }
    }

    fn input(&self, id: u64, data: &str) -> Result<()> {
        self.send_service(&ClientMsg::Input { id, data: data.to_string() })
    }

    pub fn session(&self, id: u64) -> Option<crate::protocol::SessionInfo> {
        lock(&self.inner).state.as_ref()?.sessions.iter().find(|s| s.id == id).cloned()
    }

    fn dirs(&self) -> chat::Dirs {
        let start_dir = lock(&self.inner).start_dir.clone().unwrap_or_default();
        chat::Dirs {
            claude: self.opts.claude_dir.clone(),
            codex: self.opts.codex_dir.clone(),
            opencode_db: self.opts.opencode_db.clone(),
            antigravity: self.opts.antigravity_dir.clone(),
            start_dir,
            sqlite3: self.opts.sqlite3.clone(),
        }
    }

    // ---- phones ----

    /// Record a newly paired phone.
    pub fn add_device(&self, device: Device) -> Result<()> {
        let mut inner = lock(&self.inner);
        let devices = store::update_devices(&self.opts.dir, |devices| {
            devices.push(device);
            true
        })?;
        inner.adopt_devices(devices, store::devices_stamp(&self.opts.dir));
        Ok(())
    }

    /// Whether `device` is paired with `key`, by the list on disk now, noting
    /// when it was seen. A phone the deck's pairing screen removed is refused
    /// at once, and the list written back keeps it removed (ROOT-035).
    pub fn sign_in(&self, device: &str, key: &str) -> bool {
        let mut inner = lock(&self.inner);
        let mut known = false;
        let updated = store::update_devices(&self.opts.dir, |devices| {
            if let Some(index) = crate::remote::auth::find_device(devices, device, key) {
                devices[index].last_seen = Some(util::now_secs());
                known = true;
            }
            known
        });
        match updated {
            Ok(devices) => inner.adopt_devices(devices, store::devices_stamp(&self.opts.dir)),
            // Unable to lock or save: the phone still needs to be on the list.
            Err(_) => {
                let devices = store::load_devices(&self.opts.dir);
                known = crate::remote::auth::find_device(&devices, device, key).is_some();
            }
        }
        known
    }

    pub fn limiter_open(&self, ip: IpAddr) -> bool {
        lock(&self.inner).limiter.open(ip, Instant::now())
    }

    pub fn limiter_fail(&self, ip: IpAddr) {
        lock(&self.inner).limiter.fail(ip, Instant::now());
    }

    pub fn limiter_succeed(&self, ip: IpAddr) {
        lock(&self.inner).limiter.succeed(ip);
    }

    pub fn connections(&self) -> usize {
        lock(&self.inner).conns.len()
    }

    /// Register a connected phone; what it is sent first is queued on `tx`.
    pub fn add_conn(&self, device: &str, tx: mpsc::Sender<String>, last_seq: Option<u64>, epoch: Option<&str>) -> u64 {
        let id = self.next();
        let mut inner = lock(&self.inner);
        let _ = tx.send(json!({"t": "hello", "epoch": self.epoch, "version": env!("CARGO_PKG_VERSION")}).to_string());
        if let Some(state) = inner.state.as_ref() {
            let _ = tx.send(self.state_message(state, &inner.hosts, inner.service_version, inner.service_ready));
        }
        // What came before this connection is marked `replay`: the phone records a finish or a question in it
        // but does not notify it.
        let replay = |message: &Value| {
            let mut message = message.clone();
            if let Some(fields) = message.as_object_mut() {
                fields.insert("replay".into(), Value::Bool(true));
            }
            message.to_string()
        };
        let same_epoch = epoch == Some(self.epoch.as_str());
        for alert in &inner.alerts {
            let seq = alert.get("seq").and_then(Value::as_u64).unwrap_or(0);
            if (!same_epoch || last_seq.is_none_or(|last| seq > last)) && still_current(inner.state.as_ref(), alert) {
                let _ = tx.send(replay(alert));
            }
        }
        for prompt in inner.prompts.values() {
            let _ = tx.send(replay(prompt));
        }
        inner.conns.insert(
            id,
            Conn {
                device: device.to_string(),
                tx,
                open: None,
                sent_total: 0,
                sent_generation: 0,
                sent_screen: None,
                sent_model: chat::ModelSeen::default(),
                sent_stream: None,

                sent_questions: Vec::new(),
            },
        );
        id
    }

    pub fn remove_conn(&self, id: u64) {
        lock(&self.inner).conns.remove(&id);
    }

    /// Whether the connection may go on: its phone was not removed meanwhile.
    pub fn conn_alive(&self, id: u64) -> bool {
        lock(&self.inner).conns.contains_key(&id)
    }

    fn push_all(&self, text: &str) {
        for c in lock(&self.inner).conns.values() {
            let _ = c.tx.send(text.to_string());
        }
    }

    // ---- requests ----

    /// Carry out a phone's request. The reply goes to `tx`, at once or, for
    /// a request that waits on a session, from a thread of its own.
    pub fn handle(self: &Arc<Self>, conn: u64, req: Req, tx: &mpsc::Sender<String>) {
        let rid = req.rid();
        let reply = move |result: Result<Value>| {
            let msg = match result {
                Ok(mut v) => {
                    if v.get("t").is_none() {
                        v["t"] = json!("ok");
                    }
                    v["rid"] = json!(rid);
                    v
                }
                Err(err) => {
                    let mut msg = json!({"t": "err", "rid": rid, "error": format!("{err:#}")});
                    if crate::protocol::is_question_stale(&err) {
                        msg["code"] = json!(crate::protocol::QUESTION_STALE);
                    }
                    msg
                }
            };
            msg.to_string()
        };
        let slow = matches!(
            req,
            Req::Control { .. }
                | Req::New { .. }
                | Req::Model { .. }
                | Req::Conversations { .. }
                | Req::Resume { .. }
                | Req::Search { .. }
                | Req::QuestionReply { .. }
                | Req::Image { .. }
        );
        if slow {
            let permit = if matches!(req, Req::Control { .. }) {
                match self.begin_control() {
                    Some(permit) => Some(permit),
                    None => {
                        let _ = tx.send(reply(Err(anyhow!("four controls are running; try again shortly"))));
                        return;
                    }
                }
            } else {
                None
            };
            let hub = Arc::clone(self);
            let tx = tx.clone();
            thread::spawn(move || {
                let _permit = permit;
                let result = hub.run(conn, req);
                let _ = tx.send(reply(result));
            });
            return;
        }
        let _ = tx.send(reply(self.run(conn, req)));
    }

    fn live_session(&self, id: u64) -> Result<crate::protocol::SessionInfo> {
        self.require_service_ready()?;
        self.session(id).context("no such session")
    }

    fn run(self: &Arc<Self>, conn: u64, req: Req) -> Result<Value> {
        if !matches!(req, Req::Ping { .. } | Req::Open { id: None, .. }) {
            self.require_service_ready()?;
        }
        match req {
            Req::Control { action, args, .. } => {
                self.device_of(conn)?;
                let data = self.control(&action, args)?;
                Ok(json!({"t": "control", "action": action, "data": data}))
            }
            Req::Ping { .. } => Ok(json!({"t": "pong"})),
            Req::Models { .. } => {
                let tools = if self.links.is_shared() {
                    match self.links.ask_source(None, &HostReq::PhoneModels, None)? {
                        HostResp::PhoneModels { mut tools } => {
                            // Defaults belong to the coordinator; host-only
                            // providers still come from the shared state menu.
                            let items = tools.as_array_mut().context("invalid shared model menu")?;
                            for tool in models::all(&self.model_homes()) {
                                if Tool::builtin(tool.tool).is_none()
                                    && !items.iter().any(|item| item["tool"] == tool.tool)
                                {
                                    items.push(serde_json::to_value(tool)?);
                                }
                            }
                            tools
                        }
                        other => return Err(links::unexpected(&other)),
                    }
                } else {
                    serde_json::to_value(self.model_menu())?
                };
                Ok(json!({"t": "models", "tools": tools}))
            }
            Req::Commands { id, .. } => {
                self.device_of(conn)?;
                let info = self.live_session(id)?;
                // A session on another host runs that host's skills.
                if info.host.is_some() || self.links.is_shared() {
                    let cwd = info.worktree.as_ref().map(|w| w.path.clone()).or_else(|| info.cwd.clone());
                    let entries = match self.links.ask_source(
                        info.host.as_deref(),
                        &HostReq::Commands { tool: info.tool, cwd },
                        None,
                    )? {
                        HostResp::Commands { entries } => entries,
                        other => return Err(links::unexpected(&other)),
                    };
                    return Ok(json!({"t": "commands", "id": id, "tool": info.tool, "entries": entries}));
                }
                let cwd = info
                    .worktree
                    .as_ref()
                    .map(|w| PathBuf::from(&w.path))
                    .or_else(|| lock(&self.inner).state.as_ref().map(|s| PathBuf::from(&s.start_dir)))
                    .unwrap_or_default();
                let entries =
                    commands::all(info.tool, &cwd, &crate::paths::home(), &self.opts.claude_dir, &self.opts.codex_dir)?;
                Ok(json!({"t": "commands", "id": id, "tool": info.tool, "entries": entries}))
            }
            Req::Image { id, key, .. } => {
                self.device_of(conn)?;
                let info = self.live_session(id)?;
                let (media, data) = match info.host.as_deref() {
                    source if source.is_some() || self.links.is_shared() => {
                        let session = SessionInfo { id: split_id(info.id).1, host: None, ..info.clone() };
                        match self.links.ask_source(
                            source,
                            &HostReq::Image { session: Box::new(session), key: key.clone() },
                            None,
                        )? {
                            HostResp::Image { media, data } => (media, data),
                            other => return Err(links::unexpected(&other)),
                        }
                    }
                    _ => {
                        let dirs = self.dirs();
                        let source = lock(&self.chats).get(&info, &dirs).map(|l| l.source.clone());
                        source.context("no such conversation")?.image(&key)?
                    }
                };
                Ok(json!({"t": "image", "id": id, "key": key, "media": media, "data": data}))
            }
            Req::Chat { id, before, limit, .. } => self.chat_page(id, before, limit, None),
            Req::Open { id, limit, .. } => {
                if let Some(id) = id {
                    self.live_session(id)?;
                }
                {
                    let mut inner = lock(&self.inner);
                    if let Some(c) = inner.conns.get_mut(&conn) {
                        c.open = id;
                    }
                }
                match id {
                    Some(id) => self.chat_page(id, None, limit, Some(conn)),
                    None => Ok(json!({})),
                }
            }
            Req::Send { id, text, files, relocate, client_id, .. } => {
                anyhow::ensure!(
                    lock(&self.inner).service_version >= crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL,
                    "Update needed: direct agent messages require Cepheus protocol 19; nothing was sent"
                );
                let (lease, marker) = self.target_lease_marker(id)?;
                let clear_target = relocate && files.is_empty() && matches!(text.trim(), "/clear" | "/new");
                let info = self.target_session(&lease)?;
                let device = self.device_of(conn)?;
                let mut paths = lock(&self.uploads).paths(&device, id, &files)?;
                if info.tool == Tool::Dynamic("dot") {
                    anyhow::ensure!(
                        lock(&self.inner).service_version >= 19,
                        "Direct Dot sending needs the updated Cepheus service; the draft and images are kept."
                    );
                    self.target_session(&lease)?;
                    let images = paths
                        .iter()
                        .map(|p| crate::dot::api::Image::from_file(p).map_err(anyhow::Error::msg))
                        .collect::<Result<Vec<_>>>()?;
                    let request = self.next();
                    let control = images.is_empty().then(|| crate::dot::api::Control::parse(&text)).flatten();
                    let message = ClientMsg::Plugin {
                        request,
                        plugin: "provider-dot".into(),
                        command: "send".into(),
                        args: json!({"id":lease.local_id,"host":lease.host,"expected":lease.expected,"text":text,"images":images}),
                    };
                    match self.request_within_at(
                        &message,
                        Want::Plugin(request),
                        Duration::from_secs(125),
                        lease.generation,
                    )? {
                        ServerMsg::PluginResult { error: Some(error), .. } => bail!("{error}"),
                        ServerMsg::PluginResult { data, error: None, .. }
                            if crate::dot::api::acknowledged(&data, control) => {}
                        _ => bail!("Dot delivery status is unknown; check the conversation before retrying."),
                    }
                    lock(&self.uploads).sent(&files);
                    return Ok(json!({}));
                }
                // Codex takes pictures through its own interface and Claude
                // any file by its path; the digest taken now is checked again
                // on the host just before sending.
                let native_images = matches!(info.tool, Tool::Codex | Tool::Claude) && !paths.is_empty();
                let hashes: Vec<String> = if native_images {
                    paths
                        .iter()
                        .map(|path| std::fs::read(path).map(|bytes| auth::sha256_hex(&bytes)))
                        .collect::<std::io::Result<_>>()?
                } else {
                    Vec::new()
                };
                if !paths.is_empty() && (info.host.is_some() || self.links.is_shared()) {
                    paths = self.copy_to_host(info.host.as_deref(), &paths, &hashes)?;
                }
                let image_entries: Vec<_> = paths.iter().cloned().zip(hashes).collect();
                let message = if native_images { text.clone() } else { uploads::with_files(&text, &paths) };
                if (message.trim().is_empty() && !native_images) || message.len() > SEND_MAX {
                    bail!("the message is empty or too long");
                }
                // The phone finds its copy of this message in the program's log by these: when it left this
                // gateway, on a desktop clock as the log's times are, and the paths written after the words,
                // which are the host's own for a session on another host. With the phone's own clock and paths,
                // a message whose log was new or read afresh, or that carried a file to another host, stayed on
                // the phone twice.
                let sent_at = util::now_ms();
                let mut delivered = None;
                let moved = if clear_target {
                    self.target_clear(&lease, marker)
                } else {
                    self.target_send_with_attachments(
                        &lease,
                        &message,
                        !paths.is_empty() && !native_images,
                        &image_entries,
                        client_id.as_deref(),
                    )
                    .map(|receipt| {
                        delivered = receipt;
                        None
                    })
                }?;
                lock(&self.uploads).sent(&files);
                Ok(moved.map_or_else(
                    || json!({"delivered":delivered,"client_id":client_id,"at":sent_at,"paths":paths}),
                    |id| json!({"id":id}),
                ))
            }
            Req::Upload { id, name, size, sha256, .. } => {
                if self.live_session(id)?.tool == Tool::Dynamic("dot") && size > crate::dot::api::IMAGE_MAX as u64 {
                    bail!("Dot images may be at most 20 MiB.");
                }
                let device = self.device_of(conn)?;
                let mut uploads = lock(&self.uploads);
                let token = uploads.start(&device, id, &name, size, &sha256)?;
                // An empty file is whole at once.
                let path =
                    uploads.paths(&device, id, std::slice::from_ref(&token)).ok().and_then(|p| p.into_iter().next());
                Ok(json!({"upload": token, "path": path}))
            }
            Req::Answer { id, inputs, fp, choice, choices, text, .. } => {
                let msg = ClientMsg::Answer { id, inputs, prompt: fp, choice, choices, text };
                match self.request(&msg, Want::Answered(id))? {
                    ServerMsg::Answered { error: Some(err), stale: true, .. } => Err(QuestionStale(err).into()),
                    ServerMsg::Answered { error: Some(err), .. } => bail!("{err}"),
                    _ => Ok(json!({})),
                }
            }
            Req::QuestionReply { id, question, choice, text, .. } => {
                let _serial = lock(&self.question_lock);
                let lease = self.target_lease(id)?;
                let info = self.target_session(&lease)?;
                if !info.alive || info.tool != Tool::Codex {
                    return Err(QuestionStale("this session has no Codex question".into()).into());
                }
                if lock(&self.inner).answered_questions.contains(&(id, question.clone())) {
                    return Err(QuestionStale("this question was already answered".into()).into());
                }
                let (asked, native_allowed, source) = self
                    .with_chat(&info, |log| {
                        let log = log?;
                        Some((
                            super::questionpick::by_id(&log.questions, &question)?,
                            super::questionpick::lookup(&log.questions, &question).is_some(),
                            log.source.clone(),
                        ))
                    })
                    .ok_or_else(|| {
                        QuestionStale(
                            "this question has already been answered or is no longer in this conversation".into(),
                        )
                    })?;
                let text = text.as_deref().map(str::trim).filter(|t| !t.is_empty());
                let answer = match (choice, text) {
                    (Some(index), None) => asked.options.get(index).context("no such answer option")?.as_str(),
                    (None, Some(text))
                        if text.len() <= SEND_MAX
                            && !text.chars().any(|c| c.is_control() && c != '\n' && c != '\t') =>
                    {
                        text
                    }
                    _ => bail!("choose an option or provide one text answer"),
                };
                let mut io = self.as_ref();
                let native = match self
                    .codex_control_at(&lease, json!({"action":"question_reply","question":question,"answer":answer}))
                {
                    Err(err) if crate::protocol::is_question_stale(&err) => {
                        // The conversation's own transcript has no such open
                        // question: stop offering its card, whose answer
                        // cannot be delivered here.
                        lock(&self.inner).stale_questions.insert((id, question));
                        return Err(err);
                    }
                    other => other?,
                };
                if native.is_some() {
                    // Admission is native and thread-bound. The canonical reply
                    // receipt, rather than rendered text, resolves the card.
                    let deadline = Instant::now() + Duration::from_secs(4);
                    loop {
                        self.target_session(&lease)?;
                        if self.with_chat(&info, |log| {
                            log.is_some_and(|log| log.source == source && log.question_answered(&question))
                        }) {
                            break;
                        }
                        if Instant::now() >= deadline {
                            bail!(
                                "Codex accepted this answer but has not recorded its reply yet; check the conversation before retrying"
                            );
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                } else if let Some((shown, inputs)) = super::questionpick::find(&mut io, id, &asked, native_allowed)? {
                    let msg = ClientMsg::Answer {
                        id,
                        inputs,
                        prompt: shown.fingerprint(),
                        choice: choice.unwrap_or_else(|| shown.free_text.unwrap_or(shown.options.len())),
                        choices: vec![],
                        text: text.map(str::to_string),
                    };
                    if let ServerMsg::Answered { error: Some(err), .. } = self.request(&msg, Want::Answered(id))? {
                        bail!("{err}");
                    }
                    // A key write can be refused by Codex (for example, a
                    // clipped option). Only its logged reply confirms an answer.
                    let deadline = Instant::now() + Duration::from_secs(4);
                    loop {
                        let answered = self.with_chat(&info, |log| {
                            log.is_some_and(|log| log.source == source && log.question_answered(&question))
                        });
                        if answered {
                            break;
                        }
                        if Instant::now() >= deadline {
                            bail!(
                                "Codex has not accepted this answer; the question is still open. Try again after its full options are visible."
                            );
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                } else {
                    // Native editors close at turn end. Keep an unanswered
                    // phone card usable by sending its explicit context normally;
                    // the service still refuses input into another open prompt.
                    let message = match choice {
                        Some(choice) => super::questionpick::follow_up(&asked, choice)?,
                        None => super::questionpick::follow_up_text(&asked, answer)?,
                    };
                    self.target_send(&lease, &message)?;
                }
                lock(&self.inner).answered_questions.insert((id, question));
                Ok(json!({}))
            }
            Req::Stop { id, .. } => {
                if self.live_session(id)?.tool == Tool::Dynamic("dot") {
                    bail!("Stop Dot's cloud work in ChatGPT; closing the deck only disconnects its bridge.");
                }
                // A service with the native Stop ends a Claude turn through
                // Claude's plugin, which keeps the owner's queued messages.
                if lock(&self.inner).service_version >= crate::protocol::NATIVE_STOP_PROTOCOL {
                    self.send_service(&ClientMsg::Stop { id })?;
                } else {
                    self.input(id, "\x1b")?;
                }
                Ok(json!({}))
            }
            // Claude Code's send now on the messages queued behind its turn,
            // as the phone's "Send now" under a queued message asks.
            Req::SendNow { id, .. } => {
                let info = self.live_session(id)?;
                if info.tool != Tool::Claude {
                    bail!("only Claude Code sends a queued message now");
                }
                if lock(&self.inner).service_version < crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL {
                    bail!("Update needed: direct agent messages require Cepheus protocol 19; nothing was sent");
                }
                match self.request(&ClientMsg::SendNow { id }, Want::SentNow(id))? {
                    ServerMsg::SentNow { error: Some(err), .. } => bail!("{err}"),
                    _ => Ok(json!({})),
                }
            }
            Req::Close { id, .. } => {
                self.live_session(id)?;
                self.send_service(&ClientMsg::Close { id })?;
                Ok(json!({}))
            }
            Req::Reopen { id, .. } => {
                let marker = self.session_state_marker()?;
                match self.request_alone(&ClientMsg::Reopen { id }, Want::Reopened(id), marker)? {
                    ServerMsg::Notice { text, .. } => bail!("{text}"),
                    _ => {
                        self.wait_for_session_state(id, marker)?;
                        Ok(json!({"id": id}))
                    }
                }
            }
            Req::Conversations { .. } => {
                self.device_of(conn)?;
                match self.request_within(
                    &ClientMsg::ListConversations { request: None, query: String::new() },
                    Want::Conversations,
                    LIST_REPLY,
                )? {
                    ServerMsg::Conversations { items, .. } => Ok(json!({"t": "conversations", "items": items})),
                    _ => bail!("unexpected reply"),
                }
            }
            Req::Resume { tool, conversation, host, .. } => {
                self.device_of(conn)?;
                if let Some(name) = host.as_deref() {
                    if !self.host_online(name) {
                        let label = host_label(&lock(&self.inner).hosts, Some(name)).unwrap_or_default();
                        bail!("{label} is not reachable now");
                    }
                }
                // The id becomes an argument of the program's command line.
                if !service::valid_conversation_id(&conversation) {
                    bail!("invalid conversation id");
                }
                let expected_conversation = conversation.clone();
                let expected_host = host.clone();
                let marker = self.session_state_marker()?;
                match self.request_alone(&ClientMsg::Resume { tool, conversation, host }, Want::Resumed, marker)? {
                    ServerMsg::Created { id, .. } => {
                        anyhow::ensure!(
                            self.session_state_marker()?.0 == marker.0,
                            "The Cepheus service connection changed; check Home before resuming again."
                        );
                        let existing = self.session(id).is_some_and(|session| {
                            session.alive
                                && session.tool == tool
                                && session.conversation.as_deref() == Some(expected_conversation.as_str())
                                && session.host == expected_host
                        });
                        if !existing {
                            self.wait_for_session_state(id, marker)?;
                        }
                        Ok(json!({"id": id}))
                    }
                    ServerMsg::Notice { text, .. } => bail!("{text}"),
                    _ => bail!("unexpected reply"),
                }
            }
            Req::ClearErrors { .. } => {
                if lock(&self.inner).service_version >= 14 {
                    self.request(&ClientMsg::ClearErrors, Want::ErrorsCleared)?;
                } else {
                    self.send_service(&ClientMsg::ClearErrors)?;
                }
                Ok(json!({}))
            }
            Req::Reorder { ids, .. } => {
                self.reorder(&ids)?;
                Ok(json!({}))
            }
            Req::Stats { .. } => Ok(self.stats()),
            Req::Search { q, .. } => {
                self.device_of(conn)?;
                let words = search::words(&q)?;
                Ok(json!({"t": "search", "q": q, "hits": self.search(&words)}))
            }
            Req::New { tool, model, effort, text, host, .. } => self.start(tool, model, effort, text, host),
            Req::Model { id, model, effort, .. } => {
                let lease = self.target_lease(id)?;
                self.switch_model_at(&lease, model.as_deref(), effort.as_deref())?;
                Ok(json!({}))
            }
        }
    }

    /// A page of a session's chat ending before `before` (the newest when
    /// none). For `follow`, the connection is sent what follows it.
    fn chat_page(&self, id: u64, before: Option<usize>, limit: Option<usize>, follow: Option<u64>) -> Result<Value> {
        let info = self.live_session(id)?;
        let limit = limit.unwrap_or(PAGE).clamp(1, PAGE_MAX);
        let (submitted, stale) = {
            let inner = lock(&self.inner);
            (inner.answered_questions.clone(), inner.stale_questions.clone())
        };
        let page = self.with_chat(&info, |log| {
            let log = log?;
            if log.source == Source::Screen {
                return Some((log.generation, None));
            }
            let total = log.total();
            let end = before.unwrap_or(total).min(total).max(log.dropped);
            let start = end.saturating_sub(limit).max(log.dropped);
            let items = &log.items[start - log.dropped..end - log.dropped];
            let reply = json!({
                "t": "chat", "id": id, "start": start, "total": total, "generation": log.generation,
                "source": "log", "items": items, "retained_from": log.dropped,
                "model": log.seen.model, "effort": log.seen.effort,
                "questions": phone_questions(&log.questions, id, &submitted, &stale),
            });
            Some((log.generation, Some((reply, total, log.seen.clone()))))
        });
        let Some((generation, page)) = page else {
            return Ok(
                json!({"t": "chat", "id": id, "start": 0, "total": 0, "items": [], "source": "none", "generation": 0}),
            );
        };
        if let Some((reply, total, seen)) = page {
            if let Some(conn) = follow {
                if let Some(c) = lock(&self.inner).conns.get_mut(&conn) {
                    c.sent_total = total;
                    c.sent_generation = generation;
                    c.sent_model = seen;
                    c.sent_screen = None;
                    c.sent_stream = None;
                }
            }
            return Ok(reply);
        }
        // A program whose screen is its chat.
        let (lines, _) = self.screen(id)?;
        let text = screen_text(&lines);
        if let Some(conn) = follow {
            if let Some(c) = lock(&self.inner).conns.get_mut(&conn) {
                c.sent_total = 1;
                c.sent_generation = generation;
                c.sent_screen = Some(text.clone());
            }
        }
        Ok(json!({
            "t": "chat", "id": id, "start": 0, "total": 1, "generation": generation, "source": "screen",
            "items": [Item::Screen { text }],
        }))
    }

    /// The sessions whose conversation holds every word of `words`: this
    /// machine's from their logs read whole, another host's by that host,
    /// and a session whose screen is its chat by its screen. A host out of
    /// reach, or an older build that cannot search, is searched in the chat
    /// the hub holds of it.
    fn search(&self, words: &[String]) -> Vec<search::Hit> {
        let sessions = lock(&self.inner).state.as_ref().map(|s| s.sessions.clone()).unwrap_or_default();
        let (here, away): (Vec<_>, Vec<_>) =
            sessions.iter().cloned().partition(|s| s.host.is_none() && !self.links.is_shared());
        let mut found = lock(&self.search).search(&here, &self.dirs(), words);
        let mut hosts: Vec<Option<&str>> = away.iter().map(|s| s.host.as_deref()).collect();
        hosts.sort_unstable();
        hosts.dedup();
        for host in hosts {
            let list: Vec<&SessionInfo> = away.iter().filter(|s| s.host.as_deref() == host).collect();
            let theirs =
                list.iter().map(|s| SessionInfo { id: split_id(s.id).1, host: None, ..(*s).clone() }).collect();
            let req = HostReq::Search { sessions: theirs, words: words.to_vec() };
            match host.is_none_or(|name| self.host_online(name)).then(|| self.links.ask_source(host, &req, None)) {
                Some(Ok(HostResp::Search { found: got })) => {
                    let ours = |own: u64| list.iter().find(|s| split_id(s.id).1 == own).map(|s| s.id);
                    found.hits.extend(got.hits.into_iter().filter_map(|h| Some(search::Hit { id: ours(h.id)?, ..h })));
                    found.screen.extend(got.screen.into_iter().filter_map(ours));
                }
                _ => {
                    for s in list {
                        let hit = self.with_chat(s, |log| match log {
                            Some(log) if log.source == Source::Screen => Err(()),
                            Some(log) => Ok(search::hit(s.id, search::item_messages(&log.items), words)),
                            None => Ok(None),
                        });
                        match hit {
                            Ok(hit) => found.hits.extend(hit),
                            Err(()) => found.screen.push(s.id),
                        }
                    }
                }
            }
        }
        for id in found.screen {
            if !sessions.iter().any(|s| s.id == id && (s.alive || s.has_screen)) {
                continue;
            }
            if let Ok((lines, _)) = self.screen(id) {
                let text = screen_text(&lines);
                found.hits.extend(search::hit(id, std::iter::once((search::Role::Screen, text.as_str(), None)), words));
            }
        }
        found.hits
    }

    /// Start a session, on host `host` when named. A Codex model and any
    /// first message wait until it is ready, on a thread of their own; the
    /// reply names the session.
    fn start(
        self: &Arc<Self>,
        tool: Tool,
        model: Option<String>,
        effort: Option<String>,
        text: Option<String>,
        host: Option<String>,
    ) -> Result<Value> {
        if let Some(name) = host.as_deref().filter(|name| *name != "@here") {
            if !self.host_online(name) {
                let label = host_label(&lock(&self.inner).hosts, Some(name)).unwrap_or_default();
                bail!("{label} is not reachable now");
            }
        }
        let model = model.filter(|m| !m.trim().is_empty());
        let effort = effort.filter(|e| !e.trim().is_empty());
        if model.as_deref().is_some_and(|m| !service::valid_model_name(m)) {
            bail!("invalid model name");
        }
        if effort.as_deref().is_some_and(|e| !service::valid_effort(e)) {
            bail!("invalid effort");
        }
        let text = text.filter(|t| !t.trim().is_empty());
        if text.is_some() || tool == Tool::Codex {
            anyhow::ensure!(
                lock(&self.inner).service_version >= crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL,
                "Update needed: direct agent messages require Cepheus protocol 19; nothing was sent"
            );
        }
        if text.as_ref().is_some_and(|t| t.len() > SEND_MAX) {
            bail!("the message is too long");
        }
        let msg = ClientMsg::Create {
            tool,
            model: model.clone(),
            effort: effort.clone(),
            host,
            cwd: None,
            command: Vec::new(),
            name: None,
            request: None,
        };
        let marker = self.session_state_marker()?;
        let id = match self.request_alone(&msg, Want::Created, marker)? {
            ServerMsg::Created { id, .. } => id,
            ServerMsg::Notice { text, .. } => bail!("{text}"),
            _ => bail!("unexpected reply"),
        };
        self.finish_create(id, tool, model, effort, text, marker)?;
        Ok(json!({"id": id}))
    }

    /// Created can precede State. Do not let a phone open an unpublished route.
    pub(super) fn session_state_marker(&self) -> Result<(u64, u64)> {
        let inner = lock(&self.inner);
        anyhow::ensure!(inner.service_ready, "The Cepheus service is not ready; waiting for its session state.");
        Ok((inner.service_generation, inner.state_revision))
    }

    pub(super) fn wait_for_session_state(&self, id: u64, marker: (u64, u64)) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                let inner = lock(&self.inner);
                anyhow::ensure!(
                    inner.service_ready && inner.service_generation == marker.0,
                    "The Cepheus service connection changed; check Home before starting another session."
                );
                if inner.state_revision > marker.1
                    && inner
                        .state
                        .as_ref()
                        .is_some_and(|state| state.sessions.iter().any(|session| session.id == id && session.alive))
                {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                bail!("Session {id} was created, but its session state has not arrived; check Home before starting another.");
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    // ---- what changes without a request ----

    /// Send each waiting session's prompt, and that a prompt went.
    pub fn update_prompts(&self) {
        if self.require_service_ready().is_err() {
            return;
        }
        let waiting: Vec<u64> = match lock(&self.inner).state.as_ref() {
            Some(state) => {
                state.sessions.iter().filter(|s| s.alive && s.status == Status::NeedsYou).map(|s| s.id).collect()
            }
            None => return,
        };
        let mut found = HashMap::new();
        for id in &waiting {
            let Ok((lines, inputs)) = self.screen(*id) else { continue };
            if let Some(p) = prompt::parse(&lines) {
                found.insert(
                    *id,
                    json!({"t": "prompt", "id": id, "inputs": inputs, "fp": p.fingerprint(), "prompt": p}),
                );
            }
        }
        let mut inner = lock(&self.inner);
        let mut out = Vec::new();
        let gone: Vec<u64> = inner.prompts.keys().filter(|id| !found.contains_key(id)).copied().collect();
        for id in gone {
            inner.prompts.remove(&id);
            out.push(json!({"t": "prompt", "id": id, "prompt": null}).to_string());
        }
        for (id, value) in found {
            if inner.prompts.get(&id) != Some(&value) {
                out.push(value.to_string());
                inner.prompts.insert(id, value);
            }
        }
        for text in out {
            for c in inner.conns.values() {
                let _ = c.tx.send(text.clone());
            }
        }
    }

    /// Send each phone what the chat it follows gained.
    pub fn followed(&self) -> Vec<u64> {
        if self.require_service_ready().is_err() {
            return Vec::new();
        }
        let inner = lock(&self.inner);
        let mut ids: Vec<_> = inner.conns.values().filter_map(|c| c.open).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub(super) fn update_chat(
        &self,
        only: u64,
        snapshot: Option<(Option<String>, Vec<String>)>,
        replies: &mut super::live::ReplyState,
    ) {
        if self.require_service_ready().is_err() {
            return;
        }
        let context = self.session(only).map(|s| (s.tool, s.conversation));
        let open: Vec<_> = lock(&self.inner)
            .conns
            .iter()
            .filter_map(|(cid, c)| {
                c.open
                    .filter(|id| *id == only)
                    .map(|id| (*cid, id, c.sent_total, c.sent_generation, c.sent_screen.is_some()))
            })
            .collect();
        if open.is_empty() {
            return;
        }
        let mut by_session: HashMap<u64, Option<SessionView>> = HashMap::new();
        for (_, id, ..) in &open {
            if by_session.contains_key(id) {
                continue;
            }
            let view = self.session(*id).and_then(|info| {
                self.with_chat(&info, |log| {
                    log.map(|log| match log.source {
                        Source::Screen => SessionView::Screen(log.generation),
                        _ => {
                            let total = log.total();
                            let is_snapshot = matches!(log.source, Source::Snapshot(_));
                            // Copy only the suffix these phones lack. A stable
                            // conversation is not cloned every streaming tick.
                            let items_start = open
                                .iter()
                                .map(|(_, _, sent, generation, screen)| {
                                    if *screen || *generation != log.generation || *sent < log.dropped || *sent > total
                                    {
                                        if is_snapshot {
                                            log.dropped
                                        } else {
                                            total.saturating_sub(PAGE).max(log.dropped)
                                        }
                                    } else {
                                        *sent
                                    }
                                })
                                .min()
                                .unwrap_or(total);
                            SessionView::Log {
                                total,
                                dropped: log.dropped,
                                generation: log.generation,
                                items_start,
                                snapshot: is_snapshot,
                                items: log.items[items_start - log.dropped..].to_vec(),
                                questions: log.questions.clone(),
                                seen: log.seen.clone(),
                                stream: log.stream().or_else(|| {
                                    snapshot
                                        .as_ref()
                                        .filter(|(conversation, _)| *conversation == info.conversation)
                                        .and_then(|(_, lines)| {
                                            replies.reply(&info, lines, log.generation, log.dropped, &log.items)
                                        })
                                }),
                            }
                        }
                    })
                })
            });
            let view = match view {
                Some(SessionView::Screen(generation)) => match self.screen(*id) {
                    Ok((lines, _)) => Some(SessionView::ScreenText(screen_text(&lines), generation)),
                    Err(_) => None,
                },
                other => other,
            };
            by_session.insert(*id, view);
        }
        let mut inner = lock(&self.inner);
        // A slow host read can finish after /clear or reopen changed the session.
        let current = inner
            .state
            .as_ref()
            .and_then(|state| state.sessions.iter().find(|s| s.id == only))
            .map(|s| (s.tool, s.conversation.clone()));
        if current != context {
            return;
        }
        let submitted = inner.answered_questions.clone();
        let stale = inner.stale_questions.clone();
        for (cid, id, sent_total, sent_generation, had_screen) in open {
            let Some(Some(view)) = by_session.get(&id) else { continue };
            let Some(c) = inner.conns.get_mut(&cid) else { continue };
            if c.open != Some(id)
                || (c.sent_total, c.sent_generation, c.sent_screen.is_some())
                    != (sent_total, sent_generation, had_screen)
            {
                continue;
            }
            match view {
                SessionView::ScreenText(text, generation) => {
                    if c.sent_screen.as_ref() != Some(text) || c.sent_generation != *generation {
                        c.sent_screen = Some(text.clone());
                        c.sent_generation = *generation;
                        c.sent_total = 1;
                        let msg = json!({"t": "items", "id": id, "reset": true, "start": 0, "total": 1,
                            "generation": generation,
                            "items": [Item::Screen { text: text.clone() }]});
                        let _ = c.tx.send(msg.to_string());
                    }
                }
                SessionView::Log {
                    total,
                    dropped,
                    generation,
                    items_start,
                    items,
                    questions,
                    seen,
                    stream,
                    snapshot,
                } => {
                    let reset = c.sent_screen.is_some()
                        || *generation != c.sent_generation
                        || c.sent_total < *dropped
                        || c.sent_total > *total;
                    let question_snapshot = phone_questions(questions, id, &submitted, &stale);
                    if !reset
                        && *total == c.sent_total
                        && *seen == c.sent_model
                        && *stream == c.sent_stream
                        && question_snapshot == c.sent_questions
                    {
                        continue;
                    }
                    let from = if reset && *snapshot {
                        *dropped
                    } else if reset {
                        total.saturating_sub(PAGE).max(*dropped)
                    } else {
                        c.sent_total
                    };
                    // An open/page reply can advance or reset a cursor while
                    // the host read is in flight. Retry from its new position.
                    if from < *items_start || from > *total {
                        continue;
                    }
                    let msg = json!({
                        "t": "items", "id": id, "reset": reset, "start": from, "total": total,
                        "generation": generation, "items": &items[from - items_start..], "retained_from": dropped,
                        "model": seen.model, "effort": seen.effort, "stream": stream,
                        "questions": &question_snapshot,
                    });
                    c.sent_questions = question_snapshot;
                    c.sent_total = *total;
                    c.sent_generation = *generation;
                    c.sent_model = seen.clone();
                    c.sent_stream = stream.clone();
                    c.sent_screen = None;
                    let _ = c.tx.send(msg.to_string());
                }
                SessionView::Screen(_) => {}
            }
        }
        let live: Vec<u64> =
            inner.state.as_ref().map(|s| s.sessions.iter().map(|s| s.id).collect()).unwrap_or_default();
        drop(inner);
        lock(&self.chats).forget_missing(&live);
    }

    /// The programs' models for the phone's pill, each default being what a
    /// start that picks nothing runs with: the deck's saved launch settings
    /// (`providers::launch`), else the program's own. A settings file that
    /// does not parse shows the programs' own; starting then says why.
    fn model_menu(&self) -> Vec<models::ToolModels> {
        let homes = self.model_homes();
        let mut tools = models::all(&homes);
        if let Some(path) = &self.opts.config_file {
            if Config::load_from(path).0.plugin_enabled(launch::PLUGIN) {
                if let Ok(settings) = launch::load(&launch::path(&homes.state)) {
                    launch::overlay(&mut tools, &settings);
                }
            }
        }
        tools
    }

    /// Where this machine's programs keep their model lists.
    fn model_homes(&self) -> models::Homes {
        models::Homes {
            claude: self.opts.claude_dir.clone(),
            codex: self.opts.codex_dir.clone(),
            antigravity: self.opts.antigravity_dir.clone(),
            ..models::Homes::here()
        }
    }

    /// Exit when the service should start a fresh gateway, drop phones
    /// removed in the deck's pairing screen, read this machine's use and
    /// send the phones the deck's new order when it moved a session.
    pub fn housekeeping(&self) {
        if self.opts.watch_exe && handover::replaced_binary().is_some() {
            eprintln!("cepheus remote: the program was replaced; exiting for the new build");
            std::process::exit(0);
        }
        if let Some(path) = &self.opts.config_file {
            let (config, _) = Config::load_from(path);
            if !config.remote
                || config.remote_listen != self.opts.listen
                || config.apk_listen != self.opts.apk_listen
                || parse_allow(&config.remote_allow) != self.opts.allow
                // A host installed or removed: the fresh gateway reaches it.
                || config.hosts != self.opts.hosts
                || self.links.profile_changed(path, &config)
            {
                eprintln!("cepheus remote: the config changed; exiting");
                std::process::exit(0);
            }
        }
        let stamp = store::devices_stamp(&self.opts.dir);
        let order_changed = {
            let mut inner = lock(&self.inner);
            if stamp != inner.devices_stamp {
                inner.adopt_devices(store::load_devices(&self.opts.dir), stamp);
            }
            if !self.links.is_shared() {
                let dir = self.disk_dir(&inner);
                inner.resources.sample(&dir);
            }
            util::file_stamp(&self.opts.order_file) != inner.order_stamp
        };
        // The deck moved a session (Super+↑/↓): the phones follow.
        if order_changed {
            self.push_order();
        }
        self.clean_uploads(false);
    }
}

/// The existing guarded question-menu fallback uses the hub's connection.
/// Owner text and model settings use their captured native targets separately.
impl codexpick::SessionIo for &Hub {
    fn screen(&mut self, id: u64) -> Result<(Vec<String>, u64)> {
        Hub::screen(self, id)
    }

    fn input(&mut self, id: u64, data: &str) -> Result<()> {
        Hub::input(self, id, data)
    }

    fn type_message(&mut self, id: u64, text: &str) -> Result<()> {
        Hub::type_message(self, id, text)
    }
}

enum SessionView {
    Screen(u64),
    ScreenText(String, u64),
    Log {
        total: usize,
        dropped: usize,
        generation: u64,
        items_start: usize,
        snapshot: bool,
        items: Vec<Item>,
        questions: Vec<chat::ChatQuestion>,
        seen: chat::ModelSeen,
        stream: Option<String>,
    },
}

/// A screen's text without the blank lines at its bottom.
fn screen_text(lines: &[String]) -> String {
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
    lines[..end].iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
}

/// The allowed addresses; anything unreadable allows nothing.
pub fn parse_allow(list: &[String]) -> Vec<IpAddr> {
    list.iter().filter_map(|a| a.trim().parse().ok()).collect()
}

#[cfg(test)]
#[path = "../tests/remote_requests.rs"]
mod request_tests;

#[cfg(test)]
#[path = "../tests/remote_clear_errors.rs"]
mod clear_error_tests;

#[cfg(test)]
#[path = "../tests/remote_state.rs"]
mod state_tests;

/// Submission is distinct from the agent's persisted answer receipt. A card
/// its native transcript refused (`stale`) is not offered again.
fn phone_questions(
    questions: &[chat::ChatQuestion],
    id: u64,
    submitted: &HashSet<(u64, String)>,
    stale: &HashSet<(u64, String)>,
) -> Vec<Value> {
    questions
        .iter()
        .filter(|question| !stale.contains(&(id, question.id.clone())))
        .map(|question| {
            let mut value = serde_json::to_value(question).expect("serializable question");
            value["submitted"] = json!(submitted.contains(&(id, question.id.clone())));
            value
        })
        .collect()
}

#[cfg(test)]
#[path = "../tests/remote_dot.rs"]
mod dot_tests;
