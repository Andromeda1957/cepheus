//! Native phone access to the deck's existing guarded Lua runtime.
//!
//! Lua stays on a single actor thread. The actor uses an ordinary control
//! connection, so scripts work with the terminal deck closed and cannot steal
//! its PTY attachment, focus, clipboard or keyboard.

use std::collections::HashMap;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::{App, Dialog, Sel};
use crate::config::Config;
use crate::lua::{ItemKind, JobDone, JobSink, PickKey, Places};
use crate::protocol::{read_msg, write_msg, ClientMsg, ClientRole, DeckState, HostInfo, ServerMsg, PROTOCOL_VERSION};

const MAX_REQUEST: usize = 32 * 1024;
const MAX_CODE: usize = 16 * 1024;
const MAX_ARGUMENTS: usize = 64;
const REPLY_WAIT: Duration = Duration::from_secs(10);
static NEXT_PICKER: AtomicU64 = AtomicU64::new(1);

enum Event {
    Request { state: Box<DeckState>, hosts: Vec<HostInfo>, args: Value, reply: mpsc::Sender<Result<Value>> },
    Server(Box<ServerMsg>),
    Lua(JobDone),
    Gone,
}

type Actors = Mutex<HashMap<PathBuf, mpsc::Sender<Event>>>;
static ACTORS: OnceLock<Actors> = OnceLock::new();

/// Run one authenticated phone's scripting operation. The gateway performs
/// authentication before calling this; this is never a public listener.
pub fn run(socket: &Path, state: DeckState, hosts: Vec<HostInfo>, args: Value) -> Result<Value> {
    if serde_json::to_vec(&args)?.len() > MAX_REQUEST {
        bail!("The scripting request is too large.");
    }
    let actors = ACTORS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = actors.lock().map_err(|_| anyhow::anyhow!("The script controller is unavailable."))?;
    let sender = registry
        .entry(socket.to_path_buf())
        .or_insert_with(|| {
            let (tx, rx) = mpsc::channel();
            let events = tx.clone();
            let socket = socket.to_path_buf();
            thread::spawn(move || actor(socket, events, rx));
            tx
        })
        .clone();
    drop(registry);
    let (reply, answer) = mpsc::channel();
    if sender.send(Event::Request { state: Box::new(state), hosts, args, reply }).is_err() {
        actors.lock().map_err(|_| anyhow::anyhow!("The script controller is unavailable."))?.remove(socket);
        bail!("The service connection changed. Open Scripting again to reconnect.");
    }
    match answer.recv_timeout(REPLY_WAIT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            actors.lock().map_err(|_| anyhow::anyhow!("The script controller is unavailable."))?.remove(socket);
            bail!("The scripting connection closed. Open Scripting again to reconnect.")
        }
        Err(mpsc::RecvTimeoutError::Timeout) => bail!("The script controller did not answer in time."),
    }
}

struct Scripts {
    app: App,
    output: mpsc::Receiver<ClientMsg>,
    /// Internal Lua callback and the opaque token issued to the phone.
    picker: Option<(u64, u64)>,
    created_selection: Option<u64>,
    retire: bool,
    /// The state as this connection was last sent it, which its patches
    /// apply to; requests also bring the gateway's state.
    service_state: Option<DeckState>,
}

impl Scripts {
    fn new(cfg: Config, places: Places, state: DeckState, hosts: Vec<HostInfo>, sink: JobSink, version: u32) -> Self {
        let (tx, output) = mpsc::channel();
        let enabled = cfg.lua;
        let mut app = App::new(tx, cfg);
        app.service_version = version;
        app.hosts = hosts;
        app.state = state;
        app.have_state = true;
        if enabled {
            app.load_lua(places, sink);
        }
        Self { app, output, picker: None, created_selection: None, retire: false, service_state: None }
    }

