//! Owner input for hosted Claude sessions through Claude's own prompt interface, as Codex takes it through its
//! own queue. Every Claude the deck launches loads the `cepheus-owner-input` plugin (`--plugin-dir`). For the
//! session's life the plugin runs `cepheus hook owner-input listen`, submits each message this service hands it
//! with `$.prompt.submit({ text, asUser: true })`, the call Claude makes for a typed prompt, and reports what
//! Claude answered with `cepheus hook owner-input receipt`. A plugin's prompt runs only once Claude is idle, so
//! a broadcast, which addresses working agents, is handed over to steer: the plugin holds it for the running
//! main turn's next tool result and attaches it there, as a prompt typed mid-turn joins the turn, and submits
//! it as its own turn only if that turn ends first. The owner's Stop ends the running turn through the same
//! plugin (`$.turn.abort`): a terminal Esc would make Claude remove the owner's queued messages and refuse
//! every later one until someone types at the terminal. Nothing is typed into the terminal and no peer inbox or
//! remote endpoint is involved. The plugin holds no policy: which session a listener serves and what an answer
//! means is decided here.
use super::*;
use crate::protocol::{AlertKind, Delivered, OwnerInputOutcome};
use crate::providers::messages::Delivery;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

/// The plugin's name; its folders' names start with it.
const PLUGIN: &str = "cepheus-owner-input";
/// The plugin's files, by path within its folder.
const FILES: [(&str, &str); 3] = [
    (".claude-plugin/plugin.json", include_str!("claude-owner-input/.claude-plugin/plugin.json")),
    ("hooks/hooks.json", include_str!("claude-owner-input/hooks/hooks.json")),
    ("hooks/register.ts", include_str!("claude-owner-input/hooks/register.ts")),
];
/// Submitted messages whose later answer is still awaited, at most.
const ANSWERS: usize = 256;
/// How long the plugin has to answer a Stop.
const STOP_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct Inputs {
    /// The listener serving each session, by session id.
    listeners: HashMap<u64, Listener>,
    /// Messages handed to a listener, by message id.
    pending: HashMap<String, Pending>,
}

struct Listener {
    client: u64,
    /// The hosted Claude process it runs under, and that process's start.
    pid: u32,
    start: u64,
    /// Its plugin ends a turn when handed `OwnerStop`.
    stop: bool,
}

struct Pending {
    session: u64,
    pid: u32,
    batch: u64,
    deadline: Instant,
    /// The conversation, text and files of a message no listener has been
    /// handed yet: Claude connects its listener once it has loaded its
    /// plugins, and again after the service restarts.
    waiting: Option<(String, String, Vec<String>)>,
    /// Deliver it into the running main turn (a broadcast).
    steer: bool,
    /// When Claude took it; its delivery is then settled, and only a later
    /// refusal is still reported, on the phone's copy too.
    submitted: Option<Instant>,
    /// The phone's identity of the message.
    client_id: Option<String>,
    /// An owner Stop, not a message.
    stop: bool,
}

const NOT_CONNECTED: &str = "Claude's native owner input is not connected in this session; nothing was sent";

/// The plugin folder a Claude launch passes to `--plugin-dir`: one folder for
/// each version of the plugin, written once and never changed, because Claude
/// reloads a plugin folder whose files change under a running session.
pub(super) fn materialize(state: &Path) -> Result<PathBuf> {
    let mut content = Vec::new();
    for (path, text) in FILES {
        content.extend_from_slice(path.as_bytes());
        content.push(0);
        content.extend_from_slice(text.as_bytes());
        content.push(0);
    }
    let digest = crate::remote::auth::sha256_hex(&content);
    let root = state.join("claude-plugins");
    crate::job_sessions::store::directory(&root)?;
    let dir = root.join(format!("{PLUGIN}-{}", &digest[..16]));
    if !dir.exists() {
        let staging = root.join(format!(".{PLUGIN}-{}", util::uuid_v4()?));
        let placed = FILES
            .iter()
            .try_for_each(|(path, text)| -> Result<()> {
                let file = staging.join(path);
                fs::create_dir_all(file.parent().context("plugin file without a folder")?)?;
                Ok(fs::write(&file, text)?)
            })
            .and_then(|()| Ok(fs::rename(&staging, &dir)?));
        if placed.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        // Another launch may have placed the same folder first.
        if !dir.is_dir() {
            placed.context("could not place Claude's owner input plugin")?;
        }
    }
    for (path, text) in FILES {
        anyhow::ensure!(
            fs::read(dir.join(path)).is_ok_and(|bytes| bytes == text.as_bytes()),
            "Claude's owner input plugin in {} was changed; remove that folder to restore it",
            dir.display()
        );
    }
    Ok(dir)
}

