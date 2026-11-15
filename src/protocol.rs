//! Messages between the TUI (or helper commands) and the background service.
//!
//! Each message is a 4-byte big-endian length followed by a JSON body.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::screen::ScreenFrame;

fn is_false(value: &bool) -> bool {
    !value
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Version 2 added Antigravity, the dev server and the error log; version 3
/// the restart in place; version 4 Grok and opencode; version 5 the phone
/// gateway (`remote` role, alerts, a model and effort for new sessions);
/// version 6 the owner's own session names (`Rename`); version 7 other hosts
/// (`host` on sessions and starts, the `peer` role, `Hosts`); version 8 a
/// file pasted from the deck's clipboard (`PasteFile`); version 9 scratch
/// terminals (`Tool::Shell`, a start directory and command on `Create`),
/// sub-agents and activity on sessions, branches on hosts, script panels,
/// installs, security scans and a restart that reaches every host; version
/// 10 targeted shell resizing (`ResizeTerminal`) and Claude Code's send now
/// on a queued message (`SendNow`, `SentNow`). A deck only sends those to a
/// service that welcomed it with that version or later: an older service
/// drops the connection on a message it cannot read. Version 11 added
/// registered providers. The service leaves sessions of a newer program out of
/// what it sends an older deck. Version 14 adds fleet scheduling, jobs, work
/// queues, correlated native message receipts, shared ordering and automatic
/// placement, working-only broadcasts, directory changes and persistent
/// error-clear acknowledgments. Clients gate these requests on version 14.
/// Version 15 adds correlated broadcast receipts and private attention actions;
/// version 16 adds native permission hooks; version 17 adds process generations.
/// Version 18 adds bounded styled history snapshots, with terminal incarnation,
/// dimensions and output-revision fences. Both coordinator and source gate it.
/// Version 19 guarantees native owner-message delivery without terminal fallback.
/// Version 20 sends a deck view or phone gateway only the parts of the state
/// that changed (`StatePatch`) after its first whole state.
/// Version 21 adds native owner steering receipts.
/// Version 22 adds the owner's native Stop (`Stop`), which ends a Claude turn
/// through Claude's own interface instead of a terminal Esc; a coordinator
/// sends an older source the Esc it used before.
pub const PROTOCOL_VERSION: u32 = 22;
pub const NATIVE_OWNER_MESSAGES_PROTOCOL: u32 = 19;
pub const NATIVE_STOP_PROTOCOL: u32 = 22;

/// Public Stop is never permission to end a session, including with `--yes`.
pub const STOP_REFUSED_NOTICE: &str = "Cannot stop the service while sessions are live. Use :q to leave them running, or explicit Close to end a chosen session.";
/// Sent on the requesting connection before an empty service starts shutdown.
pub const STOP_ACCEPTED_NOTICE: &str = "Stopping the service; no live sessions.";
/// The reply to `SetOrder` that saved it; the phone's request waits for it.
pub const ORDER_SAVED_NOTICE: &str = "Session order saved.";

/// The `code` a phone's refused answer carries when its question no longer waits here: answered, replaced, or
/// not part of this conversation. The phone then returns to its ordinary composer without reading the error's
/// words.
pub const QUESTION_STALE: &str = "question_stale";

/// An answer refused because its question no longer waits ([`QUESTION_STALE`]).
#[derive(Debug)]
pub struct QuestionStale(pub String);

impl std::fmt::Display for QuestionStale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for QuestionStale {}

/// Whether `error` is, or wraps, a [`QuestionStale`] refusal.
pub fn is_question_stale(error: &anyhow::Error) -> bool {
    error.downcast_ref::<QuestionStale>().is_some()
}

/// A session on another host is known to the hub's decks by its id there
/// with the host's tag above this bit (`host_id`), so every message that
/// names a session keeps working unchanged; the hub's own ids stay below it.
pub const HOST_SHIFT: u32 = 40;
/// The same for the process id of an outside session: Linux process ids stay
/// below `1 << 22` (`PID_MAX_LIMIT`).
pub const HOST_PID_SHIFT: u32 = 22;
/// Tags fit the bits above `HOST_PID_SHIFT` in a `u32`.
pub const MAX_HOST_TAG: u16 = 1023;

/// The hub's id for session `id` on the host tagged `tag`.
pub fn host_id(tag: u16, id: u64) -> u64 {
    (u64::from(tag) << HOST_SHIFT) | (id & ((1 << HOST_SHIFT) - 1))
}

/// The host tag (0 for the hub itself) and that host's own id.
pub fn split_id(id: u64) -> (u16, u64) {
    ((id >> HOST_SHIFT) as u16, id & ((1 << HOST_SHIFT) - 1))
}

/// The hub's process id for outside process `pid` on the host tagged `tag`.
pub fn host_pid(tag: u16, pid: u32) -> u32 {
    (u32::from(tag) << HOST_PID_SHIFT) | (pid & ((1 << HOST_PID_SHIFT) - 1))
}

/// The host tag and that host's own process id.
pub fn split_pid(pid: u32) -> (u16, u32) {
    ((pid >> HOST_PID_SHIFT) as u16, pid & ((1 << HOST_PID_SHIFT) - 1))
}
pub const MAX_MESSAGE: usize = 32 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tool {
    Claude,
    Codex,
    Antigravity,
    /// xAI's Grok CLI (`grok`).
    Grok,
    /// The opencode CLI, which runs models from many providers.
    OpenCode,
    /// A scratch terminal: the owner's shell, or a command a script asked
    /// for (protocol 9). It has no conversation, no status of its own and
    /// never takes a broadcast or raises an alert.
    Shell,
    /// A registered provider, represented on the wire by its stable id (protocol 11).
    Dynamic(&'static str),
}

impl Tool {
    pub fn label(self) -> &'static str {
        match self {
            Tool::Claude => "Claude",
            Tool::Codex => "Codex",
            Tool::Antigravity => "Antigravity",
            Tool::Grok => "Grok",
            Tool::OpenCode => "opencode",
            Tool::Shell => "Shell",
            Tool::Dynamic(id) => crate::providers::label(id),
        }
    }

    /// Whether this is an agent, with a conversation and turns, rather than
    /// a scratch terminal.
    pub fn is_agent(self) -> bool {
        self != Tool::Shell
    }

    /// The name a command line or a script uses for it.
    pub fn key(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Antigravity => "agy",
            Tool::Grok => "grok",
            Tool::OpenCode => "opencode",
            Tool::Shell => "shell",
            Tool::Dynamic(id) => id,
        }
    }

    /// The program a command line or a script names: its key, its label or,
    /// for Antigravity, `antigravity`.
    pub fn parse(name: &str) -> Option<Tool> {
        Self::builtin(name).or_else(|| crate::providers::parse(&name.trim().to_ascii_lowercase()))
    }

    pub fn builtin(name: &str) -> Option<Tool> {
        let name = name.trim().to_ascii_lowercase();
        ALL_TOOLS.into_iter().find(|t| t.key() == name || t.wire_key() == name).or(match name.as_str() {
            "antigravity" | "gemini" => Some(Tool::Antigravity),
            "terminal" | "term" | "sh" => Some(Tool::Shell),
            _ => None,
        })
    }

    /// Existing JSON strings remain unchanged, including antigravity rather than agy.
    pub fn wire_key(self) -> &'static str {
        if self == Tool::Antigravity {
            "antigravity"
        } else {
            self.key()
        }
    }

    /// The protocol version a service needs to start this program, and a
    /// deck to read a session of it.
    pub fn since_version(self) -> u32 {
        match self {
            Tool::Claude | Tool::Codex => 1,
            Tool::Antigravity => 2,
            Tool::Grok | Tool::OpenCode => 4,
            Tool::Shell => 9,
            Tool::Dynamic(_) => 11,
        }
    }
}

