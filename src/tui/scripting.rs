//! The deck's side of the Lua API (`lua.rs`): loading the scripts, doing
//! what they ask once each call returns, telling them what happens, and
//! keeping their sections, badges and phone panels current.

use std::rc::Rc;
use std::time::Instant;

use crossterm::event::KeyEvent;

use super::{App, Dialog, Mode, Sel};
use crate::hotkeys::Chord;
use crate::lua::{self, Engine, JobDone, KeyAction, Places, Request};
use crate::protocol::{clean_panels, ClientMsg, DeckState, HostInfo, Tool};

/// How deep scripts may run commands that run scripts.
const MAX_DEPTH: u8 = 8;

use crate::lua::changes;

/// What the view's scripts read with `deck.state()`: the service's state,
/// the hosts, the session order, the highlighted session and the mode.
/// Fields become JSON only when a script asks for them.
pub struct ViewState {
    deck: Rc<DeckState>,
    hosts: Rc<Vec<HostInfo>>,
    order: Vec<u64>,
    current: Option<u64>,
    mode: &'static str,
}

impl lua::StateSource for ViewState {
    fn field(&self, name: &str) -> Option<serde_json::Value> {
        match name {
            "hosts" => serde_json::to_value(&*self.hosts).ok(),
            "host_order" => Some(serde_json::json!(self.hosts.iter().map(|h| &h.name).collect::<Vec<_>>())),
            "order" => Some(serde_json::json!(self.order)),
            "current" => Some(serde_json::json!(self.current)),
            "mode" => Some(self.mode.into()),
            _ => lua::pick(&*self.deck, name),
        }
    }

    fn whole(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut fields = match serde_json::to_value(&*self.deck) {
            Ok(serde_json::Value::Object(fields)) => fields,
            _ => serde_json::Map::new(),
        };
        for name in ["hosts", "host_order", "order", "current", "mode"] {
            if let Some(value) = self.field(name) {
                fields.insert(name.into(), value);
            }
        }
        fields
    }
}

impl App {
    /// Start the scripts (`init.lua`, the plugins, the bundled ones) once
    /// the service has said where it starts sessions: the bundled plugins
    /// look there.
    pub fn load_lua(&mut self, places: Places, sink: lua::JobSink) {
        self.lua_setup = Some((places, sink));
        self.lua_waiting = true;
        if !self.state.start_dir.is_empty() {
            self.lua_state_changed(true);
        }
    }

    /// `:reload`: a fresh Lua with every script loaded again.
    pub fn reload_lua(&mut self) {
        self.watch_order();
        let Some((places, sink)) = self.lua_setup.clone() else {
            self.set_notice("Lua is off (\"lua\": false in the config).", true);
            return;
        };
        self.lua = None;
        self.lua_keys.clear();
        self.user_commands.clear();
        self.lua_sections.clear();
        self.lua_machines.clear();
        self.lua_badges.clear();
        let mut engine = match Engine::new(places, sink) {
            Ok(engine) => engine,
            Err(e) => {
                self.local_error(format!("Lua did not start: {e}"));
                return;
            }
        };
        engine.use_source_dir(self.cfg.plugin_source_dir.clone());
        self.lua_view = None;
        let (view, _) = self.view_state();
        engine.set_state_source(view, None);
        engine.load_with(&self.cfg.plugins);
        self.lua = Some(engine);
        self.drain_lua();
        self.refresh_lua_ui();
        // A reload is quiet; listing every script filled the footer. Failures still report.
        self.dirty = true;
    }

    /// The deck's state as `deck.state()` gives it whole: the service's
    /// state, the hosts, the highlighted session and the mode.
    #[cfg(test)]
    pub fn lua_state(&self) -> serde_json::Value {
        let (order, current, mode) = self.view_parts();
        let view =
            ViewState { deck: Rc::new(self.state.clone()), hosts: Rc::new(self.hosts.clone()), order, current, mode };
        serde_json::Value::Object(lua::StateSource::whole(&view))
    }

    /// What the view adds to the service's state: the session order, the
    /// highlighted session and the mode.
    fn view_parts(&self) -> (Vec<u64>, Option<u64>, &'static str) {
        let order: Vec<u64> = self.ordered_sessions().iter().map(|s| s.id).collect();
        let current = match self.current {
            Some(Sel::Session(id)) => Some(id),
            _ => None,
        };
        let mode = match self.mode {
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Command => "command",
        };
        (order, current, mode)
    }