/// Whether the hosted Claude was started with the plugin: by the command line
/// this service started it with, or, for a Claude adopted from an earlier run
/// of the service, by the command line its process runs. The process's own
/// command line is not read for a launch of this service's, because the
/// launch can return before the program has replaced the service's copy.
fn launched_with_plugin(hosted: &crate::session::Hosted) -> bool {
    let cmdline;
    let args: Vec<&[u8]> = match &hosted.argv {
        Some(argv) => argv.iter().map(|arg| arg.as_bytes()).collect(),
        None => {
            let Some(pid) = hosted.pid else { return false };
            let Ok(read) = fs::read(format!("/proc/{pid}/cmdline")) else { return false };
            cmdline = read;
            cmdline.split(|byte| *byte == 0).collect()
        }
    };
    args.windows(2).any(|pair| {
        pair[0] == b"--plugin-dir"
            && Path::new(OsStr::from_bytes(pair[1]))
                .file_name()
                .is_some_and(|name| name.as_bytes().starts_with(PLUGIN.as_bytes()))
    })
}

fn clean(reason: Option<String>) -> String {
    let reason: String =
        reason.unwrap_or_default().chars().map(|c| if c.is_control() { ' ' } else { c }).take(300).collect();
    if reason.trim().is_empty() {
        "no reason given".into()
    } else {
        reason
    }
}

impl Service {
    /// The hosted Claude session hook client `cid` runs under: the session's
    /// id and its process (pid, start).
    fn owner_input_session(&self, cid: u64) -> Result<(u64, u32, u64)> {
        let client = self.clients.get(&cid).context("the connection closed")?;
        anyhow::ensure!(client.role == ClientRole::Hook, "owner input takes only a hook connection");
        let (peer, start) =
            client.native_endpoint.as_ref().map(|e| e.peer()).context("the caller's process is unknown")?;
        anyhow::ensure!(procs::stat(peer).is_some_and(|s| s.start_ticks == start), "the caller's process changed");
        let mut found = None;
        for s in self.sessions.values().filter(|s| s.alive && s.closing.is_none() && s.tool == Tool::Claude) {
            let Some(pid) = s.hosted.as_ref().and_then(|h| h.pid) else { continue };
            if crate::providers::peers::descendant(peer, pid) {
                anyhow::ensure!(found.is_none(), "the caller runs under more than one session");
                found = Some((s.id, pid));
            }
        }
        let (id, pid) = found.context("the caller does not run under a Claude session this service hosts")?;
        let start = procs::stat(pid).context("the Claude process ended")?.start_ticks;
        Ok((id, pid, start))
    }