    fn perform(&mut self, args: &Value) -> Result<Value> {
        self.app.script_selection = None;
        self.created_selection = None;
        if self.app.lua.is_none() {
            bail!("Lua scripting is disabled or could not start. Check the deck configuration and Errors.");
        }
        if let Some(value) = args.get("id") {
            let id = value.as_u64().context("A session id must be a positive integer.")?;
            if self.app.session(id).is_none() {
                bail!("That session is no longer in the deck.");
            }
            self.app.select(Sel::Session(id));
        }
        let operation = match args.get("operation") {
            None => "list",
            Some(Value::String(operation)) => operation.as_str(),
            _ => bail!("The scripting operation must be text."),
        };
        match operation {
            "list" => {}
            "reload" => {
                if let Some(Dialog::Choose { callback, .. }) = self.app.dialog.take() {
                    if let Some(engine) = self.app.lua.as_mut() {
                        engine.forget(callback);
                    }
                }
                self.picker = None;
                self.app.reload_lua();
            }
            "eval" => {
                let code = text(args, "code", MAX_CODE)?;
                self.app.lua_eval(code);
            }
            "command" => {
                let name = text(args, "name", 32)?;
                let values = args.get("args").and_then(Value::as_array);
                if values.is_some_and(|a| a.len() > MAX_ARGUMENTS) {
                    bail!("A command takes at most {MAX_ARGUMENTS} arguments.");
                }
                let mut words = Vec::new();
                if let Some(values) = values {
                    for value in values {
                        let word = value.as_str().context("Each command argument must be text.")?;
                        if word.len() > 4096 || word.contains('\0') {
                            bail!("A command argument is too long or contains a null character.");
                        }
                        words.push(word.to_string());
                    }
                } else if args.get("args").is_some() {
                    bail!("Command arguments must be a list of text.");
                }
                let engine = self.app.lua.as_mut().context("Lua is unavailable.")?;
                if !engine.run_command(name.trim_start_matches(':'), &words) {
                    bail!("That script command is not registered. Refresh the command list.");
                }
                self.app.drain_lua();
                self.app.refresh_lua_ui();
            }
            "set" => {
                let name = text(args, "name", 80)?;
                let raw = args.get("value").context("An option value is required.")?;
                let value = serde_json::to_string(raw)?;
                if value.len() > 4096 || raw.as_str().is_some_and(|v| v.contains('\0')) {
                    bail!("The option value is too long or contains a null character.");
                }
                self.app
                    .lua
                    .as_mut()
                    .context("Lua is unavailable.")?
                    .set_option(name, &value)
                    .map_err(anyhow::Error::msg)?;
                self.app.refresh_lua_ui();
            }
            "choose" => {
                let token = args.get("callback").and_then(Value::as_u64).context("The picker has no callback.")?;
                let (callback, current_token) =
                    self.picker.context("That picker has closed. Run the command again.")?;
                if token != current_token {
                    bail!("That picker has changed. Run the command again.");
                }
                let Some(Dialog::Choose { callback: shown, pick, sel, .. }) = &self.app.dialog else {
                    bail!("That picker has closed. Run the command again.");
                };
                if *shown != callback {
                    bail!("That picker has changed. Run the command again.");
                }
                if args.get("cancel").and_then(Value::as_bool).unwrap_or(false) {
                    self.app.lua.as_mut().context("Lua is unavailable.")?.forget(callback);
                } else {
                    let index = usize::try_from(
                        args.get("index").and_then(Value::as_u64).context("Choose an item in the picker.")?,
                    )?;
                    // A page row after the keys turns to that page, as ← →
                    // do on the desktop; the picker stays open.
                    if let Some(page) = index.checked_sub(pick.items.len() + pick.keys.len()) {
                        let label = pick.tabs.get(page).context("That picker page no longer exists.")?.label.clone();
                        if args.get("item").and_then(Value::as_str).is_some_and(|expected| expected != label) {
                            bail!("That picker item has changed. Refresh the picker.");
                        }
                        if let Some(Dialog::Choose { pick, sel, .. }) = self.app.dialog.as_mut() {
                            pick.tab = page;
                            *sel = 0;
                        }
                        self.app.lua.as_mut().context("Lua is unavailable.")?.tab(callback, page, &label);
                        self.picker = None;
                        self.app.drain_lua();
                        self.app.refresh_lua_ui();
                        return Ok(self.snapshot());
                    }
                    // Keep the original row indices. Older phones can invoke the
                    // panel's keys through action rows appended to the same list.
                    let (item, key) = if let Some(item) = pick.items.get(index) {
                        (item.plain(), None)
                    } else {
                        let key =
                            pick.keys.get(index - pick.items.len()).context("That picker item no longer exists.")?;
                        (picker_key_label(key).to_string(), Some(key.key.clone()))
                    };
                    if let Some(expected) = args.get("item").and_then(Value::as_str) {
                        if expected != item {
                            bail!("That picker item has changed. Refresh the picker.");
                        }
                    }
                    let chosen = if key.is_some() {
                        pick.items
                            .get(*sel)
                            .filter(|item| item.kind == ItemKind::Choice)
                            .map(|item| (*sel, item.plain()))
                    } else {
                        Some((index, item))
                    };
                    // Consume first: a callback may open the next picker.
                    self.app.dialog = None;
                    self.app.lua.as_mut().context("Lua is unavailable.")?.choose(
                        callback,
                        chosen.as_ref().map(|(index, item)| (*index, item.as_str())),
                        key.as_deref(),
                    );
                }
                if args.get("cancel").and_then(Value::as_bool).unwrap_or(false) {
                    self.app.dialog = None;
                }
                self.picker = None;
                self.app.drain_lua();
                self.app.refresh_lua_ui();
            }
            _ => bail!("Unknown scripting operation."),
        }
        Ok(self.snapshot())
    }