    /// The view's state for its scripts as it is now, and the fields that
    /// changed since the scripts were last given it (none: all of them).
    /// Parts that did not change are shared with that last one, not copied.
    /// Only for giving the scripts their state: it becomes the last one.
    fn view_state(&mut self) -> (Rc<ViewState>, Option<Vec<&'static str>>) {
        let (order, current, mode) = self.view_parts();
        let Some(last) = self.lua_view.clone() else {
            let view = Rc::new(ViewState {
                deck: Rc::new(self.state.clone()),
                hosts: Rc::new(self.hosts.clone()),
                order,
                current,
                mode,
            });
            self.lua_view = Some(view.clone());
            return (view, None);
        };
        let mut changed = Vec::new();
        let deck = if *last.deck == self.state {
            last.deck.clone()
        } else {
            changed.extend(self.state.changed(&last.deck).into_iter().filter(|name| *name != "hosts"));
            Rc::new(self.state.clone())
        };
        let hosts = if *last.hosts == self.hosts {
            last.hosts.clone()
        } else {
            changed.extend(["hosts", "host_order"]);
            Rc::new(self.hosts.clone())
        };
        if last.order != order {
            changed.push("order");
        }
        if last.current != current {
            changed.push("current");
        }
        if last.mode != mode {
            changed.push("mode");
        }
        if changed.is_empty() {
            return (last, Some(changed));
        }
        let view = Rc::new(ViewState { deck, hosts, order, current, mode });
        self.lua_view = Some(view.clone());
        (view, Some(changed))
    }

    /// Give the scripts the view's state as it is now.
    fn give_lua_state(&mut self) {
        if self.lua.is_none() {
            return;
        }
        let (view, changed) = self.view_state();
        if let Some(engine) = self.lua.as_mut() {
            engine.set_state_source(view, changed.as_deref());
        }
    }

    /// A new state arrived: tell the scripts and draw what they show again.
    pub fn lua_state_changed(&mut self, first: bool) {
        self.watch_order();
        if self.lua_waiting {
            self.lua_waiting = false;
            if let Some((places, _)) = self.lua_setup.as_mut().filter(|_| !self.state.start_dir.is_empty()) {
                places.start_dir = self.state.start_dir.clone().into();
            }
            self.lua_seen = lua::seen(&self.state.sessions);
            self.reload_lua();
            return;
        }
        if self.lua.is_none() {
            return;
        }
        self.give_lua_state();
        let events = if first { Vec::new() } else { changes(&self.lua_seen, &self.state.sessions) };
        self.lua_seen = crate::lua::seen(&self.state.sessions);
        if let Some(engine) = self.lua.as_mut() {
            for change in &events {
                engine.emit_json(change.event, &change.payload);
            }
            engine.emit_json("state", &serde_json::Value::Null);
        }
        self.drain_lua();
        self.refresh_lua_ui();
    }

    /// Read the sections and badges again and send the phone its panels
    /// when they changed.
    pub fn refresh_lua_ui(&mut self) {
        let Some(engine) = self.lua.as_mut() else { return };
        let sections = engine.sections();
        let badges_on = !matches!(engine.option("badges"), Some(serde_json::Value::Bool(false)));
        let sessions: Vec<serde_json::Value> = if badges_on && engine.has_badges() {
            self.state.sessions.iter().filter_map(|s| serde_json::to_value(s).ok()).collect()
        } else {
            Vec::new()
        };
        let badges = engine.badges(&sessions);
        let order = engine.option("sidebar").and_then(|v| serde_json::from_value::<Vec<String>>(v).ok());
        let hidden = engine
            .option("sidebar_hidden")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default();
        self.lua_sections = sections;
        self.lua_machines = engine.machines();
        self.lua_badges = badges;
        self.sidebar_order = order;
        self.sidebar_hidden = hidden;
        self.lua_keys = engine.keymaps();
        self.user_commands = engine.commands();
        self.drain_lua();
        self.dirty = true;
        let panels = clean_panels(self.lua_sections.iter().filter(|s| s.phone).map(lua::Section::panel).collect());
        if panels != self.sent_panels && self.service_version >= 9 {
            self.send(ClientMsg::Panels { panels: panels.clone() });
            self.sent_panels = panels;
        }
    }