impl Serialize for Tool {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.wire_key())
    }
}

impl<'de> Deserialize<'de> for Tool {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        // Wire IDs are canonical: aliases belong to the CLI, not persistence.
        if let Some(tool) = ALL_TOOLS.into_iter().find(|tool| tool.wire_key() == id) {
            return Ok(tool);
        }
        if !crate::providers::valid_id(&id) || Self::builtin(&id).is_some() {
            return Err(serde::de::Error::custom("invalid provider id"));
        }
        crate::providers::intern(&id).map(Tool::Dynamic).map_err(serde::de::Error::custom)
    }
}

/// Every program the deck hosts, agents first.
pub const ALL_TOOLS: [Tool; 6] =
    [Tool::Claude, Tool::Codex, Tool::Antigravity, Tool::Grok, Tool::OpenCode, Tool::Shell];

/// Longest session name the deck keeps (`ClientMsg::Rename`), in characters.
pub const MAX_NAME: usize = 80;

/// Most native starts (Codex, opencode) a service has in flight at once.
pub const MAX_PENDING_STARTS: usize = 8;

/// Largest requested cohort and bounded native-start backlog. Preparation
/// remains limited separately so a batch does not multiply backend load.
pub const MAX_START_BATCH: usize = 256;

/// How a message names a session: its program and title, or the program
/// alone while it has no title, as the list shows it. The numbers the list
/// shows are places that change as sessions come and go, and the service's
/// ids are numbers the owner never sees, so a notice or an error that
/// outlives the moment names the session by what it is about.
pub fn session_label(tool: Tool, title: &str) -> String {
    let title = crate::util::one_line(title);
    if title.is_empty() || title == tool.label() {
        return tool.label().to_string();
    }
    format!("{} \u{201c}{}\u{201d}", tool.label(), crate::util::truncate_width(&title, 40))
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ClientRole {
    Tui,
    Control,
    Hook,
    /// The phone gateway (`cepheus remote`, protocol 5): it also receives
    /// the deck's alerts.
    Remote,
    /// The hub's own connection to this host's service (protocol 7): it
    /// receives the state and the alerts, which the hub shows as its own.
    Peer,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ScrollOp {
    Lines(i32),
    PageUp,
    PageDown,
    Top,
    Bottom,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Local authenticated native PermissionRequest hook (protocol 16).
    PermissionRequest {
        session: u64,
        spawned_ms: u64,
        token: String,
        payload: serde_json::Value,
    },
    Fleet {
        request: u64,
        action: String,
        args: serde_json::Value,
    },
    PluginList {
        request: u64,
    },
    Plugin {
        request: u64,
        plugin: String,
        command: String,
        args: serde_json::Value,
    },
    Hello {
        version: u32,
        role: ClientRole,
    },
    /// Stream frames of one session (or none) sized for the client's pane.
    Attach {
        id: Option<u64>,
        cols: u16,
        rows: u16,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Resize only this live shell, without attaching or changing another pane
    /// (protocol 10). Replies ScreenText after applying the size.
    ResizeTerminal {
        id: u64,
        cols: u16,
        rows: u16,
    },
    Input {
        id: u64,
        data: String,
    },
    /// Correlated owner-message delivery. Protocol 19 requires the provider's
    /// native message interface and never falls back to terminal input.
    SendMessage {
        id: u64,
        request: u64,
        text: String,
        attachments: bool,
    },
    /// Replace the coordinator's shared manual session order (protocol 14).
    SetOrder {
        ids: Vec<u64>,
    },
    /// Deliver one message to each listed session through its native interface.
    Broadcast {
        /// A shared owner instruction for agents already working (protocol 14). False keeps
        /// legacy direct-message clients able to start a new turn in idle sessions.
        #[serde(default, skip_serializing_if = "is_false")]
        running_only: bool,
        ids: Vec<u64>,
        text: String,
    },
    /// One host's part of a working-agent broadcast (protocol 15).
    BroadcastPart {
        request: u64,
        /// None selects current working agents at the destination itself.
        ids: Option<Vec<u64>>,
        text: String,
    },
    Scroll {
        id: u64,
        op: ScrollOp,
    },
    Create {
        /// Opaque caller token echoed by Created; absent for older clients.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<u64>,
        tool: Tool,
        /// The model to start with, as the program's own flag names it
        /// (protocol 5). Absent: the program's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// The reasoning effort to start with (protocol 5).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        /// The host to start it on, by name (protocol 7); absent: this one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        /// The directory to start in (protocol 9): a directory on that host,
        /// such as one of its worktrees. Absent: its start directory.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// For a scratch terminal, the command to run in place of the
        /// owner's shell (protocol 9), as an argument list: never through a
        /// shell. Ignored for agents.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        command: Vec<String>,
        /// A name for it from the start, as `Rename` gives one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Close {
        id: u64,
    },
    Reopen {
        id: u64,
    },
    /// Resolve a folder on the session's host, then restart just that agent
    /// with a fresh conversation in the same deck slot (protocol 14).
    ChangeDirectory {
        id: u64,
        path: String,
    },
    Forget {
        id: u64,
    },
    Resume {
        tool: Tool,
        conversation: String,
        /// The host whose conversation it is (protocol 7); absent: this one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
    },
    MoveIn {
        tool: Tool,
        pid: u32,
        conversation: Option<String>,
    },
    ListConversations {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<u64>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        query: String,
    },
    Focus {
        focused: bool,
    },
    Hook {
        session: u64,
        tool: Tool,
        payload: serde_json::Value,
    },
    /// Ask for the plain text of a session's screen (control clients).
    Screen {
        id: u64,
        /// Bounded live scrollback, without resizing, scrolling or focusing the PTY.
        /// Older services ignore this optional field and return the visible screen.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tail: Option<usize>,
    },
    /// Read-only styled history snapshot (protocol 18), fenced to the frame
    /// the view requested. Every subsequent scroll step stays in that view.
    History {
        id: u64,
        request: u64,
        epoch: u64,
        cols: u16,
        rows: u16,
    },
    /// Stop an empty service; refuse if any local or remote session is live.
    Shutdown,
    /// Start (`run`) or stop the development server (protocol 2).
    DevServer {
        run: bool,
    },
    /// Run the service's program again in its own process, every session
    /// carrying on (protocol 3).
    Restart,
    /// Empty the service's error list for every deck (protocol 4).
    ClearErrors,
    /// Propagate a clear without creating another clear event (protocol 14).
    ClearErrorsThrough {
        clears: std::collections::BTreeMap<String, u64>,
    },
    /// Name the session's current chat, over the title the deck draws from
    /// it; `None` or a blank name goes back to that title (protocol 6). A new
    /// chat (`/clear`) drops the name, as it drops the drawn title.
    Rename {
        id: u64,
        name: Option<String>,
    },
    /// Answer the question or approval a session waits on with option
    /// `choice` (from 0), and `text` for a free-text option, but only while
    /// the session has had `inputs` inputs and shows the prompt whose
    /// fingerprint is `prompt` (`prompt.rs`; protocol 5). The service replies
    /// `Answered`. `choices`, when not empty, answers a several-choice
    /// question instead: the options to leave ticked (`choice` is unused).
    /// An older service ignores it and refuses such a question.
    Answer {
        id: u64,
        inputs: u64,
        prompt: String,
        choice: usize,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        choices: Vec<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Press Claude Code's send-now key in session `id`, so the messages
    /// queued behind its running turn reach the model now instead of when
    /// the turn ends (protocol 10). A message whose Enter is still due gets
    /// the key just after that Enter. The service replies `SentNow`.
    SendNow {
        id: u64,
    },
    /// The owner's Stop for session `id` (protocol 22): a Claude session
    /// whose owner input plugin can stop a turn ends it through Claude's own
    /// interface, which keeps the owner's queued messages; any other session
    /// gets the terminal's Esc, as `Input` would send it.
    Stop {
        id: u64,
    },
    /// `cepheus hook owner-input listen` (local Hook client): serve the owner
    /// messages of the hosted Claude session this process runs under, whose
    /// conversation Claude names `conversation`. The service sends
    /// `OwnerInput`, or a `Notice` error and closes the connection. `stop`:
    /// the plugin also ends a turn when handed `OwnerStop`.
    OwnerInputListen {
        conversation: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stop: bool,
    },
    /// `cepheus hook owner-input receipt` (local Hook client): what Claude's
    /// prompt interface answered for one `OwnerInput`. The service replies
    /// with a `Notice`, an error when it refuses the receipt.
    OwnerInputReceipt {
        message: String,
        outcome: OwnerInputOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Keep `data` (base64) as a file named after `name` on the machine the
    /// session runs on and paste its path there, as typing would (protocol
    /// 8). The deck sends its own clipboard's image this way to a session on
    /// another host, whose program would read that host's clipboard; Claude
    /// Code and Codex attach an image whose path is pasted.
    PasteFile {
        id: u64,
        name: String,
        data: String,
    },
    /// The panels this deck's scripts publish for every client, the phone's
    /// stats screen among them (protocol 9); each deck's replace the ones it
    /// sent before, and go when it disconnects.
    Panels {
        panels: Vec<Panel>,
    },
    /// Install a plugin, MCP server or skill for agents on a host
    /// (protocol 9); the host replies `Installed`.
    Install {
        request: InstallRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
    },
    /// Run a Codex Security scan of a host's start directory in a scratch
    /// worktree that is removed when the scan is saved (protocol 9).
    SecurityScan {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        /// A path inside the repository to scan alone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    /// Link the deck's agent skills into every agent's skill folder on each
    /// host again (protocol 9); the host replies with a notice.
    SyncSkills,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
// Snapshots remain inline to preserve the existing native API.
#[allow(clippy::large_enum_variant)]
pub enum ServerMsg {
    /// Every requested recipient has resolved; IDs belong to the receiving host.
    BroadcastDone {
        request: u64,
        sent: Vec<u64>,
        failed: Vec<(u64, String)>,
    },
    /// Exactly one native hook invocation; never a standing permission.
    PermissionDecision {
        request: String,
        allow: bool,
    },
    /// The preceding clear state has been applied and published (protocol 14).
    ErrorsCleared,
    FleetResult {
        request: u64,
        data: serde_json::Value,
        error: Option<String>,
    },
    /// Acknowledges exactly one SendMessage after admission/submission.
    MessageSent {
        id: u64,
        request: u64,
        error: Option<String>,
        /// What the provider confirmed for a delivered message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivered: Option<Delivered>,
    },
    PluginResult {
        request: u64,
        data: serde_json::Value,
        error: Option<String>,
    },
    Welcome {
        version: u32,
        pid: u32,
        start_dir: String,
    },
    State(DeckState),
    /// What changed since the last state or patch (protocol 20).
    StatePatch(StatePatch),
    Frame {
        id: u64,
        frame: ScreenFrame,
    },
    History {
        id: u64,
        request: u64,
        epoch: u64,
        cols: u16,
        rows: u16,
        snapshot: Option<crate::screen::HistorySnapshot>,
        error: Option<String>,
    },
    Created {
        id: u64,
        /// The caller token from Create, when that request supplied one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<u64>,
    },
    Conversations {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        items: Vec<Conversation>,
    },
    Clipboard {
        text: String,
    },
    Notice {
        text: String,
        error: bool,
    },
    /// An owner message for the session an `OwnerInputListen` client serves:
    /// submit `text` into `conversation` only.
    OwnerInput {
        message: String,
        conversation: String,
        text: String,
        /// Files the owner attached, checked on this host: the plugin tells
        /// Claude to open them. `text` already names them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
        /// Deliver it into the running main turn at the next result of a tool
        /// the model called, not as a turn of its own after it (a broadcast).
        /// An older plugin ignores it and submits the message as before.
        #[serde(default, skip_serializing_if = "is_false")]
        steer: bool,
    },
    /// The owner's Stop for the session a stop-capable `OwnerInputListen`
    /// client serves: end its running main turn, answered by a receipt.
    OwnerStop {
        message: String,
    },
    Bell {
        id: u64,
    },
    ScreenText {
        id: u64,
        lines: Vec<String>,
        /// Inputs the session has had, for a guarded `Answer` (protocol 5).
        #[serde(default)]
        inputs: u64,
    },
    /// A notification the deck raised, for `remote` clients (protocol 5):
    /// sent whether or not desktop notifications are on.
    Alert(Alert),
    /// The outcome of an `Answer`: `error` says why nothing was pressed, and
    /// `stale` that the question read is no longer the one waiting.
    Answered {
        id: u64,
        error: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stale: bool,
    },
    /// The outcome of a `SendNow`: `error` says why no key was pressed
    /// (protocol 10).
    SentNow {
        id: u64,
        error: Option<String>,
    },
    DirectoryChanged {
        id: u64,
        cwd: Option<String>,
        error: Option<String>,
    },
    /// The other hosts this hub drives, each with its use of CPU, memory and
    /// disk (protocol 7). Sent on connecting and whenever one changes.
    Hosts {
        hosts: Vec<HostInfo>,
    },
    /// What an `Install` did on host `host` (protocol 9).
    Installed {
        host: String,
        ok: bool,
        lines: Vec<String>,
    },
}

/// What to install and for which agents.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct InstallRequest {
    /// A GitHub repository (`owner/repo`, `gh:owner/repo` or its URL), a
    /// git URL, an npm package (`npm:<package>`), a directory on the host,
    /// or `mcp:<name>=<command> <args>…` / `mcp:<name>=<https url>`.
    pub source: String,
    /// `claude`, `codex`, `agy`; empty: every one the host has.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Only say what would be installed.
    #[serde(default)]
    pub dry_run: bool,
    /// Remove what this source installed instead.
    #[serde(default)]
    pub remove: bool,
}

/// A titled block of lines a deck's script publishes (`deck.ui.section`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Panel {
    pub id: String,
    pub title: String,
    pub lines: Vec<String>,
}

/// At most this many panels per deck, lines per panel, and characters per
/// line or title: a script's panels reach every client and the phone.
pub const MAX_PANELS: usize = 16;
pub const MAX_PANEL_LINES: usize = 40;
pub const MAX_PANEL_WIDTH: usize = 160;

/// Panels as the service passes them on: one line each, no control
/// characters (an escape sequence would reach every terminal), spacing kept
/// for columns, within the limits above, and no empty ones.
pub fn clean_panels(panels: Vec<Panel>) -> Vec<Panel> {
    let clip = |s: &str| {
        let line: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).take(MAX_PANEL_WIDTH).collect();
        line.trim_end().to_string()
    };
    panels
        .into_iter()
        .take(MAX_PANELS)
        .map(|p| Panel {
            id: clip(&p.id),
            title: clip(&p.title),
            lines: p.lines.iter().take(MAX_PANEL_LINES).map(|l| clip(l)).collect(),
        })
        .filter(|p| !p.id.is_empty() && (!p.title.is_empty() || !p.lines.is_empty()))
        .collect()
}

/// A branch checked out in one of a host's worktrees.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct BranchInfo {
    pub branch: String,
    /// The worktree that has it checked out.
    pub path: String,
    /// Its commit, abbreviated, the commit's subject and time (unix seconds).
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub committed_at: Option<u64>,
    /// Commits it has that the main checkout's branch lacks, and the reverse.
    #[serde(default)]
    pub ahead: Option<u32>,
    #[serde(default)]
    pub behind: Option<u32>,
    /// The main checkout itself.
    #[serde(default)]
    pub main: bool,
}

/// A Codex Security scan the deck ran or runs.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ScanInfo {
    /// Its folder's name, which is when it started.
    pub id: String,
    pub repo: String,
    pub started_at: u64,
    #[serde(default)]
    pub finished_at: Option<u64>,
    /// Where it is kept, and its report once written.
    pub dir: String,
    #[serde(default)]
    pub report: Option<String>,
    /// The session running it while it runs.
    #[serde(default)]
    pub session: Option<u64>,
    /// "running", "saved" or "failed: why".
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// A sub-agent a session started (a Claude Task agent or a Codex thread
/// spawned for one).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Subagent {
    /// What it was asked to do, or the name its program gave it.
    pub name: String,
    /// Its kind, such as Claude's agent type.
    #[serde(default)]
    pub kind: Option<String>,
    /// Still running.
    pub active: bool,
    pub started_at: u64,
    /// When it last wrote anything.
    pub last_active: u64,
}

/// Another host as the hub sees it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct HostInfo {
    /// The name sessions carry in `host` and `Create` takes.
    pub name: String,
    /// How the deck heads its sessions.
    pub label: String,
    pub online: bool,
    /// Why it is not online: unreachable, refused, an older build.
    #[serde(default)]
    pub reason: Option<String>,
    /// Its start directory, where new sessions start.
    #[serde(default)]
    pub start_dir: String,
    #[serde(default)]
    pub stats: Option<HostStats>,
    /// A build host is listed for its resources only, never offered for a
    /// session.
    #[serde(default, skip_serializing_if = "crate::config::HostRole::is_agents")]
    pub role: crate::config::HostRole,
}

/// A host's resources and repository, as its sidebar row shows them.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct HostStats {
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    pub disk_used: Option<u64>,
    pub disk_available: Option<u64>,
    /// Linked worktrees beside its main checkout (`gitwt::sibling_worktrees`).
    pub worktrees: usize,
    /// The branch its start directory has checked out and its commit.
    pub branch: Option<String>,
    pub head: Option<String>,
    /// Every branch checked out in its worktrees (protocol 9).
    #[serde(default)]
    pub branches: Vec<BranchInfo>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    Finished,
    NeedsYou,
    Error,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Alert {
    pub kind: AlertKind,
    /// The session, absent for a service error.
    pub id: Option<u64>,
    pub title: String,
    /// What the session waits for, or the error's text.
    pub reason: String,
    /// A deck window had focus when it was raised: the owner is at the desk.
    pub desk_focused: bool,
    /// The phone's identity of the owner message an error is about: the
    /// provider refused it after admitting it, and nothing was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Working,
    NeedsYou,
    Finished,
    Idle,
    Ended,
}

/// What a Codex session needs you for when its agent asked while its turn
/// runs on (`status::ScreenSignals::queued_question`). Only Codex's own key
/// opens those questions, so typing into it still sends a message.
pub const QUEUED_QUESTION: &str = "queued question";

/// What a provider's own message interface confirmed for one message.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Delivered {
    /// In the provider's queue; it runs after the agent's current turn.
    Queued,
    /// The provider admitted input into the active turn.
    Steered,
    /// The provider started a turn with it.
    Started,
    /// Handed to the provider, which did not confirm that a turn started.
    Submitted,
}