    fn accept(&mut self, message: ServerMsg) {
        match message {
            ServerMsg::Clipboard { .. } | ServerMsg::Bell { .. } | ServerMsg::Frame { .. } => {}
            ServerMsg::Created { id, request } => {
                self.app.handle_server(ServerMsg::Created { id, request });
                self.created_selection = Some(id);
            }
            ServerMsg::State(state) => {
                self.service_state = Some(state.clone());
                self.app.handle_server(ServerMsg::State(state));
            }
            ServerMsg::StatePatch(patch) => {
                if let Some(state) = self.service_state.as_mut() {
                    state.apply(patch);
                    self.app.handle_server(ServerMsg::State(state.clone()));
                }
            }
            message => self.app.handle_server(message),
        }
    }

    fn snapshot(&mut self) -> Value {
        let picker = if let Some(Dialog::Choose { pick, callback, .. }) = &self.app.dialog {
            let token = match self.picker {
                Some((old, token)) if old == *callback => token,
                _ => {
                    let token = NEXT_PICKER.fetch_add(1, Ordering::Relaxed);
                    self.picker = Some((*callback, token));
                    token
                }
            };
            let items: Vec<String> = pick
                .items
                .iter()
                .map(crate::lua::PickItem::plain)
                .chain(pick.keys.iter().map(|key| picker_key_label(key).to_string()))
                .chain(pick.tabs.iter().map(|tab| tab.label.clone()))
                .collect();
            Some(json!({ "title": pick.title, "items": items, "callback": token }))
        } else {
            self.picker = None;
            None
        };
        let report = match &self.app.dialog {
            Some(Dialog::Report { title, lines, .. }) => Some(json!({ "title": title, "lines": lines })),
            // The phone shows `:machines` as a report of the table's text.
            Some(Dialog::Machines) => {
                let lines: Vec<String> = crate::view::machines_lines(&self.app, 48, &self.app.theme)
                    .iter()
                    .map(|line| {
                        line.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim_end().to_string()
                    })
                    .collect();
                Some(json!({ "title": "Machines", "lines": lines }))
            }
            Some(Dialog::Usage) => Some(json!({ "title": "Usage", "lines": crate::view::usage_text(&self.app) })),
            _ => None,
        };
        let commands: Vec<Value> = self
            .app
            .user_commands
            .iter()
            .map(|c| {
                json!({
                    "name": c.name, "desc": c.desc, "complete": c.complete,
                })
            })
            .collect();
        let mut messages: Vec<Value> = self
            .app
            .messages
            .iter()
            .rev()
            .take(20)
            .map(|m| json!({ "text": m.text, "error": false, "at": m.at }))
            .collect();
        messages.extend(
            self.app.local_errors.iter().rev().take(20).map(|m| json!({ "text": m.text, "error": true, "at": m.at })),
        );
        messages.sort_by_key(|m| m["at"].as_u64().unwrap_or_default());
        let selected = self.created_selection.or(self.app.script_selection);
        let selected_tool = selected.and_then(|id| self.app.session(id)).map(|s| s.tool.key());
        let engine = self.app.lua.as_ref();
        json!({
            "commands": commands,
            "options": engine.map(|e| e.options()).unwrap_or_default(),
            "loaded": engine.map(|e| e.loaded.clone()).unwrap_or_default(),
            "messages": messages,
            "picker": picker,
            "report": report,
            "selected": selected,
            "selected_tool": selected_tool,
        })
    }
}