    /// `cepheus hook owner-input listen`: serve the session the listener runs
    /// under, in a conversation that session has had.
    pub(super) fn owner_input_listen(&mut self, cid: u64, conversation: &str, stop: bool) {
        let bound = self.owner_input_session(cid).and_then(|(id, pid, start)| {
            let s = &self.sessions[&id];
            anyhow::ensure!(
                s.conversation.as_deref() == Some(conversation) || s.left.iter().any(|c| c == conversation),
                "the listener's Claude conversation is not this session's"
            );
            Ok((id, Listener { client: cid, pid, start, stop }))
        });
        match bound {
            // One listener a session: a plugin loaded again replaces the old.
            Ok((id, listener)) => {
                let pid = listener.pid;
                if let Some(old) = self.messages.inputs.listeners.insert(id, listener).filter(|old| old.client != cid) {
                    self.owner_input_close(old.client, "another owner input listener serves this session now");
                }
                self.owner_input_hand(id, pid, cid);
            }
            Err(error) => {
                self.log_error(&format!("Claude owner input refused a listener: {error:#}"));
                self.owner_input_close(cid, &format!("{error:#}"));
            }
        }
    }

    /// Refuse a hook connection and close it.
    fn owner_input_close(&mut self, cid: u64, text: &str) {
        self.send(cid, &ServerMsg::Notice { text: text.into(), error: true });
        self.clients.remove(&cid);
    }