/// What Claude's prompt interface answered for one owner message
/// (`$.prompt.submit`, through the `cepheus-owner-input` plugin).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OwnerInputOutcome {
    /// Handed to Claude, which starts it as a turn of its own once idle, or
    /// held for the running turn's next tool result when it is steered.
    Submitted,
    /// Its turn started.
    Started,
    /// A steered message joined the running main turn with a tool result.
    Steered,
    /// Claude did not take it (`{ drop }`).
    Dropped,
    /// The call failed.
    Failed,
    /// The session is no longer in the addressed conversation; not submitted.
    Moved,
    /// An `OwnerStop` ended the running main turn.
    Stopped,
    /// An `OwnerStop` found no main turn running.
    Idle,
}

impl Delivered {
    pub fn label(self) -> &'static str {
        match self {
            Delivered::Queued => "queued",
            Delivered::Steered => "steered",
            Delivered::Started => "started",
            Delivered::Submitted => "submitted",
        }
    }
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Starting => "Starting",
            Status::Working => "Working",
            Status::NeedsYou => "Needs you",
            Status::Finished => "Finished",
            Status::Idle => "Idle",
            Status::Ended => "Ended",
        }
    }
}

pub const MAX_SESSION_ORDER: usize = 10_000;

/// Declares the deck state and its patch from one field list, so every
/// field the state gains is carried by a patch too.
macro_rules! deck_state {
    (
        $(#[$meta:meta])*
        pub struct DeckState {
            $( $(#[$field_meta:meta])* pub $field:ident : $ty:ty, )*
        }
    ) => {
        $(#[$meta])*
        pub struct DeckState {
            $( $(#[$field_meta])* pub $field: $ty, )*
        }

        /// The top-level parts of the deck state that differ from the state
        /// a deck already holds (protocol 20), each whole; a part that did
        /// not change is absent.
        #[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
        pub struct StatePatch {
            $(
                #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )*
        }

        impl DeckState {
            /// What differs in `self` from `base`.
            pub fn diff(&self, base: &DeckState) -> StatePatch {
                StatePatch { $( $field: (self.$field != base.$field).then(|| self.$field.clone()), )* }
            }

            /// The names of the top-level fields that differ from `base`.
            pub fn changed(&self, base: &DeckState) -> Vec<&'static str> {
                let mut out = Vec::new();
                $( if self.$field != base.$field { out.push(stringify!($field)); } )*
                out
            }

            /// Take on the parts a patch carries.
            pub fn apply(&mut self, patch: StatePatch) {
                $( if let Some(value) = patch.$field { self.$field = value; } )*
            }
        }

        impl StatePatch {
            /// The names of the parts it carries.
            #[cfg(test)]
            pub fn fields(&self) -> Vec<&'static str> {
                let mut out = Vec::new();
                $( if self.$field.is_some() { out.push(stringify!($field)); } )*
                out
            }

            pub fn is_empty(&self) -> bool {
                true $( && self.$field.is_none() )*
            }
        }
    };
}

/// A patch part that is present is taken whole, even as `null`: an optional
/// part that was cleared arrives as `Some(None)`, not as unchanged.
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// The first protocol that takes `StatePatch` after its first whole state.
pub const STATE_PATCH_PROTOCOL: u32 = 20;

deck_state! {
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct DeckState {
    #[serde(default)]
    pub attention: crate::attention::Snapshot,
    #[serde(default)]
    pub attention_capable: bool,
    #[serde(default)]
    pub work_queue: crate::work_queue::Snapshot,
    #[serde(default)]
    pub work_queue_capable: bool,
    #[serde(default)]
    pub jobs: Vec<crate::job_sessions::Info>,
    #[serde(default)]
    pub job_capable: bool,
    /// The shared coordinator's complete peer inventory (protocol 14).
    #[serde(default)]
    pub hosts: Vec<HostInfo>,
    /// Coordinator-native identity/resources, independent of the viewing desktop.
    #[serde(default)]
    pub native_host: Option<HostInfo>,
    /// Launchable agent executables on this native host (protocol 14).
    #[serde(default)]
    pub available_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub host_schedulers: std::collections::BTreeMap<String, crate::scheduler::Snapshot>,
    #[serde(default)]
    pub scheduler: crate::scheduler::Snapshot,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<crate::extensions::ProcessSnapshot>,
    /// Provider capabilities on connected hosts (protocol 11), never launch commands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<crate::providers::Info>,
    pub sessions: Vec<SessionInfo>,
    /// Canonical manual order shared by every interface (protocol 14).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_order: Vec<u64>,
    pub outside: Vec<OutsideInfo>,
    pub usage: Usage,
    pub start_dir: String,
    /// The development server (protocol 2; absent from older services).
    #[serde(default)]
    pub dev_server: DevServer,
    /// The service's recent errors, oldest first (protocol 2).
    #[serde(default)]
    pub errors: Vec<LogEntry>,
    /// Per-service clear counters; independent offline clears must both survive.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub error_clears: std::collections::BTreeMap<String, u64>,
    /// What the decks' scripts publish (protocol 9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panels: Vec<Panel>,
    /// Security scans, newest first (protocol 9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scans: Vec<ScanInfo>,
    /// The start directory's branches, as `HostStats::branches` (protocol 9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<BranchInfo>,
}
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct DevServer {
    /// The command, as the button's tooltip and the error log name it.
    pub command: String,
    /// Port the command listens on, read from its last `host:port` argument.
    pub port: Option<u16>,
    /// Started from the deck and running: its process id.
    pub pid: Option<u32>,
    pub started_at: Option<u64>,
    /// Something else listens on the port.
    pub port_busy: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LogEntry {
    /// When it last happened.
    pub at: u64,
    pub text: String,
    /// Earlier occurrences of this same error since it was last cleared.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub repeats: u32,
}

impl LogEntry {
    pub fn new(at: u64, text: impl Into<String>) -> Self {
        Self { at, text: text.into(), repeats: 0 }
    }

    /// Add an occurrence of `text` at `at` to `log` as its newest entry,
    /// folding in an identical entry still listed, and keep at most `keep`.
    /// Whether it repeats an entry still listed.
    pub fn record(log: &mut Vec<LogEntry>, at: u64, text: &str, keep: usize) -> bool {
        let earlier = log.iter().position(|e| e.text == text).map(|i| log.remove(i));
        let repeat = earlier.is_some();
        let repeats = earlier.map_or(0, |e| e.repeats.saturating_add(1));
        log.push(LogEntry { at, text: text.to_string(), repeats });
        let excess = log.len().saturating_sub(keep);
        log.drain(..excess);
        repeat
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SessionInfo {
    /// Exact hosted process/conversation generation (protocol 17); zero is unknown.
    #[serde(default)]
    pub spawned_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_root: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
    pub id: u64,
    pub tool: Tool,
    pub title: String,
    pub status: Status,
    pub detail: String,
    pub pid: Option<u32>,
    pub conversation: Option<String>,
    pub worktree: Option<Worktree>,
    pub containers: Vec<ContainerInfo>,
    pub review: bool,
    pub alive: bool,
    /// Whether the deck still holds the emulator (final screen of an ended session).
    pub has_screen: bool,
    pub created_at: u64,
    pub status_since: u64,
    /// The host it runs on (protocol 7); absent: the hub itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Its sub-agents, running ones first (protocol 9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<Subagent>,
    /// What it does or did last: a tool and its target (protocol 9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// When its conversation last changed (unix seconds; protocol 9).
    #[serde(default)]
    pub last_active: u64,
    /// The directory it started in when that is not the start directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// What the deck runs it for, such as "security scan".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
}

impl SessionInfo {
    /// Whether a message typed into the session goes to its program as a
    /// message, not as the answer to a question it shows. The service skips
    /// the others when it sends a message to many sessions.
    pub fn takes_messages(&self) -> bool {
        crate::providers::takes_broadcast(self.tool)
            && (self.status != Status::NeedsYou || self.detail == QUEUED_QUESTION)
    }

    /// A broadcast may steer active work, including a queued mid-turn question,
    /// but must never start a task in an idle, finished or starting session.
    pub fn takes_broadcast(&self) -> bool {
        self.alive
            && self.takes_messages()
            && (self.status == Status::Working || self.status == Status::NeedsYou && self.detail == QUEUED_QUESTION)
    }

    /// Sub-agents still running.
    pub fn active_subagents(&self) -> usize {
        self.subagents.iter().filter(|a| a.active).count()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Worktree {
    pub path: String,
    pub branch: Option<String>,
    pub task: Option<String>,
    pub exists: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ContainerInfo {
    pub name: String,
    pub state: String,
    pub status: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct OutsideInfo {
    pub tool: Tool,
    pub pid: u32,
    pub title: String,
    pub busy: Option<bool>,
    pub cwd: String,
    pub conversation: Option<String>,
    pub started_at: u64,
    pub start_ticks: u64,
    pub kitty_pid: Option<u32>,
    pub kitty_window: Option<String>,
    pub kitty_socket: bool,
    pub workspace: Option<String>,
    pub worktree: Option<Worktree>,
    /// The host it runs on (protocol 7); absent: the hub itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// Each program's usage windows. A window labelled with a model before its
/// window (`Argon wk`, `Fable wk`) is that model's own limit; the others are
/// the program's.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub claude: Vec<UsageWindow>,
    pub codex: Vec<UsageWindow>,
    #[serde(default)]
    pub grok: Vec<UsageWindow>,
    #[serde(default)]
    pub gemini: Vec<UsageWindow>,
    /// Claude's per-model limits (`Fable wk`). They bind only that model's
    /// sessions, so they stay out of `claude`, which the scheduler's quota
    /// hold reads for every Claude session.
    #[serde(default)]
    pub claude_models: Vec<UsageWindow>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UsageWindow {
    pub label: String,
    /// Some providers report a reset without exposing the account allowance.
    pub used_percent: Option<f64>,
    pub resets_at: Option<u64>,
    pub updated_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Conversation {
    /// Only query words found in saved owner prompts; never transcript contents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search_words: Vec<String>,
    pub tool: Tool,
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub open_in: Option<u64>,
    /// The host it belongs to (protocol 7); absent: the hub itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// Serialize a message with its length prefix.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = serde_json::to_vec(msg).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg))?;
    w.flush()
}

/// Read one message; `Ok(None)` on a clean end of stream.
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
#[path = "tests/protocol.rs"]
mod tests;
