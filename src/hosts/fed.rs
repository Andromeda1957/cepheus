//! How the hub turns a host's messages into its own and back: every id that
//! names a session or an outside process gains the host's tag on the way in
//! and loses it on the way out (`protocol::host_id`), and what is merged
//! (usage, errors) is merged here.

use crate::protocol::{
    host_id, host_pid, split_id, split_pid, Alert, ClientMsg, Conversation, DeckState, LogEntry, OutsideInfo,
    ServerMsg, SessionInfo, Usage, UsageWindow,
};

/// Where a deck's message goes.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// The hub's own service handles it.
    Local(ClientMsg),
    /// The host tagged so, the message rewritten to that host's own ids.
    Host(u16, ClientMsg),
    /// It names a host this hub does not know.
    UnknownHost(String),
}

/// Where `msg` goes; `tag_of` finds a host's tag by its name.
pub fn route(msg: ClientMsg, tag_of: impl Fn(&str) -> Option<u16>) -> Route {
    let by_id = |id: u64, make: &dyn Fn(u64) -> ClientMsg| match split_id(id) {
        (0, _) => Route::Local(make(id)),
        (tag, local) => Route::Host(tag, make(local)),
    };
    let by_host = |host: Option<String>, msg: ClientMsg| match host {
        None => Route::Local(msg),
        Some(name) => match tag_of(&name) {
            Some(tag) => Route::Host(tag, msg),
            None => Route::UnknownHost(name),
        },
    };
    match msg {
        ClientMsg::Attach { id: Some(id), cols, rows } => {
            by_id(id, &|id| ClientMsg::Attach { id: Some(id), cols, rows })
        }
        ClientMsg::ResizeTerminal { id, cols, rows } => by_id(id, &|id| ClientMsg::ResizeTerminal { id, cols, rows }),
        ClientMsg::Input { id, data } => by_id(id, &|id| ClientMsg::Input { id, data: data.clone() }),
        ClientMsg::Scroll { id, op } => by_id(id, &|id| ClientMsg::Scroll { id, op }),
        ClientMsg::PasteFile { id, name, data } => {
            by_id(id, &|id| ClientMsg::PasteFile { id, name: name.clone(), data: data.clone() })
        }
        ClientMsg::Close { id } => by_id(id, &|id| ClientMsg::Close { id }),
        ClientMsg::Reopen { id } => by_id(id, &|id| ClientMsg::Reopen { id }),
        ClientMsg::ChangeDirectory { id, path } => {
            by_id(id, &|id| ClientMsg::ChangeDirectory { id, path: path.clone() })
        }
        ClientMsg::Forget { id } => by_id(id, &|id| ClientMsg::Forget { id }),
        ClientMsg::Screen { id, tail } => by_id(id, &|id| ClientMsg::Screen { id, tail }),
        ClientMsg::History { id, request, epoch, cols, rows } => {
            by_id(id, &|id| ClientMsg::History { id, request, epoch, cols, rows })
        }
        ClientMsg::SendMessage { id, request, text, attachments } => {
            by_id(id, &|id| ClientMsg::SendMessage { id, request, text: text.clone(), attachments })
        }
        ClientMsg::SendNow { id } => by_id(id, &|id| ClientMsg::SendNow { id }),
        ClientMsg::Stop { id } => by_id(id, &|id| ClientMsg::Stop { id }),
        ClientMsg::Rename { id, name } => by_id(id, &|id| ClientMsg::Rename { id, name: name.clone() }),
        ClientMsg::Answer { id, inputs, prompt, choice, choices, text } => by_id(id, &|id| ClientMsg::Answer {
            id,
            inputs,
            prompt: prompt.clone(),
            choice,
            choices: choices.clone(),
            text: text.clone(),
        }),
        ClientMsg::Fleet { request, action, mut args } => {
            let host = args.get("host").and_then(|v| v.as_str()).map(str::to_string);
            if let Some(obj) = args.as_object_mut() {
                obj.remove("host");
            }
            by_host(host, ClientMsg::Fleet { request, action, args })
        }
        ClientMsg::Plugin { request, plugin, command, mut args } => {
            let host = match args.get("host") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(host)) => Some(host.clone()),
                _ => return Route::UnknownHost("invalid host argument".into()),
            };
            if let Some(obj) = args.as_object_mut() {
                obj.remove("host");
            }
            by_host(host, ClientMsg::Plugin { request, plugin, command, args })
        }
        ClientMsg::Create { tool, model, effort, host, cwd, command, name, request } => {
            by_host(host, ClientMsg::Create { tool, model, effort, host: None, cwd, command, name, request })
        }
        ClientMsg::Install { request, host } => by_host(host, ClientMsg::Install { request, host: None }),
        ClientMsg::SecurityScan { host, scope } => by_host(host, ClientMsg::SecurityScan { host: None, scope }),
        ClientMsg::Resume { tool, conversation, host } => {
            by_host(host, ClientMsg::Resume { tool, conversation, host: None })
        }
        ClientMsg::MoveIn { tool, pid, conversation } => match split_pid(pid) {
            (0, _) => Route::Local(ClientMsg::MoveIn { tool, pid, conversation }),
            (tag, local) => Route::Host(tag, ClientMsg::MoveIn { tool, pid: local, conversation }),
        },
        other => Route::Local(other),
    }
}