    /// Hand an owner message to the listener of session `id`. Its receipt
    /// settles the delivery; without one by `deadline` the outcome is unknown.
    /// A Claude launched with the plugin whose listener is not connected yet
    /// is handed the message once it connects; if it has not by `deadline`,
    /// nothing was sent. A `steer` message joins the running main turn.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn owner_input_send(
        &mut self,
        id: u64,
        batch: u64,
        conversation: &str,
        text: &str,
        files: &[String],
        steer: bool,
        client_id: Option<&str>,
        deadline: Instant,
    ) -> Result<(), String> {
        let hosted = self.sessions.get(&id).and_then(|s| s.hosted.as_ref()).ok_or("not running")?;
        let pid = hosted.pid.ok_or("not running")?;
        let start = procs::stat(pid).map(|s| s.start_ticks);
        let client = self
            .messages
            .inputs
            .listeners
            .get(&id)
            .filter(|l| l.pid == pid && Some(l.start) == start && self.clients.contains_key(&l.client))
            .map(|l| l.client);
        if client.is_none() && !launched_with_plugin(hosted) {
            return Err("this Claude session was started before native owner input; resume it to enable".into());
        }
        let message = util::uuid_v4().map_err(|e| format!("could not name the message: {e}; nothing was sent"))?;
        let (conversation, text, files) = (conversation.to_string(), text.to_string(), files.to_vec());
        let waiting = match client {
            Some(client) => {
                let input = ServerMsg::OwnerInput { message: message.clone(), conversation, text, files, steer };
                self.send(client, &input);
                None
            }
            None => Some((conversation, text, files)),
        };
        let client_id = client_id.map(str::to_owned);
        let pending =
            Pending { session: id, pid, batch, deadline, waiting, steer, submitted: None, client_id, stop: false };
        self.messages.inputs.pending.insert(message, pending);
        Ok(())
    }

    /// The owner's Stop for session `id`, through its Claude plugin when the
    /// listener serving its current process can end a turn: false when it
    /// cannot, and the caller sends the terminal's Esc instead.
    pub(super) fn owner_input_stop(&mut self, id: u64) -> bool {
        let Some(s) = self.sessions.get(&id).filter(|s| s.alive && s.closing.is_none() && s.tool == Tool::Claude)
        else {
            return false;
        };
        let Some(pid) = s.hosted.as_ref().and_then(|h| h.pid) else { return false };
        let start = procs::stat(pid).map(|s| s.start_ticks);
        let Some(client) = (self.messages.inputs.listeners.get(&id))
            .filter(|l| l.stop && l.pid == pid && Some(l.start) == start && self.clients.contains_key(&l.client))
            .map(|l| l.client)
        else {
            return false;
        };
        let Ok(message) = util::uuid_v4() else { return false };
        self.send(client, &ServerMsg::OwnerStop { message: message.clone() });
        let pending = Pending {
            session: id,
            pid,
            batch: 0,
            deadline: Instant::now() + STOP_DEADLINE,
            waiting: None,
            steer: false,
            submitted: None,
            client_id: None,
            stop: true,
        };
        self.messages.inputs.pending.insert(message, pending);
        true
    }

    /// Report an owner message Claude refused after taking it, on the phone's
    /// copy as well as in the error list: the phone had shown it submitted.
    fn owner_input_refused(&mut self, session: u64, client_id: Option<&str>, error: &str) {
        let label = self.label_of(session);
        let text = format!("{label}: {error}");
        let repeated = self.log_error(&text);
        if client_id.is_some() || !repeated {
            self.alert_remote_for(AlertKind::Error, Some(session), &label, error, client_id);
        }
        self.broadcast(&ServerMsg::Notice { text, error: true });
    }

    /// Hand the messages that waited for session `id`'s listener to the
    /// listener `client` that now serves its Claude process `pid`.
    fn owner_input_hand(&mut self, id: u64, pid: u32, client: u64) {
        let waiting: Vec<String> = self
            .messages
            .inputs
            .pending
            .iter()
            .filter(|(_, p)| p.session == id && p.waiting.is_some())
            .map(|(message, _)| message.clone())
            .collect();
        for message in waiting {
            let Some((batch, sent_to)) = self.messages.inputs.pending.get(&message).map(|p| (p.batch, p.pid)) else {
                continue;
            };
            if sent_to != pid || !self.messages.current(id, batch) {
                self.messages.inputs.pending.remove(&message);
                let error = "the session changed or the delivery was canceled; nothing was sent";
                self.native_message_delivered(id, batch, Delivery::Failed(error.into()));
                continue;
            }
            let Some((conversation, text, files, steer)) =
                self.messages.inputs.pending.get_mut(&message).and_then(|p| {
                    p.waiting.take().map(|(conversation, text, files)| (conversation, text, files, p.steer))
                })
            else {
                continue;
            };
            self.send(client, &ServerMsg::OwnerInput { message, conversation, text, files, steer });
        }
    }

    /// `cepheus hook owner-input receipt`: what Claude answered for a message,
    /// accepted only from a process of the message's own session.
    pub(super) fn owner_input_receipt(
        &mut self,
        cid: u64,
        message: &str,
        outcome: OwnerInputOutcome,
        reason: Option<String>,
    ) {
        let checked = self.owner_input_session(cid).and_then(|(id, pid, _)| {
            let pending = (self.messages.inputs.pending.get(message))
                .filter(|p| p.waiting.is_none())
                .context("no owner message awaits this receipt")?;
            anyhow::ensure!(
                pending.session == id && pending.pid == pid,
                "the receipt is not from the message's session"
            );
            Ok(())
        });
        let Some(pending) = checked.as_ref().ok().and_then(|()| self.messages.inputs.pending.remove(message)) else {
            let error =
                checked.err().map_or_else(|| "no owner message awaits this receipt".into(), |e| format!("{e:#}"));
            return self.owner_input_close(cid, &error);
        };
        self.send(cid, &ServerMsg::Notice { text: String::new(), error: false });
        let reason = clean(reason);
        let (session, batch) = (pending.session, pending.batch);
        if pending.stop {
            if !matches!(outcome, OwnerInputOutcome::Stopped | OwnerInputOutcome::Idle) {
                let label = self.label_of(session);
                self.announce_error(format!("{label}: Claude did not stop the turn ({reason}); press Stop again"));
            }
            return;
        }
        let failed = |outcome| match outcome {
            OwnerInputOutcome::Dropped => format!("Claude did not take the message ({reason}); nothing was sent"),
            OwnerInputOutcome::Moved => "the Claude session moved to another conversation; nothing was sent".into(),
            _ => format!("Claude's prompt interface failed ({reason}); check the conversation before resending"),
        };
        match (pending.submitted, outcome) {
            // Claude starts it as a turn of its own once it is idle, or a
            // steered message waits for the running turn's next tool result;
            // either is reported separately and may come much later.
            (None, OwnerInputOutcome::Submitted) => {
                let pending = Pending { submitted: Some(Instant::now()), ..pending };
                self.messages.inputs.pending.insert(message.into(), pending);
                self.native_message_delivered(session, batch, Delivery::Accepted(Delivered::Submitted));
            }
            (None, OwnerInputOutcome::Started) => {
                self.native_message_delivered(session, batch, Delivery::Accepted(Delivered::Started))
            }
            (None, OwnerInputOutcome::Steered) => {
                self.native_message_delivered(session, batch, Delivery::Accepted(Delivered::Steered))
            }
            (None, outcome) => self.native_message_delivered(session, batch, Delivery::Failed(failed(outcome))),
            (Some(_), OwnerInputOutcome::Started) => {
                if let Some(s) = self
                    .sessions
                    .get_mut(&session)
                    .filter(|s| s.alive && s.hosted.as_ref().and_then(|h| h.pid) == Some(pending.pid))
                {
                    s.tracker.user_input();
                    s.tracker.clear_finished();
                }
            }
            (Some(_), OwnerInputOutcome::Dropped | OwnerInputOutcome::Failed) => {
                self.owner_input_refused(session, pending.client_id.as_deref(), &failed(outcome));
            }
            (Some(_), _) => {}
        }
    }

    /// Settle what Claude did not answer in time, and forget listeners and
    /// awaited answers whose session or process is gone.
    pub(super) fn owner_input_due(&mut self) {
        let (sessions, clients) = (&self.sessions, &self.clients);
        let live = |id: &u64, pid: u32| {
            sessions.get(id).is_some_and(|s| s.alive && s.hosted.as_ref().and_then(|h| h.pid) == Some(pid))
        };
        let inputs = &mut self.messages.inputs;
        inputs.listeners.retain(|id, l| clients.contains_key(&l.client) && live(id, l.pid));
        inputs.pending.retain(|_, p| (p.submitted.is_none() && !p.stop) || live(&p.session, p.pid));
        while inputs.pending.len() > ANSWERS {
            let Some(oldest) =
                inputs.pending.iter().filter_map(|(m, p)| Some((p.submitted?, m.clone()))).min().map(|(_, m)| m)
            else {
                break;
            };
            inputs.pending.remove(&oldest);
        }
        let now = Instant::now();
        let stops: Vec<String> = (inputs.pending.iter())
            .filter(|(_, p)| p.stop && now >= p.deadline)
            .map(|(message, _)| message.clone())
            .collect();
        for message in stops {
            let Some(p) = self.messages.inputs.pending.remove(&message) else { continue };
            let label = self.label_of(p.session);
            self.announce_error(format!("{label}: Claude did not confirm the Stop; press Stop again"));
        }
        let messages = &self.messages;
        let settled: Vec<(String, &'static str)> = messages
            .inputs
            .pending
            .iter()
            .filter(|(_, p)| p.submitted.is_none() && !p.stop)
            .filter_map(|(message, p)| {
                let error = match &p.waiting {
                    Some(_) if !messages.current(p.session, p.batch) => "delivery canceled; nothing further was sent",
                    _ if now < p.deadline => return None,
                    Some(_) => NOT_CONNECTED,
                    None => "Claude did not confirm the owner message; check the conversation before resending",
                };
                Some((message.clone(), error))
            })
            .collect();
        for (message, error) in settled {
            let Some(p) = self.messages.inputs.pending.remove(&message) else { continue };
            self.native_message_delivered(p.session, p.batch, Delivery::Failed(error.into()));
        }
    }

    /// Let every unanswered owner message reach its deadline now.
    #[cfg(test)]
    pub(super) fn owner_input_expire(&mut self) {
        for pending in self.messages.inputs.pending.values_mut() {
            pending.deadline = Instant::now();
        }
    }
}

#[cfg(test)]
#[path = "../../src/tests/claude_input.rs"]
mod tests;
