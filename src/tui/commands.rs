//! The `:` commands (`actions.rs` names them) and the overlays they open: every branch on every host (F3),
//! the security scans, install reports and scripts' pickers.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Dialog, Mode, Sel};
use crate::actions;
use crate::hotkeys::Hotkey;
use crate::lua::{ItemKind, PickItem};
use crate::protocol::{BranchInfo, ClientMsg, InstallRequest, ServerMsg, SessionInfo, Status, Tool};
use crate::{paths, util, view};

/// The name that marks the scratch terminal.
pub const SCRATCH_NAME: &str = "scratch";

/// A branch as F3 lists it: its host (none: this machine), the branch, and
/// the live sessions working on it.
pub struct BranchRow<'a> {
    pub host: Option<String>,
    pub info: &'a BranchInfo,
    pub sessions: Vec<&'a SessionInfo>,
}

impl BranchRow<'_> {
    /// Kept only as a ref: no worktree has it and no agent works on it.
    pub fn detached(&self) -> bool {
        !self.info.main && self.info.path.is_empty() && self.sessions.is_empty()
    }
}

/// The completion offered on the command line: the words, the one filled
/// in (none: only listed) and where the completed word starts.
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    pub items: Vec<String>,
    pub at: Option<usize>,
    pub start: usize,
}

/// Whether `session` works on `info` on `host`: in its worktree, on its
/// branch, or in the main checkout when the branch is the main one.
fn works_on(session: &SessionInfo, host: Option<&str>, info: &BranchInfo) -> bool {
    if session.host.as_deref() != host || !session.alive || !session.tool.is_agent() {
        return false;
    }
    match &session.worktree {
        Some(wt) => {
            wt.branch.as_deref() == Some(info.branch.as_str()) || (!info.path.is_empty() && wt.path == info.path)
        }
        None => info.main || session.cwd.as_deref().is_some_and(|cwd| !info.path.is_empty() && cwd == info.path),
    }
}

#[path = "../../plugins/agent-extensions/ui.rs"]
mod agent_extensions;
#[path = "../../plugins/dev-services/ui.rs"]
mod dev_services;
#[path = "../../plugins/launch-settings/ui.rs"]
mod launch_settings;

#[path = "../../plugins/security-scans/ui.rs"]
mod security_scans;