/// Split a broadcast's sessions by host: the hub's own first (tag 0), each
/// with the ids its host knows.
pub fn split_ids(ids: &[u64]) -> Vec<(u16, Vec<u64>)> {
    let mut out: Vec<(u16, Vec<u64>)> = Vec::new();
    for &id in ids {
        let (tag, local) = split_id(id);
        let local = if tag == 0 { id } else { local };
        match out.iter_mut().find(|(t, _)| *t == tag) {
            Some((_, list)) => list.push(local),
            None => out.push((tag, vec![local])),
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out
}

pub fn session_from(tag: u16, host: &str, s: SessionInfo) -> SessionInfo {
    SessionInfo { id: host_id(tag, s.id), host: Some(host.to_string()), ..s }
}

/// An outside process on a host: its kitty window is not one the hub can
/// focus, so that is left out.
pub fn outside_from(tag: u16, host: &str, o: OutsideInfo) -> OutsideInfo {
    OutsideInfo {
        pid: host_pid(tag, o.pid),
        host: Some(host.to_string()),
        kitty_pid: None,
        kitty_window: None,
        kitty_socket: false,
        ..o
    }
}

pub fn conversation_from(tag: u16, host: &str, c: Conversation) -> Conversation {
    Conversation { open_in: c.open_in.map(|id| host_id(tag, id)), host: Some(host.to_string()), ..c }
}

/// Project every identifier in the public attention snapshot into this hub's domain.
pub fn attention_from(tag: u16, host: &str, attention: &mut crate::attention::Snapshot) {
    for message in &mut attention.messages {
        message.session = host_id(tag, message.session);
        message.host = Some(host.into());
    }
    for audit in &mut attention.supervisor.audit {
        audit.session = host_id(tag, audit.session);
    }
    attention.queued =
        std::mem::take(&mut attention.queued).into_iter().map(|(id, count)| (host_id(tag, id), count)).collect();
}

fn project_field(tag: u16, value: &mut serde_json::Value, field: &str) {
    if let Some(id) = value[field].as_u64() {
        value[field] = serde_json::json!(host_id(tag, id));
    }
}
fn project_rows(tag: u16, value: &mut serde_json::Value, field: &str) {
    if let Some(rows) = value.as_array_mut() {
        for row in rows {
            project_field(tag, row, field);
        }
    }
}
fn project_recipients(tag: u16, value: &mut serde_json::Value) {
    if let Some(rows) = value.as_array_mut() {
        for row in rows {
            if let Some(id) = row.as_u64() {
                *row = serde_json::json!(host_id(tag, id));
            } else {
                project_field(tag, row, "session");
            }
        }
    }
}
fn project_supervisor(tag: u16, value: &mut serde_json::Value) {
    if let Some(audit) = value.get_mut("audit") {
        project_rows(tag, audit, "session");
    }
}
fn attention_value_from(tag: u16, host: &str, value: &mut serde_json::Value) {
    if let Some(rows) = value.get_mut("messages") {
        project_rows(tag, rows, "session");
    }
    if let Some(messages) = value.get_mut("messages").and_then(serde_json::Value::as_array_mut) {
        for message in messages {
            if let Some(object) = message.as_object_mut() {
                object.insert("host".into(), serde_json::json!(host));
            }
        }
    }
    if let Some(supervisor) = value.get_mut("supervisor") {
        project_supervisor(tag, supervisor);
    }
    if let Some(queued) = value.get_mut("queued").and_then(serde_json::Value::as_object_mut) {
        *queued = std::mem::take(queued)
            .into_iter()
            .map(|(id, count)| (id.parse::<u64>().map(|id| host_id(tag, id).to_string()).unwrap_or(id), count))
            .collect();
    }
}

/// A host's state with the hub's ids and the host's name on everything.
pub fn state_from(tag: u16, host: &str, mut state: DeckState) -> DeckState {
    for slot in state.scheduler.holders.iter_mut().chain(state.scheduler.waiters.iter_mut()) {
        slot.owner.session = slot.owner.session.map(|id| host_id(tag, id));
    }
    for start in &mut state.scheduler.starts {
        start.session = start.session.map(|id| host_id(tag, id));
    }
    for paused in &mut state.scheduler.paused {
        paused.session = host_id(tag, paused.session);
    }
    for rerun in &mut state.scheduler.reruns {
        rerun.session = host_id(tag, rerun.session);
    }
    attention_from(tag, host, &mut state.attention);
    state.host_schedulers.clear();
    DeckState {
        jobs: state
            .jobs
            .into_iter()
            .map(|mut j| {
                j.id = host_id(tag, j.id);
                j.host = Some(host.into());
                j
            })
            .collect(),
        providers: state
            .providers
            .into_iter()
            .map(|p| crate::providers::Info { host: Some(host.to_string()), ..p })
            .collect(),
        sessions: state.sessions.into_iter().map(|s| session_from(tag, host, s)).collect(),
        outside: state.outside.into_iter().map(|o| outside_from(tag, host, o)).collect(),
        scans: state
            .scans
            .into_iter()
            .map(|s| crate::protocol::ScanInfo {
                host: Some(host.to_string()),
                session: s.session.map(|id| host_id(tag, id)),
                ..s
            })
            .collect(),
        ..state
    }
}

/// A reply or notice from a host, as the hub's deck reads it. `label` is
/// how the deck names the host.
pub fn inbound(tag: u16, host: &str, label: &str, msg: ServerMsg) -> ServerMsg {
    match msg {
        ServerMsg::FleetResult { request, mut data, error } => {
            if let Some(id) = data["job"].as_u64() {
                data["job"] = serde_json::json!(host_id(tag, id));
            } else if let Some(id) = data["job"]["id"].as_u64() {
                data["job"]["id"] = serde_json::json!(host_id(tag, id));
                data["job"]["host"] = serde_json::json!(host);
            }
            if let Some(jobs) = data["jobs"].as_array_mut() {
                for job in jobs {
                    if let Some(id) = job["id"].as_u64() {
                        job["id"] = serde_json::json!(host_id(tag, id));
                        job["host"] = serde_json::json!(host);
                    }
                }
            }
            if let Some(attention) = data.get_mut("attention") {
                attention_value_from(tag, host, attention);
            }
            if let Some(supervisor) = data.get_mut("supervisor") {
                project_supervisor(tag, supervisor);
            }
            if let Some(message) = data.get_mut("message").filter(|message| message.is_object()) {
                project_field(tag, message, "session");
                message["host"] = serde_json::json!(host);
            }
            if let Some(messages) = data.get_mut("messages") {
                project_rows(tag, messages, "session");
                if let Some(rows) = messages.as_array_mut() {
                    for message in rows {
                        if let Some(object) = message.as_object_mut() {
                            object.insert("host".into(), serde_json::json!(host));
                        }
                    }
                }
            }
            for name in ["recipients", "targets"] {
                if let Some(rows) = data.get_mut(name) {
                    project_recipients(tag, rows);
                }
                if let Some(rows) = data.get_mut("decision").and_then(|d| d.get_mut(name)) {
                    project_recipients(tag, rows);
                }
            }
            ServerMsg::FleetResult { request, data, error }
        }
        ServerMsg::Created { id, request } => ServerMsg::Created { id: host_id(tag, id), request },
        ServerMsg::Frame { id, frame } => ServerMsg::Frame { id: host_id(tag, id), frame },
        ServerMsg::History { id, request, epoch, cols, rows, snapshot, error } => {
            ServerMsg::History { id: host_id(tag, id), request, epoch, cols, rows, snapshot, error }
        }
        ServerMsg::Bell { id } => ServerMsg::Bell { id: host_id(tag, id) },
        ServerMsg::ScreenText { id, lines, inputs } => ServerMsg::ScreenText { id: host_id(tag, id), lines, inputs },
        ServerMsg::Answered { id, error, stale } => ServerMsg::Answered { id: host_id(tag, id), error, stale },
        ServerMsg::MessageSent { id, request, error, delivered } => {
            ServerMsg::MessageSent { id: host_id(tag, id), request, error, delivered }
        }
        ServerMsg::SentNow { id, error } => ServerMsg::SentNow { id: host_id(tag, id), error },
        ServerMsg::DirectoryChanged { id, cwd, error } => {
            ServerMsg::DirectoryChanged { id: host_id(tag, id), cwd, error }
        }
        ServerMsg::Alert(a) => ServerMsg::Alert(Alert { id: a.id.map(|id| host_id(tag, id)), ..a }),
        ServerMsg::Conversations { query, request, items } => ServerMsg::Conversations {
            request,
            query,
            items: items.into_iter().map(|c| conversation_from(tag, host, c)).collect(),
        },
        ServerMsg::State(state) => ServerMsg::State(state_from(tag, host, state)),
        ServerMsg::Notice { text, error } => ServerMsg::Notice { text: format!("{label}: {text}"), error },
        other => other,
    }
}

/// A frame message from a host with its session id given the host's tag,
/// rewritten in place of being parsed: frames are the bulk of what a host
/// sends. None when `body` is not a frame as serde writes one.
pub fn rewrite_frame(body: &[u8], tag: u16) -> Option<Vec<u8>> {
    const PREFIX: &[u8] = b"{\"t\":\"frame\",\"id\":";
    let rest = body.strip_prefix(PREFIX)?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let id: u64 = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
    if digits == 0 || rest.get(digits) != Some(&b',') {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() + 24);
    out.extend_from_slice(&((body.len() - digits + host_id(tag, id).to_string().len()) as u32).to_be_bytes());
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(host_id(tag, id).to_string().as_bytes());
    out.extend_from_slice(&rest[digits..]);
    Some(out)
}

/// What kind of message a body is, read from its first bytes.
pub fn kind_of(body: &[u8]) -> Option<&str> {
    let rest = body.strip_prefix(b"{\"t\":\"")?;
    let end = rest.iter().position(|b| *b == b'"')?;
    std::str::from_utf8(&rest[..end]).ok()
}

/// Usage windows from every host: per program and window, the most recent
/// reading wins, as the hosts share the owner's accounts.
pub fn merge_usage(base: &mut Usage, other: &Usage) {
    merge_windows(&mut base.claude, &other.claude);
    merge_windows(&mut base.codex, &other.codex);
    merge_windows(&mut base.grok, &other.grok);
    merge_windows(&mut base.gemini, &other.gemini);
    merge_windows(&mut base.claude_models, &other.claude_models);
}

fn merge_windows(base: &mut Vec<UsageWindow>, other: &[UsageWindow]) {
    for w in other {
        match base.iter_mut().find(|b| b.label == w.label) {
            Some(b) if b.updated_at < w.updated_at => *b = w.clone(),
            Some(_) => {}
            None => base.push(w.clone()),
        }
    }
}

/// Every host's errors in one list, oldest first, each named by its host;
/// at most `keep`.
pub fn merge_errors(local: Vec<LogEntry>, hosts: &[(String, Vec<LogEntry>)], keep: usize) -> Vec<LogEntry> {
    let mut all = local;
    for (label, errors) in hosts {
        all.extend(errors.iter().map(|e| LogEntry {
            at: e.at,
            text: format!("{label}: {}", e.text),
            repeats: e.repeats,
        }));
    }
    all.sort_by_key(|e| e.at);
    let skip = all.len().saturating_sub(keep);
    all.split_off(skip)
}

#[cfg(test)]
#[path = "../tests/hosts_fed.rs"]
mod tests;