    /// Do what the scripts asked and keep their errors for F2.
    pub fn drain_lua(&mut self) {
        if self.lua_depth >= MAX_DEPTH {
            self.local_error("Lua: scripts ran commands that ran scripts too deep; stopped there");
            return;
        }
        self.lua_depth += 1;
        while let Some(engine) = self.lua.as_mut() {
            let errors = engine.take_errors();
            let requests = engine.take_requests();
            if errors.is_empty() && requests.is_empty() {
                break;
            }
            for e in errors {
                self.local_error(e);
            }
            for request in requests {
                self.do_request(request);
            }
        }
        self.lua_depth -= 1;
    }

    fn do_request(&mut self, request: Request) {
        match request {
            Request::Reorder(ids) => {
                if ids.iter().any(|id| self.session(*id).is_none_or(|s| !s.alive)) {
                    return self.local_error("Lua: session order contains an unknown session");
                }
                let mut order = ids;
                for session in self.ordered_sessions() {
                    if !order.contains(&session.id) {
                        order.push(session.id);
                    }
                }
                if self.order != order {
                    self.order = order;
                    self.save_order();
                }
            }
            Request::ServiceCall { plugin, command, args, callback } => {
                if self.service_version < 11 {
                    self.local_error("Service plugins need an updated service");
                } else {
                    self.send(ClientMsg::Plugin { request: callback.unwrap_or(0), plugin, command, args });
                }
            }
            Request::Annotate { .. } | Request::Title { .. } | Request::Process { .. } => {
                self.local_error("This operation belongs to a service plugin")
            }
            Request::Command(line) => self.run_command(&line),
            Request::Notify { text, error } => self.set_notice(text, error),
            Request::Pick { pick, callback } => {
                if pick.items.is_empty() {
                    self.set_notice(format!("{}: nothing to pick.", pick.title), false);
                    if let Some(engine) = self.lua.as_mut() {
                        engine.forget(callback);
                    }
                } else {
                    // A refresh that names no row keeps the one the reader
                    // is on, as figures arrive under it.
                    let kept = match &self.dialog {
                        Some(Dialog::Choose { pick: old, sel, filter, .. })
                            if old.title == pick.title && old.tab == pick.tab && pick.selected == 0 =>
                        {
                            super::choose_matches(&old.items, filter).get(*sel).copied()
                        }
                        _ => None,
                    };
                    // A picker replacing another drops the old one's
                    // functions; it was not closed, so on_close stays quiet.
                    if let Some(Dialog::Choose { callback: old, .. }) = &self.dialog {
                        let old = *old;
                        if let Some(engine) = self.lua.as_mut() {
                            engine.discard(old);
                        }
                    }
                    // A background refresh replaces the visible panel in place;
                    // a choice callback opens a fresh one after dialog_key takes it.
                    // Another page of the same window starts at its top.
                    let refreshing = matches!(&self.dialog, Some(Dialog::Choose { pick: old, .. })
                        if old.title == pick.title && old.tab == pick.tab);
                    if !refreshing {
                        self.dialog_viewport = super::DialogViewport::default();
                    }
                    let filter = self.dialog_viewport.search.clone().unwrap_or_default();
                    let matches = super::choose_matches(&pick.items, &filter);
                    let wanted = kept.filter(|&i| i < pick.items.len()).unwrap_or(pick.selected);
                    let selected = matches.iter().position(|&i| i == wanted).unwrap_or(0);
                    let sel = super::choose_snap(&pick.items, &matches, selected, true);
                    self.dialog = Some(Dialog::Choose { pick, sel, filter, callback });
                }
            }
            Request::Fleet { action, args, callback } => {
                let available = if action.starts_with("attention-") {
                    self.attention_gate()
                } else {
                    self.service_has(14, "Fleet scheduling")
                };
                if available {
                    self.send(ClientMsg::Fleet { request: callback, action, args });
                }
            }
            Request::Create { tool, host, cwd, name, command } => {
                let Some(tool) = Tool::parse(&tool) else {
                    return self.local_error(format!("Lua: {tool:?} is not a program the deck starts"));
                };
                let host = match host.as_deref().map(|h| self.resolve_host(h)) {
                    Some(Ok(host)) => host,
                    Some(Err(why)) => return self.local_error(format!("Lua: {why}")),
                    None => None,
                };
                if tool != Tool::Shell && !command.is_empty() {
                    return self.local_error("Lua: only a terminal takes a command");
                }
                self.start_session(tool, host, cwd, name, command, None);
            }
            Request::Resume { tool, conversation, host } => {
                let Some(tool) = Tool::parse(&tool).filter(|tool| tool.is_agent()) else {
                    return self.local_error("Lua: resume needs an agent program");
                };
                let host = match host.as_deref().map(|h| self.resolve_host(h)) {
                    Some(Ok(host)) => host,
                    Some(Err(why)) => return self.local_error(format!("Lua: {why}")),
                    None => None,
                };
                self.send(ClientMsg::Resume { tool, conversation, host });
            }
            Request::Send { id, text } => {
                if self.session(id).is_some_and(|s| s.alive) {
                    if let Err(error) = self.send_owner_message(id, &text) {
                        self.local_error(format!("Lua: {error}"));
                    }
                } else {
                    self.local_error(format!("Lua: session {id} is not running"));
                }
            }
            Request::Select { id } => {
                if self.session(id).is_some() {
                    self.script_selection = Some(id);
                    self.select(Sel::Session(id));
                }
            }
        }
    }