// Keys and hints have already passed the Lua picker's validation.
fn picker_key_label(key: &PickKey) -> &str {
    if key.label.trim().is_empty() {
        &key.key
    } else {
        &key.label
    }
}

fn text<'a>(args: &'a Value, name: &str, max: usize) -> Result<&'a str> {
    let value = args.get(name).and_then(Value::as_str).with_context(|| format!("{name} must be text."))?;
    if value.trim().is_empty() || value.len() > max || value.contains('\0') {
        bail!("{name} must contain 1–{max} bytes of text without null characters.");
    }
    Ok(value)
}

fn actor(socket: PathBuf, events: mpsc::Sender<Event>, input: mpsc::Receiver<Event>) {
    let first = loop {
        match input.recv() {
            Ok(Event::Request { state, hosts, args, reply }) => break (state, hosts, args, reply),
            Ok(_) => {}
            Err(_) => return,
        }
    };
    let result = actor_connected(&socket, events, &input, first);
    if let Err(error) = result {
        // Jobs queued while the service closed receive an explicit failure.
        for event in input.try_iter() {
            if let Event::Request { reply, .. } = event {
                let _ = reply.send(Err(anyhow::anyhow!("{error:#}")));
            }
        }
    }
}

type First = (Box<DeckState>, Vec<HostInfo>, Value, mpsc::Sender<Result<Value>>);

fn actor_connected(
    socket: &Path,
    events: mpsc::Sender<Event>,
    input: &mpsc::Receiver<Event>,
    first: First,
) -> Result<()> {
    let (state, hosts, args, reply) = first;
    let opened = (|| -> Result<_> {
        let mut stream =
            crate::client::connect_public_socket(socket).context("Connecting the script controller to the service")?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        write_msg(&mut stream, &ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control })?;
        let welcome: ServerMsg = read_msg(&mut stream)?.context("The service closed before welcoming scripting.")?;
        let ServerMsg::Welcome { version, .. } = welcome else {
            bail!("The service did not welcome the script controller.");
        };
        if version < 9 {
            bail!("Update the deck service to use phone scripting.");
        }
        stream.set_read_timeout(None)?;
        let reader = stream.try_clone()?;
        let tx = events.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            while let Ok(Some(message)) = read_msg::<_, ServerMsg>(&mut reader) {
                if tx.send(Event::Server(Box::new(message))).is_err() {
                    return;
                }
            }
            let _ = tx.send(Event::Gone);
        });
        let (cfg, error) = Config::load();
        if let Some(error) = error {
            bail!("The deck configuration could not load: {error}");
        }
        let places = Places { config_dir: crate::paths::config_dir(), start_dir: state.start_dir.clone().into() };
        let sink: JobSink = std::sync::Arc::new(move |done| {
            let _ = events.send(Event::Lua(done));
        });
        let mut scripts = Scripts::new(cfg, places, *state, hosts, sink, version);
        scripts.app.use_order_file(crate::paths::order_file());
        scripts.app.lua_state_changed(false);
        Ok((stream, scripts))
    })();
    let (mut stream, mut scripts) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let reason = format!("{error:#}");
            let _ = reply.send(Err(error));
            bail!("{reason}");
        }
    };
    let mut deferred = std::collections::VecDeque::new();
    let _ = reply.send(perform_and_flush(&mut scripts, &mut stream, &args, input, &mut deferred));
    if scripts.retire {
        bail!("The script control connection was retired after an incomplete session start.");
    }
    loop {
        let event = deferred.pop_front().map(Ok).unwrap_or_else(|| input.recv_timeout(Duration::from_secs(1)));
        match event {
            Ok(Event::Request { state, hosts, args, reply }) => {
                scripts.app.hosts = hosts;
                scripts.app.handle_server(ServerMsg::State(*state));
                let _ = reply.send(perform_and_flush(&mut scripts, &mut stream, &args, input, &mut deferred));
                if scripts.retire {
                    bail!("The script control connection was retired after an incomplete session start.");
                }
            }
            Ok(Event::Server(message)) => scripts.accept(*message),
            Ok(Event::Lua(done)) => scripts.app.lua_job_done(done),
            Ok(Event::Gone) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        scripts.app.watch_order();
        scripts.app.lua_tick(Instant::now());
        flush(&mut scripts, &mut stream)?;
    }
    Ok(())
}