impl App {
    /// Every branch on every host, this machine's first, with the sessions
    /// on each; only those every word of `filter` finds.
    pub fn branch_rows(&self, filter: &str) -> Vec<BranchRow<'_>> {
        let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
        let mut sources: Vec<(Option<String>, &[BranchInfo])> = vec![(None, &self.state.branches)];
        for h in &self.hosts {
            if let Some(stats) = &h.stats {
                sources.push((Some(h.name.clone()), &stats.branches));
            }
        }
        let mut rows = Vec::new();
        for (host, branches) in sources {
            let label = self.host_label(host.as_deref()).to_lowercase();
            let first = rows.len();
            for info in branches {
                let sessions: Vec<&SessionInfo> =
                    self.state.sessions.iter().filter(|s| works_on(s, host.as_deref(), info)).collect();
                let found = words.iter().all(|w| {
                    info.branch.to_lowercase().contains(w.as_str())
                        || info.subject.as_deref().is_some_and(|s| s.to_lowercase().contains(w.as_str()))
                        || label.contains(w.as_str())
                        || sessions.iter().any(|s| s.title.to_lowercase().contains(w.as_str()))
                });
                if found {
                    rows.push(BranchRow { host: host.clone(), info, sessions });
                }
            }
            // Each host's main branch, then the ones in a worktree or with an
            // agent, then the rest, each newest first as the service sent them.
            rows[first..].sort_by_key(|row| if row.info.main { 0 } else { 1 + usize::from(row.detached()) });
        }
        rows
    }

    /// `:`: the command line, ready for a command.
    pub fn open_command_line(&mut self) {
        self.command.clear();
        self.completion = None;
        self.command_history_stop();
        self.mode = Mode::Command;
    }

    /// Run one command line, as typed after `:`, bound to a key or sent by
    /// a script.
    pub fn run_command(&mut self, line: &str) {
        let line = line.trim();
        let line = line.strip_prefix(':').unwrap_or(line).trim();
        if line.is_empty() {
            return;
        }
        let (word, rest) = match line.split_once(char::is_whitespace) {
            Some((w, r)) => (w, r.trim()),
            None => (line, ""),
        };
        if matches!(word, "attention" | "supervisor" | "decide") {
            if let Err(why) = self.attention_command(word, rest) {
                self.set_notice(format!(":{word} — {why}"), true);
            }
            return;
        }
        if matches!(word, "queue" | "after" | "batch" | "chore" | "chores") {
            if let Err(why) = self.work_queue_command(word, rest) {
                self.set_notice(format!(":{word} — {why}"), true);
            }
            return;
        }
        if matches!(word, "job" | "jobs") {
            if let Err(why) = self.job_command(rest) {
                self.set_notice(format!(":job — {why}"), true);
            }
            return;
        }
        let Some(spec) = actions::find(word) else {
            if !self.run_user_command(word, rest) && !self.launch_word(word, rest) {
                let help = self.keys.label(Hotkey::Help).unwrap_or_else(|| ":help".into());
                self.set_notice(format!("Not a deck command: :{word}. Tab completes one; {help} lists them."), false);
            }
            return;
        };
        if let Some(owner) = crate::extensions::command_owner(spec.name) {
            if !self.cfg.plugin_enabled(&owner) {
                self.local_error(format!("Plugin {owner} is disabled"));
                return;
            }
        }
        let outcome = match spec.name {
            "new" if rest.is_empty() => {
                self.open_new();
                Ok(())
            }
            "movein" => {
                self.run_hotkey(Hotkey::MoveIn);
                Ok(())
            }
            "window" => {
                self.run_hotkey(Hotkey::KittyWindow);
                Ok(())
            }
            "q" => {
                self.quit = true;
                Ok(())
            }
            "help" if rest == "commands" => {
                self.show_commands();
                Ok(())
            }
            "help" => {
                self.dialog = Some(Dialog::Help);
                Ok(())
            }
            "rename" => {
                self.rename(rest);
                Ok(())
            }
            "lua" => {
                self.lua_eval(rest);
                Ok(())
            }
            "broadcast" if rest.is_empty() => {
                self.open_broadcast();
                Ok(())
            }
            "broadcast" => self.broadcast_now(rest),
            "slots" => {
                let mut lines = Vec::new();
                let sources = std::iter::once(("here", &self.state.scheduler))
                    .chain(self.state.host_schedulers.iter().map(|(host, s)| (host.as_str(), s)));
                for (host, data) in sources {
                    lines.push(format!("{host}: {} holders · {} waiters", data.holders.len(), data.waiters.len()));
                    if let Some(error) = &data.error {
                        lines.push(error.clone());
                    }
                    for s in data.holders.iter().chain(data.waiters.iter()) {
                        lines.push(format!(
                            "#{} {} · {} · pid {} · {}",
                            s.id,
                            s.pool,
                            s.label,
                            s.owner.pid,
                            if s.granted_at.is_some() { "held" } else { "waiting" }
                        ));
                    }
                    for s in &data.starts {
                        if s.session.is_none() {
                            lines.push(format!(
                                "Start #{} {} · {} · reset {}",
                                s.id,
                                s.provider.label(),
                                s.reason,
                                s.reset.map_or_else(|| "unknown".into(), |at| at.to_string())
                            ));
                        }
                    }
                    for p in &data.paused {
                        lines.push(format!(
                            "Session {} {} · {} · reset {}",
                            p.session,
                            p.provider.label(),
                            p.reason,
                            p.reset.map_or_else(|| "unknown".into(), |at| at.to_string())
                        ));
                    }
                    for r in &data.reruns {
                        lines.push(format!(
                            "Rerun #{} session {} · {}",
                            r.id,
                            r.session,
                            if r.authoritative {
                                "repository hook owns rerun"
                            } else {
                                "F6 on session / :rerun <id> [host]"
                            }
                        ));
                    }
                }
                self.dialog =
                    Some(Dialog::Report { title: "Fleet slots".into(), lines, scroll: 0, confirm: None, waiting: 0 });
                Ok(())
            }
            "rerun" => {
                let selected = self.current_session().map(|s| s.id);
                let mut words = rest.split_whitespace();
                let explicit = words.next().map(|s| s.parse::<u64>());
                let named = words.next();
                let mut chosen = None;
                let sources = std::iter::once((None, &self.state.scheduler))
                    .chain(self.state.host_schedulers.iter().map(|(host, s)| (Some(host.as_str()), s)));
                for (host, data) in sources {
                    if named.is_some() && named != host {
                        continue;
                    }
                    for r in &data.reruns {
                        let matches = match &explicit {
                            Some(Ok(id)) => r.id == *id && (named.is_some() || host.is_none()),
                            Some(Err(_)) => false,
                            None => selected == Some(r.session),
                        };
                        if matches && !r.authoritative && data.memory_free {
                            chosen = Some((r.id, host.map(str::to_string)));
                            break;
                        }
                    }
                    if chosen.is_some() {
                        break;
                    }
                }
                match chosen{Some((id,host))=>{let mut args=serde_json::json!({"id":id});if let Some(host)=host{args["host"]=serde_json::json!(host);}self.send(ClientMsg::Fleet{request:0,action:"rerun".into(),args});Ok(())},None=>Err("No eligible memory rerun; select its session when memory is free (:slots shows repository-owned reruns)".into())}
            }
            "dispatch" => {
                match rest
                    .split_once(' ')
                    .and_then(|(provider, prompt)| Tool::parse(provider).map(|tool| (tool, prompt)))
                {
                    Some((tool, prompt)) => {
                        self.send(ClientMsg::Fleet {
                            request: 0,
                            action: "enqueue".into(),
                            args: serde_json::json!({"provider":tool,"prompt":prompt}),
                        });
                        Ok(())
                    }
                    None => Err("dispatch requires a known provider and prompt".into()),
                }
            }
            "send" => self.send_command(rest),
            "go" => self.go(rest),
            "scratch" => {
                self.toggle_scratch();
                Ok(())
            }
            "dev" => self.dev_command(rest),
            "resume" => {
                self.open_picker();
                Ok(())
            }
            "branches" => {
                self.open_branches();
                Ok(())
            }
            "scans" => {
                self.dialog = Some(Dialog::Scans { sel: 0 });
                Ok(())
            }
            "hosts" => {
                self.show_hosts();
                Ok(())
            }
            "machines" => {
                self.dialog = Some(Dialog::Machines);
                Ok(())
            }
            "usage" => {
                self.dialog = Some(Dialog::Usage);
                Ok(())
            }
            "errors" if rest == "clear" => {
                self.clear_errors();
                self.set_notice("Cleared the errors.", false);
                Ok(())
            }
            "errors" => {
                self.open_errors();
                Ok(())
            }
            "restart" => {
                self.restart_service();
                Ok(())
            }
            "stop" => {
                self.run_hotkey(Hotkey::Stop);
                Ok(())
            }
            "reload" => {
                self.reload_lua();
                Ok(())
            }
            "pair" => {
                self.open_pairing();
                Ok(())
            }
            "sidebar" => {
                self.sidebar = !self.sidebar;
                Ok(())
            }
            "set" => self.set_command(rest),
            "skills" => self.skills_command(rest),
            "launch" => self.launch_command(rest),
            name => match actions::words(rest) {
                Ok(words) => match name {
                    "new" => self.new_command(words),
                    "claude" | "codex" | "agy" | "grok" | "opencode" => {
                        let mut start = vec![name.to_string()];
                        start.extend(words);
                        self.new_command(start)
                    }
                    "term" => self.term_command(words),
                    "cd" => self.cd_command(&words),
                    "close" => self.close_command(&words),
                    "reopen" => self.reopen_command(&words),
                    "scan" => self.scan_command(words),
                    "install" => self.install_command(words, false),
                    "uninstall" => self.install_command(words, true),
                    _ => Err(format!(":{name} is not handled here")),
                },
                Err(why) => Err(why),
            },
        };
        if let Err(why) = outcome {
            self.set_notice(format!(":{} — {why}. Usage: :{}", spec.name, spec.usage), false);
        }
    }

    /// A host named in a command: `here` (or this machine's name) is this
    /// machine; another must be one the deck drives and online.
    pub fn resolve_host(&self, word: &str) -> Result<Option<String>, String> {
        let w = word.to_lowercase();
        if w == "here" || w == "local" || w == self.host_label(None).to_lowercase() {
            return Ok(None);
        }
        // must reach "Windows (WSL)". Any word of a host's name or label names it, as does a unique prefix.
        let words = |h: &crate::protocol::HostInfo| -> Vec<String> {
            format!("{} {}", h.name, h.label)
                .to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect()
        };
        // Build hosts run no sessions.
        let hosts: Vec<_> = self.hosts.iter().filter(|h| h.role.is_agents()).collect();
        let exact: Vec<_> =
            hosts.iter().copied().filter(|h| h.name.to_lowercase() == w || h.label.to_lowercase() == w).collect();
        let by_word: Vec<_> = hosts.iter().copied().filter(|h| words(h).contains(&w)).collect();
        let by_prefix: Vec<_> = hosts
            .iter()
            .copied()
            .filter(|h| {
                w.len() >= 2 && (h.label.to_lowercase().starts_with(&w) || words(h).iter().any(|x| x.starts_with(&w)))
            })
            .collect();
        let found = [exact, by_word, by_prefix].into_iter().find(|hosts| !hosts.is_empty()).unwrap_or_default();
        match found.as_slice() {
            [h] if h.online => Ok(Some(h.name.clone())),
            [h] => Err(format!("{} is {}", h.label, view::host_state(h))),
            [] => Err(format!("no host is called {word:?}; :hosts lists them")),
            many => Err(format!(
                "{word:?} could be {}; name one",
                many.iter().map(|h| h.label.as_str()).collect::<Vec<_>>().join(" or ")
            )),
        }
    }

    /// The session a command names by its number in the list; none named:
    /// the highlighted one.
    fn session_arg(&self, word: Option<&str>) -> Result<u64, String> {
        match word {
            None => self.current_session().map(|s| s.id).ok_or_else(|| "highlight a session or give its number".into()),
            Some(n) => match n.trim_start_matches('#').parse::<usize>() {
                Ok(n) if n >= 1 => {
                    self.numbered_sessions().get(n - 1).map(|s| s.id).ok_or_else(|| format!("there is no session {n}"))
                }
                _ => Err(format!("{n:?} is not a session number")),
            },
        }
    }

    /// A folder given to `:new` or `:term`: `~` is the home folder here, a
    /// relative one is under the start directory. On another host the path
    /// must be that host's own.
    fn folder_arg(&self, dir: &str, host: Option<&str>) -> String {
        if host.is_some() {
            return dir.to_string();
        }
        let path = match dir.strip_prefix("~") {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => paths::home().join(rest.trim_start_matches('/')),
            _ => PathBuf::from(dir),
        };
        let path = if path.is_relative() { Path::new(&self.state.start_dir).join(path) } else { path };
        path.to_string_lossy().into_owned()
    }

    /// Start a session the way the commands and scripts do.
    #[allow(clippy::too_many_arguments)]
    pub fn start_session(
        &mut self,
        tool: Tool,
        host: Option<String>,
        cwd: Option<String>,
        name: Option<String>,
        command: Vec<String>,
        model: Option<String>,
    ) {
        self.start_sessions(1, tool, host, cwd, name, command, model);
    }

    /// Start `count` alike sessions at once.
    #[allow(clippy::too_many_arguments)]
    fn start_sessions(
        &mut self,
        count: usize,
        tool: Tool,
        host: Option<String>,
        cwd: Option<String>,
        name: Option<String>,
        command: Vec<String>,
        model: Option<String>,
    ) {
        let placed = cwd.is_some() || name.is_some() || !command.is_empty();
        let needs = if placed { tool.since_version().max(9) } else { tool.since_version() };
        if !self.service_has(needs, &format!("Starting {} that way", tool.label())) {
            return;
        }
        if model.is_some() && !self.service_has(5, "Choosing a model") {
            return;
        }
        if tool == Tool::Codex && !self.service_has(19, "Native Codex creation") {
            return;
        }
        let location = match host.as_deref() {
            Some(name) => format!(
                " on {} in {}",
                self.host_label(Some(name)),
                cwd.clone().unwrap_or_else(|| self.start_dir_on(Some(name)))
            ),
            None => format!(" in {}", cwd.clone().unwrap_or_else(|| self.start_dir_on(None))),
        };
        let what = match (tool, count) {
            (Tool::Shell, _) => "a terminal".to_string(),
            (_, 1) => tool.label().to_string(),
            (_, n) => format!("{n} {} sessions", tool.label()),
        };
        for _ in 0..count {
            self.send(ClientMsg::Create {
                tool,
                model: model.clone(),
                effort: None,
                host: host.clone(),
                cwd: cwd.clone(),
                command: command.clone(),
                name: name.clone(),
                request: None,
            });
        }
        self.set_notice(format!("Starting {what}{location}…"), false);
    }

    /// `:new <tool> [host] [N] [--cwd DIR] [--name NAME] [--model MODEL]`:
    /// a number starts that many sessions, before or after the host.
    fn new_command(&mut self, mut words: Vec<String>) -> Result<(), String> {
        let cwd = actions::take_value(&mut words, "--cwd")?;
        let name = actions::take_value(&mut words, "--name")?;
        let model = actions::take_value(&mut words, "--model")?;
        let mut words = words.into_iter();
        let tool_word = words.next().ok_or("name the program")?;
        let tool = Tool::parse(&tool_word)
            .filter(|t| t.is_agent())
            .ok_or_else(|| format!("{tool_word:?} is not claude, codex, agy, grok or opencode"))?;
        let (mut host_word, mut count_word) = (None, None);
        for word in words {
            let slot = if !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()) {
                &mut count_word
            } else {
                &mut host_word
            };
            if slot.is_some() {
                return Err(format!("{word:?} is one word too many"));
            }
            *slot = Some(word);
        }
        let count = match count_word {
            None => 1,
            Some(word) => match word.parse::<usize>() {
                Ok(n @ 1..=crate::protocol::MAX_START_BATCH) => n,
                _ => return Err(format!("start 1 to {} sessions at once", crate::protocol::MAX_START_BATCH)),
            },
        };
        let host = match host_word {
            Some(word) => self.resolve_host(&word)?,
            None => None,
        };
        if let Some(model) = &model {
            if !crate::service::valid_model_name(model) {
                return Err(format!("{model:?} is not a model name"));
            }
        }
        let cwd = cwd.map(|d| self.folder_arg(&d, host.as_deref()));
        self.start_sessions(count, tool, host, cwd, name, Vec::new(), model);
        Ok(())
    }

    /// `:term [host] [--cwd DIR] [--name NAME] [-- COMMAND…]`.
    fn term_command(&mut self, mut words: Vec<String>) -> Result<(), String> {
        let cwd = actions::take_value(&mut words, "--cwd")?;
        let name = actions::take_value(&mut words, "--name")?;
        let command = match words.iter().position(|w| w == "--") {
            Some(i) => {
                let command = words.split_off(i + 1);
                words.pop();
                if command.is_empty() {
                    return Err("give the command after --".into());
                }
                command
            }
            None => Vec::new(),
        };
        let host = match words.first() {
            Some(word) => self.resolve_host(word)?,
            None => None,
        };
        if words.len() > 1 {
            return Err(format!("{:?} is one word too many; a command goes after --", words[1]));
        }
        let cwd = cwd.map(|d| self.folder_arg(&d, host.as_deref()));
        self.start_session(Tool::Shell, host, cwd, name, command, None);
        Ok(())
    }

    /// `:scratch`: type into the scratch terminal, starting it in
    /// the owner's shell the first time; from it, go back where the keys
    /// were.
    pub fn toggle_scratch(&mut self) {
        let scratch = self
            .state
            .sessions
            .iter()
            .find(|s| s.tool == Tool::Shell && s.alive && s.host.is_none() && s.title == SCRATCH_NAME)
            .map(|s| s.id);
        match scratch {
            Some(id) if self.current == Some(Sel::Session(id)) => {
                let back = self.scratch_back.take().filter(|b| self.rows().contains(b));
                match back {
                    Some(back) => self.select(back),
                    None => self.mode = Mode::Normal,
                }
            }
            Some(id) => {
                self.scratch_back = self.current;
                self.select(Sel::Session(id));
                self.enter_insert();
            }
            None => {
                if !self.service_has(9, "The scratch terminal") {
                    return;
                }
                self.scratch_back = self.current;
                self.send(ClientMsg::Create {
                    tool: Tool::Shell,
                    model: None,
                    effort: None,
                    host: None,
                    cwd: None,
                    command: Vec::new(),
                    name: Some(SCRATCH_NAME.into()),
                    request: None,
                });
                self.set_notice("Opening the scratch terminal in your shell…", false);
            }
        }
    }

    /// `:broadcast <message>`: every agent currently working.
    fn broadcast_now(&mut self, text: &str) -> Result<(), String> {
        if !self.service_has(crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL, "Native owner broadcasts") {
            return Ok(());
        }
        self.send(ClientMsg::Broadcast { running_only: true, ids: vec![], text: text.to_string() });
        self.show_notice("Sending to working agents on all hosts…".into(), false);
        Ok(())
    }

    /// `:send <n> <message>`.
    fn send_command(&mut self, rest: &str) -> Result<(), String> {
        if rest == "--idle" {
            return self.idle_send_command("");
        }
        if let Some(rest) = rest.strip_prefix("--idle ") {
            return self.idle_send_command(rest.trim());
        }
        let (n, text) = rest.split_once(char::is_whitespace).ok_or("give a session number and the message")?;
        let id = self.session_arg(Some(n))?;
        let text = text.trim();
        if text.is_empty() {
            return Err("the message is empty".into());
        }
        self.send_owner_message(id, text)
    }

    pub(super) fn send_owner_message(&mut self, id: u64, text: &str) -> Result<(), String> {
        if !self.service_has(crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL, "Native owner messages") {
            return Ok(());
        }
        if self.owner_sends.len() >= 64 {
            return Err("too many unconfirmed native messages; check the conversations before retrying".into());
        }
        let request = self.next_fleet_request();
        self.owner_sends.insert(request, id);
        self.send(ClientMsg::SendMessage { id, request, text: text.to_string(), attachments: false });
        Ok(())
    }

    /// `:go <n|name>`.
    fn go(&mut self, rest: &str) -> Result<(), String> {
        if rest.is_empty() {
            return Err("give a session number or part of its title".into());
        }
        let id = match self.session_arg(Some(rest)) {
            Ok(id) => id,
            Err(_) => {
                let want = rest.to_lowercase();
                self.ordered_sessions()
                    .iter()
                    .find(|s| s.title.to_lowercase().contains(&want))
                    .map(|s| s.id)
                    .ok_or_else(|| format!("no session's title has {rest:?}"))?
            }
        };
        self.select(Sel::Session(id));
        Ok(())
    }

    fn close_command(&mut self, words: &[String]) -> Result<(), String> {
        // Resolve the entire selection before closing anything: sidebar
        // positions change as rows disappear.
        let ids = if words.is_empty() {
            vec![self.session_arg(None)?]
        } else {
            let sessions = self.numbered_sessions();
            let number = |word: &str| -> Result<usize, String> {
                let word = word.trim();
                let n = word
                    .trim_start_matches('#')
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("{word:?} is not a session number"))?;
                if n > sessions.len() {
                    return Err(format!("there is no session {n}"));
                }
                Ok(n)
            };
            let mut positions = std::collections::BTreeSet::new();
            for part in words.join(" ").split(',') {
                let (first, last) = match part.split_once('-') {
                    Some((first, last)) => (number(first)?, number(last)?),
                    None => {
                        let n = number(part)?;
                        (n, n)
                    }
                };
                if first > last {
                    return Err(format!("session range {part:?} must run from smaller to larger"));
                }
                positions.extend(first..=last);
            }
            positions.into_iter().map(|n| sessions[n - 1].id).collect()
        };
        let count = ids.len();
        for id in ids {
            self.select(Sel::Session(id));
            self.run_hotkey(Hotkey::Close);
        }
        if count > 1 {
            self.set_notice(format!("Closed {count} sessions. :resume resumes their conversations."), false);
        }
        Ok(())
    }

    fn reopen_command(&mut self, words: &[String]) -> Result<(), String> {
        let id = self.session_arg(words.first().map(String::as_str))?;
        match self.session(id).map(|s| s.alive) {
            Some(false) => {
                self.send(ClientMsg::Reopen { id });
                Ok(())
            }
            Some(true) => Err("that session is running".into()),
            None => Err("there is no such session".into()),
        }
    }

    fn cd_command(&mut self, words: &[String]) -> Result<(), String> {
        let s = self.current_session().ok_or("highlight an agent first")?;
        if !s.tool.is_agent() {
            return Err("use cd inside the shell terminal".into());
        }
        if self.is_cloud(s) {
            return Err("cloud agents keep their existing conversation and cannot change project directory".into());
        }
        let id = s.id;
        let cwd = s.cwd.clone().unwrap_or_else(|| self.start_dir_on(s.host.as_deref()));
        if words.len() > 1 {
            return Err("give one directory; quote a path containing spaces".into());
        }
        if words.is_empty() {
            self.open_command_line();
            self.command.set("cd ");
            self.set_notice(format!("Current directory: {cwd}. Enter a path, e.g. .. or ~/projects."), false);
            return Ok(());
        }
        if !self.service_has(14, "Changing an agent's directory") {
            return Ok(());
        }
        self.send(ClientMsg::ChangeDirectory { id, path: words[0].clone() });
        self.set_notice("Changing directory; restarting this agent with a fresh chat…", false);
        Ok(())
    }

    /// `:hosts`.
    fn show_hosts(&mut self) {
        let mut lines = vec![format!("{} (Here)  {}", self.host_label(None), self.state.start_dir)];
        for h in &self.hosts {
            lines.push(format!("{}  [{}]  {}  {}", h.label, h.name, view::host_state(h), h.start_dir));
        }
        if self.hosts.is_empty() {
            lines.push(String::new());
            lines.push("No other host yet: cepheus host add <name> <ssh target>.".into());
        }
        self.dialog = Some(Dialog::Report { title: "Hosts".into(), lines, scroll: 0, confirm: None, waiting: 0 });
    }

    /// `:set` lists the options; `:set name=value` (or `name value`)
    /// changes one, read as JSON where it reads.
    fn set_command(&mut self, rest: &str) -> Result<(), String> {
        let Some(engine) = self.lua.as_mut() else { return Err("Lua is off (\"lua\": false in the config)".into()) };
        if rest.is_empty() {
            let mut lines: Vec<String> = engine
                .options()
                .into_iter()
                .map(|(k, v)| format!("{k} = {}", serde_json::to_string(&v).unwrap_or_default()))
                .collect();
            if lines.is_empty() {
                lines.push("No options set. init.lua sets them as deck.opt.<name> = value.".into());
            }
            self.dialog = Some(Dialog::Report { title: "Options".into(), lines, scroll: 0, confirm: None, waiting: 0 });
            return Ok(());
        }
        let (name, value) = rest
            .split_once('=')
            .or_else(|| rest.split_once(char::is_whitespace))
            .map(|(n, v)| (n.trim(), v.trim()))
            .ok_or("give name=value")?;
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("{name:?} is not an option name"));
        }
        engine.set_option(name, value)?;
        self.refresh_lua_ui();
        self.set_notice(format!("Set {name}."), false);
        Ok(())
    }

    /// F3: the branches overlay.
    pub fn open_branches(&mut self) {
        self.dialog = Some(Dialog::Branches { sel: 0, filter: String::new() });
    }

    /// Keys in the overlays this module adds; false for another dialog.
    pub fn command_dialog_key(&mut self, dialog: Dialog, k: KeyEvent) -> Option<Dialog> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match dialog {
            Dialog::Branches { mut sel, filter } => {
                let count = self.branch_rows(&filter).len();
                match k.code {
                    _ if self.keys.find(&k) == Some(Hotkey::Branches) => return None,
                    KeyCode::Esc => return None,
                    KeyCode::Up => sel = sel.saturating_sub(1),
                    KeyCode::Down => sel = (sel + 1).min(count.saturating_sub(1)),
                    KeyCode::PageUp => sel = sel.saturating_sub(10),
                    KeyCode::PageDown => sel = (sel + 10).min(count.saturating_sub(1)),
                    KeyCode::Enter => {
                        self.branch_action(&filter, sel, None);
                        return None;
                    }
                    KeyCode::Char(c @ ('x' | 't' | 'n')) if ctrl => {
                        let tool = match c {
                            'x' => Tool::Codex,
                            't' => Tool::Shell,
                            _ => Tool::Claude,
                        };
                        self.branch_action(&filter, sel, Some(tool));
                        return None;
                    }
                    _ => {}
                }
                Some(Dialog::Branches { sel, filter })
            }
            Dialog::Scans { mut sel } => {
                let count = self.state.scans.len();
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => return None,
                    KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(count.saturating_sub(1)),
                    KeyCode::Char('s') => {
                        let _ = self.scan_command(Vec::new());
                        return None;
                    }
                    KeyCode::Enter => {
                        self.open_scan(sel);
                        return None;
                    }
                    _ => {}
                }
                Some(Dialog::Scans { sel })
            }
            Dialog::Report { title, lines, mut scroll, confirm, waiting } => {
                match k.code {
                    KeyCode::Char('y' | 'Y') if confirm.is_some() => {
                        let msgs = confirm.unwrap_or_default();
                        let waiting = msgs.len();
                        for msg in msgs {
                            self.send(msg);
                        }
                        let lines = vec!["Working…".to_string()];
                        return Some(Dialog::Report { title, lines, scroll: 0, confirm: None, waiting });
                    }
                    KeyCode::Esc | KeyCode::Char('q' | 'n' | 'N') => return None,
                    KeyCode::Enter if confirm.is_none() => return None,
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => scroll = (scroll + 1).min(lines.len().saturating_sub(1)),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    KeyCode::PageDown => scroll = (scroll + 10).min(lines.len().saturating_sub(1)),
                    _ => {}
                }
                Some(Dialog::Report { title, lines, scroll, confirm, waiting })
            }
            Dialog::Choose { pick, mut sel, filter, callback } => {
                let matches = choose_matches(&pick.items, &filter);
                let chosen = matches
                    .get(sel)
                    .filter(|&&i| pick.items[i].kind == ItemKind::Choice)
                    .map(|&i| (i, pick.items[i].plain()));
                // Search text has already been handled; named actions remain
                // available while browsing the panel.
                let named = match k.code {
                    KeyCode::Char(c) if !ctrl => Some(c.to_string()),
                    KeyCode::Backspace => Some("backspace".to_string()),
                    KeyCode::Left => Some("left".to_string()),
                    KeyCode::Right => Some("right".to_string()),
                    KeyCode::Tab => Some("tab".to_string()),
                    KeyCode::Delete => Some("delete".to_string()),
                    _ => None,
                };
                if let Some(key) = named.filter(|key| pick.keys.iter().any(|k| &k.key == key)) {
                    if let Some(engine) = self.lua.as_mut() {
                        engine.choose(callback, chosen.as_ref().map(|(i, item)| (*i, item.as_str())), Some(&key));
                    }
                    self.drain_lua();
                    return None;
                }
                // ← → and Tab move between a picker's pages; the script's
                // `on_tab` draws the page into the open window.
                let pages = pick.tabs.len();
                let step = match k.code {
                    KeyCode::Right | KeyCode::Tab if pages > 1 => Some(1),
                    KeyCode::Left | KeyCode::BackTab if pages > 1 => Some(pages - 1),
                    _ => None,
                };
                if let Some(step) = step {
                    let mut pick = pick;
                    pick.tab = (pick.tab + step) % pages;
                    let (index, label) = (pick.tab, pick.tabs[pick.tab].label.clone());
                    self.dialog_viewport = super::DialogViewport::default();
                    self.dialog = Some(Dialog::Choose { pick, sel: 0, filter: String::new(), callback });
                    if let Some(engine) = self.lua.as_mut() {
                        engine.tab(callback, index, &label);
                    }
                    self.drain_lua();
                    return None;
                }
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        if let Some(engine) = self.lua.as_mut() {
                            engine.forget(callback);
                        }
                        return None;
                    }
                    KeyCode::Up => sel = choose_snap(&pick.items, &matches, sel.saturating_sub(1), false),
                    KeyCode::Down => sel = choose_snap(&pick.items, &matches, sel + 1, true),
                    KeyCode::PageUp => sel = choose_snap(&pick.items, &matches, sel.saturating_sub(10), false),
                    KeyCode::PageDown => sel = choose_snap(&pick.items, &matches, sel + 10, true),
                    KeyCode::Enter => {
                        let Some((i, item)) = chosen else {
                            // Nothing to choose here: a panel stays open.
                            return Some(Dialog::Choose { pick, sel, filter, callback });
                        };
                        if let Some(engine) = self.lua.as_mut() {
                            engine.choose(callback, Some((i, &item)), None);
                        }
                        self.drain_lua();
                        return None;
                    }
                    _ => {}
                }
                Some(Dialog::Choose { pick, sel, filter, callback })
            }
            other => Some(other),
        }
    }

    /// Enter on a branch: go to the agent on it; or, with `tool`, start
    /// one there (Ctrl+n Claude, Ctrl+x Codex, Ctrl+t a terminal).
    fn branch_action(&mut self, filter: &str, sel: usize, tool: Option<Tool>) {
        let rows = self.branch_rows(filter);
        let Some(row) = rows.get(sel) else { return };
        let (host, path, branch) = (row.host.clone(), row.info.path.clone(), row.info.branch.clone());
        let first = row.sessions.first().map(|s| s.id);
        drop(rows);
        match (tool, first) {
            (None, Some(id)) => self.select(Sel::Session(id)),
            (None, None) | (Some(_), _) if path.is_empty() => {
                self.set_notice(format!("{branch} is not checked out anywhere, so nothing can start in it."), true)
            }
            (None, None) => {
                let new = "Ctrl+n starts Claude there, Ctrl+x Codex, Ctrl+t a terminal";
                self.set_notice(format!("No agent works on {branch}. {new}."), false);
                self.dialog = Some(Dialog::Branches { sel, filter: filter.to_string() });
            }
            (Some(tool), _) => {
                let name = (tool == Tool::Shell).then(|| branch.clone());
                self.start_session(tool, host, Some(path), name, Vec::new(), None);
            }
        }
    }

    /// Tab on the command line: complete the word before the cursor, list
    /// the choices, and on the next Tab step through them (Shift+Tab back).
    /// What follows the cursor stays, as in a shell.
    pub fn complete_command(&mut self, back: bool) {
        if let Some(c) = self.completion.as_mut().filter(|c| !c.items.is_empty()) {
            let n = c.items.len();
            let at = match c.at {
                None if back => n - 1,
                None => 0,
                Some(at) if back => (at + n - 1) % n,
                Some(at) => (at + 1) % n,
            };
            c.at = Some(at);
            self.command.replace_before(c.start, &c.items[at]);
            return;
        }
        let ctx = self.completion_ctx();
        let head = self.command.split().0;
        let (start, mut items) = actions::complete(head, &ctx);
        if !head.contains(char::is_whitespace) {
            items.extend(["job", "jobs"].into_iter().filter(|s| s.starts_with(head)).map(str::to_string));
            items.sort();
            items.dedup();
        } else if let Some(rest) = head.strip_prefix("queue ") {
            if !rest.contains(char::is_whitespace) {
                items = [
                    "list", "add", "add-json", "cancel", "reorder", "enable", "disable", "sources", "source",
                    "refresh", "help",
                ]
                .into_iter()
                .filter(|s| s.starts_with(rest))
                .map(str::to_string)
                .collect();
            }
        } else if let Some(rest) = head.strip_prefix("chores ").or_else(|| head.strip_prefix("chore ")) {
            if !rest.contains(char::is_whitespace) {
                items = ["list", "register", "register-json", "schedule", "enable", "disable", "run", "remove"]
                    .into_iter()
                    .filter(|s| s.starts_with(rest))
                    .map(str::to_string)
                    .collect();
            }
        } else if let Some(rest) = head.strip_prefix("job ") {
            if !rest.contains(char::is_whitespace) {
                items = ["start", "status", "chat", "raw", "next", "prev", "stop", "dismiss", "help"]
                    .into_iter()
                    .filter(|s| s.starts_with(rest))
                    .map(str::to_string)
                    .collect();
            }
        }
        let typed = head.len() - start;
        match items.len() {
            0 => {}
            1 => {
                self.command.replace_before(start, &items[0]);
                // A space ends the word; one already there is stepped over.
                if self.command.split().1.starts_with(' ') {
                    self.command.right();
                } else {
                    self.command.insert(" ");
                }
            }
            _ => {
                let prefix = actions::common_prefix(&items);
                if prefix.len() > typed {
                    self.command.replace_before(start, &prefix);
                }
                self.completion = Some(Completion { items, at: None, start });
            }
        }
    }

    fn completion_ctx(&self) -> actions::Ctx {
        // A bundled view plugin turns off with `:set <id> false`.
        let bundled =
            crate::lua::BUNDLED_SCRIPTS.iter().filter(|(_, context, _)| *context == "ui").map(|(id, _, _)| *id);
        let mut options: Vec<String> = ["sidebar", "badges"].into_iter().chain(bundled).map(str::to_string).collect();
        options.dedup();
        if let Some(engine) = &self.lua {
            options.extend(engine.options().into_keys());
        }
        let mut user: Vec<(String, Vec<String>)> =
            self.user_commands.iter().map(|c| (c.name.clone(), c.complete.clone())).collect();
        user.extend(self.launch_words());
        actions::Ctx {
            hosts: self.hosts.iter().map(|h| h.name.clone()).collect(),
            user,
            options,
            sessions: self.numbered_sessions().len(),
        }
    }

    /// Clear F2's list here and, from protocol 4, in the service. From
    /// protocol 14 the deck keeps the clear until the service's clear
    /// counters show it applied, and a new connection carries it again.
    pub fn clear_errors(&mut self) {
        let newest = self.error_list().iter().map(|e| e.at).max();
        self.errors_cleared_at = newest.unwrap_or(self.errors_cleared_at);
        self.local_errors.clear();
        self.state.errors.clear();
        self.notice = None;
        if self.service_version >= 14 {
            self.pending_clear = Some(super::PendingClear { over: self.state.error_clears.clone(), sent: true });
        }
        if self.service_version >= 4 {
            self.send(ClientMsg::ClearErrors);
        }
    }

    /// The complete catalog shared by Help and `:help commands`.
    pub(crate) fn command_help(&self) -> Vec<String> {
        let mut lines = super::job_sessions::help_lines();
        lines.extend(super::attention_commands::help_lines());
        for spec in actions::COMMANDS {
            let aliases =
                if spec.aliases.is_empty() { String::new() } else { format!("  (also :{})", spec.aliases.join(" :")) };
            lines.push(format!(":{}  {}{aliases}", spec.usage, spec.about));
        }
        if !self.user_commands.is_empty() {
            lines.push(String::new());
            lines.push("From scripts:".into());
            for c in &self.user_commands {
                lines.push(format!(":{}    {}", c.name, c.desc));
            }
        }
        if !self.lua_keys.is_empty() {
            lines.push(String::new());
            lines.push("Keys from scripts:".into());
            for k in &self.lua_keys {
                let does = match &k.action {
                    crate::lua::KeyAction::Command(line) => format!(":{line}"),
                    crate::lua::KeyAction::Lua(_) => "a function".to_string(),
                };
                let desc = if k.desc.is_empty() { does } else { format!("{} ({does})", k.desc) };
                lines.push(format!("{:<12} {desc}", k.key));
            }
        }
        lines
    }

    fn show_commands(&mut self) {
        self.dialog = Some(Dialog::Report {
            title: "Commands".into(),
            lines: self.command_help(),
            scroll: 0,
            confirm: None,
            waiting: 0,
        });
    }

    /// A session needing the owner, for the attention key: never a terminal.
    pub fn wants_attention(s: &SessionInfo, want: Status) -> bool {
        s.status == want && s.tool.is_agent()
    }
}