    pub fn lua_service_result(&mut self, request: u64, data: serde_json::Value, error: Option<String>) {
        if let Some(engine) = self.lua.as_mut() {
            engine.service_result(request, &data, error.as_deref());
        }
        self.drain_lua();
        self.refresh_lua_ui();
    }

    /// `:lua code`.
    pub fn lua_eval(&mut self, code: &str) {
        self.watch_order();
        self.give_lua_state();
        let Some(engine) = self.lua.as_mut() else {
            return self.set_notice("Lua is off (\"lua\": false in the config).", true);
        };
        if code.trim().is_empty() {
            return self.set_notice(":lua runs Lua; :lua =expr shows a value.", false);
        }
        let shown = engine.eval(code);
        self.drain_lua();
        if let Some(text) = shown {
            self.set_notice(text, false);
        }
        self.refresh_lua_ui();
    }

    /// A script's command, if one has that name; whether one did.
    pub fn run_user_command(&mut self, name: &str, rest: &str) -> bool {
        self.watch_order();
        self.give_lua_state();
        let Some(engine) = self.lua.as_mut() else { return false };
        let words = crate::actions::words(rest).unwrap_or_else(|_| rest.split_whitespace().map(String::from).collect());
        if !engine.run_command(name, &words) {
            return false;
        }
        self.drain_lua();
        self.refresh_lua_ui();
        true
    }

    /// A key a script mapped; whether it was one. A plain character stays
    /// text while typing into a session or on the command line.
    pub fn lua_key(&mut self, k: &KeyEvent) -> bool {
        let chord = Chord::of(k);
        if chord.types_text() && matches!(self.mode, Mode::Insert | Mode::Command) {
            return false;
        }
        let Some(map) = self.lua_keys.iter().find(|m| m.chord == chord).cloned() else { return false };
        match map.action {
            KeyAction::Command(line) => self.run_command(&line),
            KeyAction::Lua(callback) => {
                if let Some(engine) = self.lua.as_mut() {
                    engine.press(callback);
                }
                self.drain_lua();
                self.refresh_lua_ui();
            }
        }
        true
    }

    /// The scripts' timers that are due.
    pub fn lua_tick(&mut self, now: Instant) {
        let ran = self.lua.as_mut().is_some_and(|engine| engine.tick(now));
        if ran {
            self.drain_lua();
            self.refresh_lua_ui();
        }
    }

    /// When the next timer is due.
    pub fn lua_next_due(&self) -> Option<Instant> {
        self.lua.as_ref().and_then(Engine::next_due)
    }

    pub fn lua_job_done(&mut self, done: JobDone) {
        if let Some(engine) = self.lua.as_mut() {
            engine.job_done(done);
        }
        self.drain_lua();
        self.refresh_lua_ui();
    }
}