/// Wait for sessions this operation creates before reporting their ids. Other
/// phones' queued requests stay queued and every service event still reaches Lua.
fn perform_and_flush(
    scripts: &mut Scripts,
    stream: &mut UnixStream,
    args: &Value,
    input: &mpsc::Receiver<Event>,
    deferred: &mut std::collections::VecDeque<Event>,
) -> Result<Value> {
    if scripts.retire {
        bail!(
            "The previous session start did not finish. Reopen Scripting to reconnect before running another command."
        );
    }
    let result = scripts.perform(args);
    let mut creating = match flush(scripts, stream) {
        Ok(count) => count,
        Err(error) => {
            scripts.retire = true;
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Err(error);
        }
    };
    let started = creating > 0;
    let completed = (|| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while creating > 0 || scripts.created_selection.is_some_and(|id| scripts.app.session(id).is_none()) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "A script requested a session, but it did not start in time. Check Local agents before trying again."
                );
            }
            match input.recv_timeout(remaining) {
                Ok(Event::Server(message)) => {
                    if matches!(*message, ServerMsg::Created { .. }) {
                        creating = creating.saturating_sub(1);
                    }
                    if let ServerMsg::Notice { error: true, ref text } = *message {
                        return Err(anyhow::anyhow!("{}", text));
                    }
                    scripts.accept(*message);
                }
                Ok(Event::Lua(done)) => scripts.app.lua_job_done(done),
                Ok(Event::Gone) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("The scripting service connection closed.")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!("A script requested a session, but it did not start in time. Check Local agents before trying again.")
                }
                Ok(request @ Event::Request { .. }) => deferred.push_back(request),
            }
            creating += flush(scripts, stream)?;
        }
        result.map(|_| scripts.snapshot())
    })();
    if started && completed.is_err() {
        // Created has no request id. A late reply on this socket could otherwise
        // answer the next command; retire the socket and reconnect on demand.
        scripts.retire = true;
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    completed
}

fn flush(scripts: &mut Scripts, stream: &mut UnixStream) -> Result<usize> {
    let mut creating = 0;
    for message in scripts.output.try_iter() {
        // The phone runtime never takes over a PTY view or desktop focus.
        if matches!(message, ClientMsg::Attach { .. } | ClientMsg::Resize { .. } | ClientMsg::Focus { .. }) {
            continue;
        }
        if matches!(
            message,
            ClientMsg::Create { .. }
                | ClientMsg::Reopen { .. }
                | ClientMsg::Resume { .. }
                | ClientMsg::MoveIn { .. }
                | ClientMsg::SecurityScan { .. }
        ) {
            creating += 1;
        }
        write_msg(stream, &message)?;
    }
    Ok(creating)
}

#[cfg(test)]
#[path = "../tests/phone_scripts.rs"]
mod tests;