/// The items of a script's picker every word of `filter` finds.
/// The rows of a script's picker that `filter` keeps, by index: all of
/// them without one; with one, the matching rows under their group's
/// heading.
pub fn choose_matches(items: &[PickItem], filter: &str) -> Vec<usize> {
    let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return (0..items.len()).collect();
    }
    let mut out = Vec::new();
    let mut heading = None;
    for (i, item) in items.iter().enumerate() {
        if item.kind == ItemKind::Heading {
            heading = Some(i);
            continue;
        }
        let text = item.plain().to_lowercase();
        if words.iter().all(|w| text.contains(w.as_str())) {
            out.extend(heading.take());
            out.push(i);
        }
    }
    out
}

/// The row the selection lands on near `pos` among `matches`: `pos` when it
/// can be chosen, else the nearest that can in the direction it moved, else
/// the nearest the other way. Headings and text rows are never selected.
pub fn choose_snap(items: &[PickItem], matches: &[usize], pos: usize, down: bool) -> usize {
    let last = matches.len().saturating_sub(1);
    let pos = pos.min(last);
    let choosable = |p: &usize| items[matches[*p]].kind == ItemKind::Choice;
    if matches.is_empty() || choosable(&pos) {
        return pos;
    }
    let ahead = (pos + 1..matches.len()).find(choosable);
    let behind = (0..pos).rev().find(choosable);
    let found = if down { ahead.or(behind) } else { behind.or(ahead) };
    found.unwrap_or(pos)
}

#[cfg(test)]
#[path = "../tests/close_commands.rs"]
mod tests;
