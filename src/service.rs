//! The background service. It owns every hosted session (pseudo-terminal,
//! child process and emulator), keeps the durable registry, tracks status, and
//! serves TUI and helper clients over a private Unix socket.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::Rgb;
use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config::{Config, HostConfig, Palette};
use crate::devserver::{self, DevProc, Ended};
use crate::handover::{self, BinaryId, DevHandover, Handover, SessionHandover};
use crate::monitor::{self, HostedRef, MonitorConfig, Report, SessionFacts};
use crate::protocol::{
    encode, read_msg, session_label, Alert, AlertKind, BranchInfo, ClientMsg, ClientRole, Conversation, DeckState,
    DevServer, LogEntry, OutsideInfo, Panel, QuestionStale, ScanInfo, ServerMsg, SessionInfo, Status, Tool, Usage,
    MAX_NAME, PROTOCOL_VERSION, STATE_PATCH_PROTOCOL,
};
use crate::providers::launch;
use crate::registry::{Entry, Registry};
use crate::session::{self as pty, AdoptSpec, Hosted, SessionEvent, SpawnSpec};
use crate::status::{self, Notify, Tracker};
use crate::titles::Titles;
use crate::{antigravity, claude, codex, external, other, paths, procs, prompt, remote, screen, util};

#[path = "../plugins/agent-extensions/service.rs"]
mod agent_extensions;
#[path = "attention/service.rs"]
mod attention;
#[path = "attention/decisions.rs"]
mod attention_decisions;
#[path = "attention/native.rs"]
mod attention_native;
#[path = "attention/supervisor.rs"]
mod attention_supervisor;
mod broadcasts;
#[path = "../plugins/providers/claude_input.rs"]
mod claude_input;
mod conversations;
#[path = "../plugins/dev-services/native.rs"]
mod dev_services;
mod directory;
mod errors;
mod extensions;
mod federation;
#[path = "attention/input.rs"]
mod guarded_input;
#[path = "job_sessions/service.rs"]
mod job_sessions;
mod jobs;
mod messaging;
mod native_clear;
mod native_new;
mod program_restart;
#[path = "../plugins/providers/placement.rs"]
mod provider_placement;
pub(crate) mod recovery;
mod registry_writer;
mod resume;
#[path = "scheduler/service.rs"]
mod scheduler_service;
#[path = "../plugins/security-scans/service.rs"]
mod security_scans;
#[path = "work_queue/idle.rs"]
mod work_idle;
#[path = "work_queue/service.rs"]
mod work_queue;
#[path = "work_queue/receipts.rs"]
mod work_receipts;

pub enum Ev {
    /// A client writer has finished its frame and can accept another one.
    FrameReady,
    NativeNewPrepared {
        nonce: u64,
        result: Result<Box<native_new::Prepared>, String>,
    },
    NativeNewReady {
        client: u64,
        id: u64,
        request: Option<u64>,
        nonce: u64,
        result: Result<(), String>,
    },
    NativePeer {
        client: u64,
        endpoint: attention_native::Endpoint,
    },
    NativePrepared {
        client: u64,
        prepared: Box<attention_native::Prepared>,
    },
    NativeDone {
        client: u64,
        session: u64,
        rule: Option<String>,
        written: bool,
    },
    SupervisorDone {
        session: u64,
        rule: String,
        written: bool,
    },
    MessageDelivered {
        id: u64,
        batch: u64,
        result: crate::providers::messages::Delivery,
    },
    /// The files of an owner message for Claude, checked off the event loop:
    /// each still the owner's upload, holding the bytes the gateway hashed.
    ClaudeFilesChecked {
        id: u64,
        batch: u64,
        result: Result<Vec<PathBuf>, String>,
    },
    PluginJob(crate::lua::JobDone),
    PluginReply {
        client: u64,
        request: u64,
        data: Value,
        error: Option<String>,
    },
    ClientNew {
        id: u64,
        out: Sender<Out>,
        frame_busy: Arc<AtomicBool>,
    },
    Client(u64, ClientMsg),
    ClientGone(u64),
    Session(SessionEvent),
    Monitor(Box<Report>),
    Conversations(u64, String, Option<u64>, Vec<Conversation>),
    MoveInDone {
        client: u64,
        tool: Tool,
        conversation: String,
        error: Option<String>,
    },
    // News from another host's link (`hosts/link.rs`).
    Host(Box<crate::hosts::link::HostEv>),
    /// An install a deck asked for ran (`installer.rs`): its report for that deck.
    Installed {
        client: u64,
        ok: bool,
        lines: Vec<String>,
    },
    /// A scan's scratch worktree was made, or could not be (`jobs.rs`).
    ScanReady {
        client: u64,
        scan: jobs::ScanJob,
        error: Option<String>,
    },
    /// Work on a thread of its own ended (skills linked, a scan saved):
    /// what to tell one deck, or every deck and the F2 list.
    Notice {
        client: Option<u64>,
        text: String,
        error: bool,
    },
    /// The start directory's branches, read on a thread of its own.
    Branches(Vec<BranchInfo>),
    /// A session-list write handed to the registry writer finished.
    RegistrySaved(Result<(), String>),
    /// The start checkout's root-commit identity, read again on a thread.
    RepositoryIdentity(Option<(String, PathBuf)>),
    /// An api-cost collector pass ended (`plugins/api-cost/service.rs`).
    ApiCost(Result<Value, String>),
}

pub enum Out {
    Msg(Vec<u8>),
    Frame(Vec<u8>),
}

pub struct ServiceOptions {
    pub socket: PathBuf,
    pub state_dir: PathBuf,
    pub config: Config,
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
    pub antigravity_dir: PathBuf,
    /// Grok's home and opencode's database, for their titles and the
    /// conversations `Alt+r` and the phone resume.
    pub grok_home: PathBuf,
    pub opencode_db: PathBuf,
    pub usage_file: PathBuf,
    /// Path of this executable, used for the hooks passed to hosted programs.
    pub exe: PathBuf,
    pub start_dir: PathBuf,
    pub monitor: bool,
    pub signals: bool,
    /// Run a replaced binary in place, keeping every session
    /// (`handover.rs`). Off in tests, which share one process.
    pub update: bool,
    /// State left by the previous build of this service, to take over.
    pub handover: Option<PathBuf>,
    /// The config file, read again for `remote` so the phone gateway starts
    /// once the owner turns it on; none in tests.
    pub config_file: Option<PathBuf>,
}

impl ServiceOptions {
    pub fn from_env(config: Config) -> Result<Self> {
        let start_dir = config.resolve_start_dir()?;
        Ok(ServiceOptions {
            socket: paths::socket_path(),
            state_dir: paths::state_dir(),
            claude_dir: paths::claude_dir(),
            codex_dir: paths::codex_dir(),
            antigravity_dir: paths::antigravity_dir(),
            grok_home: paths::grok_home(),
            opencode_db: paths::opencode_db(),
            usage_file: paths::usage_file(),
            exe: current_exe_clean(),
            start_dir,
            config,
            monitor: true,
            signals: true,
            update: true,
            handover: None,
            config_file: Some(paths::config_path()),
        })
    }
}

/// This executable's path, without the marker Linux adds after it is replaced.
pub fn current_exe_clean() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cepheus"));
    let text = exe.to_string_lossy();
    match text.strip_suffix(" (deleted)") {
        Some(path) => PathBuf::from(path),
        None => exe,
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

const FRAME_INTERVAL: Duration = Duration::from_millis(12);
const STATUS_INTERVAL: Duration = Duration::from_millis(250);
const SCREEN_SCAN_INTERVAL: Duration = Duration::from_millis(500);
/// How long a Codex screen shows a new chat's welcome card before the
/// session is taken to be in a new chat, so a redraw passing through it
/// never counts.
const NEW_CHAT_HOLD: Duration = Duration::from_millis(1500);
/// How long after `/clear` or `/new` is submitted Codex's start screen counts
/// as the new chat. Codex refuses them while a turn runs, and keeps the chat.
const NEW_CHAT_CONFIRM: Duration = Duration::from_secs(5);
const SYNC_CHECK_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_WAIT: Duration = Duration::from_millis(100);
/// The periodic upkeep runs at most this often. Output, input and finished
/// frames wake the loop far more often; each upkeep step keeps its own pace.
const UPKEEP_INTERVAL: Duration = Duration::from_millis(50);
/// How often ended agent clients' temporary folders are looked for, besides
/// shortly after each hosted client ends (`client_temp.rs`).
const CLIENT_TEMP_SWEEP_INTERVAL: Duration = Duration::from_secs(600);
/// How long after a hosted client ends its folder is looked at: long enough
/// for the hooks and servers it started to exit too.
const CLIENT_TEMP_AFTER_EXIT: Duration = Duration::from_secs(5);
const PALETTE_RELOAD_INTERVAL: Duration = Duration::from_secs(5);
/// Pause between a broadcast message's paste and its Enter, so each program
/// has taken in the paste before the key that submits it.
const SUBMIT_DELAY: Duration = Duration::from_millis(250);
const PASTE_POLL: Duration = Duration::from_millis(100);
const PASTE_WAIT: Duration = Duration::from_secs(30);

/// The receiver-local Claude policy binds Enter to `chat:sendNow` and unbinds `ctrl+x ctrl+s`. Send the same
/// Enter from the phone: a stray Ctrl+S could otherwise reach Claude's `chat:stash` action.
pub(crate) const SEND_NOW_KEYS: &str = "\r";

enum QueuedKey {
    Answer(String),
    SendNow,
}

impl From<String> for QueuedKey {
    fn from(key: String) -> Self {
        Self::Answer(key)
    }
}

impl From<&str> for QueuedKey {
    fn from(key: &str) -> Self {
        Self::Answer(key.into())
    }
}

/// Recovery's Enter belongs to the paste's input version, and may wait for
/// Claude to finish decoding images. Never submit newer input or wait
/// indefinitely. Messages never come this way: they use the provider's own
/// interface (`messaging.rs`).
struct PendingSubmit {
    at: Instant,
    id: u64,
    cid: u64,
    inputs: u64,
    until: Instant,
}

impl PendingSubmit {
    fn new(now: Instant, id: u64, cid: u64, inputs: u64) -> Self {
        Self { at: now + SUBMIT_DELAY, id, cid, inputs, until: now + PASTE_WAIT }
    }
}
/// Pause between the key presses of a phone's answer.
const KEY_GAP: Duration = Duration::from_millis(120);
/// How long after a phone's answer to a question its "Review your answers"
/// step is waited for, and how often the screen is looked at meanwhile.
const REVIEW_WAIT: Duration = Duration::from_secs(4);
const REVIEW_POLL: Duration = Duration::from_millis(100);
/// How often the start repository's branches and the saved scans are read.
const BRANCHES_INTERVAL: Duration = Duration::from_secs(15);
/// How often the start checkout's root-commit identity is read again, so a
/// replica the coordinator deepened registers without a restart.
const IDENTITY_INTERVAL: Duration = Duration::from_secs(60);
/// How often Antigravity's and opencode's model lists are fetched again
/// (`models::refresh`); `:launch` asks sooner (`MODEL_LISTS_ASKED`).
const MODEL_LISTS_INTERVAL: Duration = Duration::from_secs(6 * 3600);
pub(crate) const MODEL_LISTS_ASKED: Duration = Duration::from_secs(300);

/// A phone's answer to a question whose last key may open Claude Code's
/// "Review your answers" step. The phone's tap is the answer, as in Claude's
/// app, so the step is confirmed when it shows by `until` and the session
/// had no other input since the answer's own keys (`inputs`).
struct ReviewWatch {
    at: Instant,
    until: Instant,
    id: u64,
    inputs: u64,
}

/// A key waiting its turn (`keys_due`): the rest of a phone's answer, or the
/// send-now key after a message. An answer's key carries the input count the
/// session has while nothing but the answer reached it. Input from the desk
/// or anyone else in between means the rest would land in that input, so it
/// is dropped (ROOT-034).
struct PendingKey {
    key: QueuedKey,
    inputs: Option<u64>,
}
/// How often the service checks that the phone gateway runs when the config
/// asks for it.
const REMOTE_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Between starts of a gateway that is not running, and once it keeps
/// failing.
const REMOTE_RETRY: Duration = Duration::from_secs(15);
const REMOTE_RETRY_SLOW: Duration = Duration::from_secs(300);
const DEFAULT_VIEW: (u16, u16) = (120, 40);
/// Errors kept for the deck's F2 list.
const ERROR_LOG_LEN: usize = 60;
/// How often the dev server's port is looked up in the kernel's tables.
const PORT_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// How often the service looks for a replaced binary to update to.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// Between refusing new connections and handing over, events already on
/// their way (a hook's turn report, a key typed in the deck) are handled.
const UPDATE_DRAIN: Duration = Duration::from_millis(300);
/// Variables describing the outer terminal or a parent agent session, which
/// must not leak into hosted programs (see `inherited_session_var`).
const CHILD_ENV_REMOVE: &[&str] = &[
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "KITTY_LISTEN_ON",
    "KITTY_PUBLIC_KEY",
    "WINDOWID",
    "TERMINFO",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "VTE_VERSION",
    "COLUMNS",
    "LINES",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_PROJECT_DIR",
    "CLAUDE_ENV_FILE",
    "CEPHEUS_SESSION",
    "CEPHEUS_OWNER_INPUT",
    "CEPHEUS_ACCOUNT_DIRECTORY",
    "CEPHEUS_ACCOUNT_ID",
    "AGENT_DECK_ACCOUNT_DIRECTORY", // legacy-name
    "AGENT_DECK_ACCOUNT_ID",        // legacy-name
    crate::config::START_DIR_ENV,
    // Their names before the rename, which a service that handed over from
    // an earlier build still carries.
    "AGENT_DECK_SESSION",   // legacy-name
    "AGENT_DECK_START_DIR", // legacy-name
];

/// Whether an inherited variable belongs to the outer terminal or to a parent
/// Claude Code or Codex session rather than to the owner's configuration
/// (`CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `CODEX_HOME` and the like stay). A service
/// started from inside a Claude session passed `CLAUDE_CODE_CHILD_SESSION` to
/// hosted Claude, which then turned transcript saving off, and the parent's
/// messaging token must never reach another session.
pub fn inherited_session_var(name: &str) -> bool {
    if CHILD_ENV_REMOVE.contains(&name) {
        return true;
    }
    if let Some(rest) = name.strip_prefix("CLAUDE_CODE_") {
        return rest.contains("SESSION")
            || rest.starts_with("MESSAGING_")
            || rest.starts_with("BRIDGE_")
            || matches!(rest, "ENTRYPOINT" | "EXECPATH" | "SSE_PORT");
    }
    if let Some(rest) = name.strip_prefix("CODEX_") {
        return rest.contains("THREAD") || rest.contains("SESSION") || rest.starts_with("SANDBOX");
    }
    false
}

/// The inherited variables to remove from a hosted program's environment.
fn child_env_remove(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = names.filter(|n| inherited_session_var(n)).collect();
    for name in CHILD_ENV_REMOVE {
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// A session name as the service keeps it: one line, at most `MAX_NAME`
/// characters.
pub fn clean_name(name: &str) -> String {
    util::one_line(name).chars().take(MAX_NAME).collect()
}

fn withheld_submit(who: &str) -> String {
    format!(
        "{who} asked a question before its message was submitted: the message is typed there, Enter was not pressed"
    )
}

/// Whether typed input presses Enter, submitting what the session's input
/// box holds. Alt+Enter (`ESC CR`) adds a line instead, and a paste carries
/// its line breaks as LF (`keys::encode_message`).
fn submits(data: &str) -> bool {
    data.match_indices('\r').any(|(at, _)| !data[..at].ends_with('\x1b'))
}

/// The work a Claude turn that just ended left running and will be woken by,
/// as the session's detail names it ("a shell", "2 shells", "2 background
/// tasks"), or None: Claude Code's Stop hook lists the running and pending
/// background tasks (`background_tasks`, by type label, Claude Code
/// 2.1.310). Its own background housekeeping ("dream", "auto-mode scan",
/// "memory import") and long-lived teammates are not the turn's work.
fn background_work(payload: &Value) -> Option<String> {
    const TURN_WORK: [(&str, &str, &str); 6] = [
        ("shell", "a shell", "shells"),
        ("monitor", "a monitor", "monitors"),
        ("subagent", "a sub-agent", "sub-agents"),
        ("workflow", "a workflow", "workflows"),
        ("MCP task", "an MCP task", "MCP tasks"),
        ("cloud session", "a cloud session", "cloud sessions"),
    ];
    let tasks = payload.get("background_tasks")?.as_array()?;
    let work: Vec<_> = tasks
        .iter()
        .filter_map(|t| {
            let field = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or("");
            if !matches!(field("status"), "running" | "pending") {
                return None;
            }
            TURN_WORK.iter().find(|(kind, ..)| *kind == field("type"))
        })
        .collect();
    let (kind, one, many) = work.first()?;
    Some(match work.len() {
        1 => one.to_string(),
        n if work.iter().all(|w| w.0 == *kind) => format!("{n} {many}"),
        n => format!("{n} background tasks"),
    })
}

/// Whether a client is sent what changed in the state after its first whole
/// one. A hub reading another host's state (`Peer`) merges whole states.
fn takes_patches(role: ClientRole, version: u32) -> bool {
    version >= STATE_PATCH_PROTOCOL && !matches!(role, ClientRole::Peer | ClientRole::Hook)
}

/// The deck state as a deck speaking protocol `version` can read it: a deck
/// built before a program was added fails on the whole message for one
/// session of it, so those sessions are left out (an older deck stays open
/// across an update in place).
fn state_for(version: u32, state: &DeckState) -> DeckState {
    let mut state = state.clone();
    if version < PROTOCOL_VERSION {
        state.sessions.retain(|s| s.tool.since_version() <= version);
    }
    if version < 15 {
        state.attention = Default::default();
        state.attention_capable = false;
    }
    if version < 14 {
        state.jobs.clear();
        state.job_capable = false;
        state.scheduler = Default::default();
        state.host_schedulers.clear();
        state.work_queue = Default::default();
        state.work_queue_capable = false;
    }
    state
}

/// The model and reasoning effort used when starting a new session (protocol 5).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartModel {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// A model name a program's flag may take: letters, digits and `._:/@+-#`,
/// not starting with `-`, so a phone cannot slip in another flag.
pub fn valid_model_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "._:/@+-#".contains(c))
}

/// A reasoning effort word such as `high` or `xhigh`.
pub fn valid_effort(effort: &str) -> bool {
    !effort.is_empty() && effort.len() <= 16 && effort.chars().all(|c| c.is_ascii_lowercase())
}

/// Each program's own flags for a model and effort, as their `--help`
/// names them (2026-10-27: Claude Code 2.1.311, grok, agy, opencode 2.1.4).
/// opencode takes the effort as the model's `#variant`, so it needs a model
/// to carry one. Codex gets none: a `-c` override runs it embedded, without
/// its shared server's thread tools (`Config::codex_notify`, review
/// AR-20261027-339e742b7b), so the gateway picks its model with `/model`
/// once it is up.
pub use crate::providers::native::model_flags;

pub fn valid_conversation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && !id.starts_with('-')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether an ended session has a conversation a reopen resumes. Claude is
/// given its id at the start and writes the transcript only once it has a
/// conversation; Codex's and Antigravity's ids are learned from what they
/// write; Grok and opencode take theirs at the start. A terminal starts its
/// command again, so it always has one.
fn has_conversation(s: &Sess, opts: &ServiceOptions) -> bool {
    if matches!(s.tool, Tool::Shell | Tool::Dynamic(_)) {
        return true;
    }
    let Some(id) = s.conversation.as_deref() else { return false };
    if s.tool != Tool::Claude {
        return true;
    }
    let cwd = s.cwd.as_deref().unwrap_or(&opts.start_dir).to_string_lossy();
    let claude_root = s.account_root.as_deref().unwrap_or(&opts.claude_dir);
    if claude::transcript_path(claude_root, &cwd, id).is_file() {
        return true;
    }
    // A session moved in from another folder keeps its own project.
    let name = format!("{id}.jsonl");
    fs::read_dir(claude_root.join("projects")).into_iter().flatten().flatten().any(|e| e.path().join(&name).is_file())
}

/// The last lines a program left on its screen, as one line: why it ended.
fn last_words(h: &Hosted) -> Option<String> {
    let lines = screen::screen_text(&h.lock().term);
    let shown: Vec<&str> = lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    let text = shown[shown.len().saturating_sub(LAST_WORDS_LINES)..].join(" ");
    Some(util::truncate_width(&util::one_line(&text), LAST_WORDS_WIDTH)).filter(|t| !t.is_empty())
}

/// How much of a failed start's screen its error keeps.
const LAST_WORDS_LINES: usize = 3;
const LAST_WORDS_WIDTH: usize = 200;

pub fn run(mut opts: ServiceOptions) -> Result<()> {
    let socket_dir = opts.socket.parent().context("the socket path has no parent directory")?.to_path_buf();
    util::ensure_private_dir(&socket_dir)?;
    util::ensure_private_dir(&opts.state_dir)?;
    let handover = match opts.handover.take() {
        Some(path) => Some(Handover::take(&path)?),
        None => None,
    };
    let (lock, listener) = match &handover {
        // The previous build of this process passed both on through exec.
        Some(h) => {
            pty::set_cloexec(h.lock_fd, true)?;
            pty::set_cloexec(h.listener_fd, true)?;
            // SAFETY: the previous build handed these two descriptors to this
            // build alone; nothing else in the process owns them.
            unsafe { (fs::File::from_raw_fd(h.lock_fd), UnixListener::from_raw_fd(h.listener_fd)) }
        }
        None => {
            let lock = acquire_lock(&opts.socket.with_extension("lock"))?;
            if opts.socket.exists() {
                if UnixStream::connect(&opts.socket).is_ok() {
                    bail!("a Cepheus service is already listening on {}", opts.socket.display());
                }
                let _ = fs::remove_file(&opts.socket);
            }
            let listener =
                UnixListener::bind(&opts.socket).with_context(|| format!("binding {}", opts.socket.display()))?;
            fs::set_permissions(&opts.socket, fs::Permissions::from_mode(0o600))?;
            (lock, listener)
        }
    };
    let fds = Fds { listener: listener.as_raw_fd(), lock: lock.as_raw_fd() };
    if opts.signals {
        // SAFETY: the handler only stores to an atomic flag.
        unsafe {
            libc::signal(libc::SIGTERM, on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
    }
    let (tx, rx) = mpsc::channel();
    let accept_paused = Arc::new(AtomicBool::new(false));
    {
        let tx = tx.clone();
        let paused = accept_paused.clone();
        thread::Builder::new().name("accept".into()).spawn(move || accept_loop(listener, tx, paused))?;
    }
    let mut service = Service::new_inner(opts, tx.clone(), handover.is_some());
    service.fds = Some(fds);
    service.accept_paused = accept_paused;
    match handover {
        Some(handover) => {
            service.adopt(handover);
            // Every program kept running: none needs starting again.
            for entry in &mut service.registry.sessions {
                if service.sessions.get(&entry.id).is_some_and(|s| s.alive) {
                    entry.resume = false;
                }
            }
            service.registry_dirty = true;
            service.save_registry();
        }
        None => service.resume_after_start(),
    }
    if let Err(err) = crate::install::link_renamed_peer(&current_exe_clean()) {
        service.log_error(&format!("Could not expose the renamed peer command: {err:#}"));
    }
    extensions::native::initialize(&mut service);
    if let Some(error) = crate::owner_env::unsettled() {
        service.announce_error(error);
    }
    if service.opts.monitor {
        let quotas = crate::providers::quotas::Refresh::start(
            [
                (Tool::Antigravity, service.opts.config.antigravity_command.clone()),
                (Tool::Grok, service.opts.config.grok_command.clone()),
                (Tool::Claude, service.opts.config.claude_command.clone()),
            ]
            .into_iter()
            .filter(|(tool, _)| crate::providers::enabled(*tool))
            .collect(),
            service.opts.start_dir.clone(),
            &service.usage,
        );
        service.quotas = Some(quotas.clone());
        monitor::spawn(
            MonitorConfig {
                quotas: Some(quotas),
                claude_dir: service.opts.claude_dir.clone(),
                codex_dir: service.opts.codex_dir.clone(),
                antigravity_dir: service.opts.antigravity_dir.clone(),
                grok_home: service.opts.grok_home.clone(),
                opencode_db: service.opts.opencode_db.clone(),
                start_dir: service.opts.start_dir.clone(),
                usage_file: service.opts.usage_file.clone(),
                service_pid: std::process::id(),
                codex_notify: service.opts.config.codex_notify,
                conventions: service.opts.config.conventions.clone(),
                desktop: service.opts.config.plugin_enabled("desktop"),
            },
            service.hosted_refs.clone(),
            tx,
        );
    }
    service.run_loop(rx);
    let _ = fs::remove_file(&service.opts.socket);
    drop(lock);
    Ok(())
}

/// The listening socket and the lock file, passed on when updating in place.
#[derive(Clone, Copy)]
struct Fds {
    listener: RawFd,
    lock: RawFd,
}

fn acquire_lock(path: &Path) -> Result<fs::File> {
    let file = fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(path)?;
    // SAFETY: flock on a file descriptor we own.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!("another cepheus service holds {}", path.display());
    }
    Ok(file)
}

fn peer_is_owner(stream: &UnixStream) -> bool {
    let mut cred: libc::ucred = libc::ucred { pid: 0, uid: u32::MAX, gid: u32::MAX };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    // SAFETY: getuid has no preconditions.
    rc == 0 && cred.uid == unsafe { libc::getuid() }
}

/// Accept deck, control and hook connections. While `paused` (an update is
/// about to hand over), new connections wait in the socket's queue, which
/// the next build serves.
fn accept_loop(listener: UnixListener, tx: Sender<Ev>, paused: Arc<AtomicBool>) {
    let mut next_id = 1u64;
    let fd = listener.as_raw_fd();
    loop {
        if paused.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        match pty::readable(fd, Duration::from_millis(100)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        }
        if paused.load(Ordering::SeqCst) {
            continue;
        }
        let Ok((stream, _)) = listener.accept() else { continue };
        let id = next_id;
        next_id += 1;
        accept_client(stream, id, tx.clone());
    }
}

fn accept_client(stream: UnixStream, id: u64, tx: Sender<Ev>) {
    if !peer_is_owner(&stream) {
        return;
    }
    // Fire-and-forget legacy hooks can exit before accept. Their owner
    // credentials remain valid; only native approvals require a live peer.
    let endpoint = attention_native::Endpoint::from_stream(&stream);
    let Ok(mut writer) = stream.try_clone() else { return };
    let (out_tx, out_rx) = mpsc::channel::<Out>();
    let frame_busy = Arc::new(AtomicBool::new(false));
    let busy = frame_busy.clone();
    let wake = tx.clone();
    let _ = thread::Builder::new().name(format!("client-w-{id}")).spawn(move || {
        for out in out_rx {
            let (buf, frame) = match out {
                Out::Msg(b) => (b, false),
                Out::Frame(b) => (b, true),
            };
            if writer.write_all(&buf).is_err() {
                break;
            }
            if frame {
                busy.store(false, Ordering::SeqCst);
                // Wake the loop for the next frame instead of polling for it.
                let _ = wake.send(Ev::FrameReady);
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Both);
    });
    if tx.send(Ev::ClientNew { id, out: out_tx, frame_busy }).is_err() {
        return;
    }
    if let Some(endpoint) = endpoint {
        if tx.send(Ev::NativePeer { client: id, endpoint }).is_err() {
            return;
        }
    }
    let tx = tx.clone();
    let _ = thread::Builder::new().name(format!("client-r-{id}")).spawn(move || {
        let mut reader = BufReader::new(stream);
        while let Ok(Some(msg)) = read_msg::<_, ClientMsg>(&mut reader) {
            if tx.send(Ev::Client(id, msg)).is_err() {
                return;
            }
        }
        let _ = tx.send(Ev::ClientGone(id));
    });
}

struct Client {
    out: Sender<Out>,
    frame_busy: Arc<AtomicBool>,
    role: ClientRole,
    /// The protocol the client said it speaks; none until Hello.
    version: Option<u32>,
    attached: Option<u64>,
    sent_gen: u64,
    focused: bool,
    /// This client's own deck connection to each other host it used, by
    /// tag (`federation.rs`); dropping one closes it.
    upstreams: HashMap<u16, Sender<Vec<u8>>>,
    /// The newest frame from another host, waiting for the client to take
    /// the last one.
    pending_frame: Option<Vec<u8>>,
    native_endpoint: Option<attention_native::Endpoint>,
    /// The state this client holds, when it takes patches (protocol 20):
    /// the next state reaches it as what changed since this one.
    state_base: Option<Arc<DeckState>>,
}

struct Sess {
    account_root: Option<PathBuf>,
    account: Option<String>,
    id: u64,
    tool: Tool,
    hosted: Option<Hosted>,
    alive: bool,
    exit_code: Option<u32>,
    conversation: Option<String>,
    term_title: Option<String>,
    facts: SessionFacts,
    tracker: Tracker,
    created_at: u64,
    /// When the program started, or a new chat in it began: the monitor
    /// looks for its conversation from then.
    spawned_ms: u64,
    closing: Option<(Instant, u8)>,
    /// Closed from the deck: leave the deck and the registry once the
    /// program exits, instead of staying listed as ended.
    remove_when_ended: bool,
    title: String,
    /// The owner's name for the current chat (`:rename`), shown over the
    /// title drawn from it until a new chat begins.
    name: Option<String>,
    retry: AutoContinue,
    /// The screen as the status pass last read it.
    screen_read: Option<ScreenRead>,
    /// Its program has shown its own update notice (`status::update_notice`),
    /// which service plugins heard of once as `update_available`.
    update_notice: bool,
    /// Conversations the session has left (`/clear`, `/new`), which the
    /// monitor never takes for its current one.
    left: Vec<String>,
    chat: ChatWatch,
    /// Input typed into the program since the service started, counted so a
    /// phone's answer applies only to the prompt it was shown: any input in
    /// between changes the count (review AR-20261027-339e742b7b).
    inputs: u64,
    recovery: recovery::ClientRecovery,
    /// An unsubmitted edit or an unknown restored draft blocks automatic input.
    owner_draft: bool,
    /// The directory it runs in when not the start directory, a scratch
    /// terminal's command, and what the deck runs it for.
    cwd: Option<PathBuf>,
    command: Vec<String>,
    job: Option<String>,
}

/// Watching a Codex screen for a new chat.
#[derive(Default)]
struct ChatWatch {
    /// Pending structured controls belong to this conversation generation.
    controls: Vec<std::sync::Weak<AtomicBool>>,
    /// When the screen last showed a message of the session's chat (unix ms).
    seen_ms: Option<u64>,
    /// Since when it has shown a new chat's welcome card instead.
    fresh_since: Option<Instant>,
    /// When `/clear` or `/new` was submitted (and in unix ms), until Codex
    /// shows the new chat or `NEW_CHAT_CONFIRM` passes.
    asked: Option<(Instant, u64)>,
    /// The items of its status line last read (`status::codex_footer`),
    /// which name the thread it is in; kept while the composer is hidden.
    footer: Vec<String>,
    /// The conversation whose log was last looked for and when, so a log
    /// not written yet is looked for again only now and then.
    rollout_probe: Option<(String, Instant)>,
}

impl ChatWatch {
    fn control_cancel(&mut self) -> Arc<AtomicBool> {
        self.controls.retain(|control| control.strong_count() > 0);
        let cancel = Arc::new(AtomicBool::new(false));
        self.controls.push(Arc::downgrade(&cancel));
        cancel
    }

    fn cancel_controls(&self) {
        for control in self.controls.iter().filter_map(std::sync::Weak::upgrade) {
            control.store(true, Ordering::SeqCst);
        }
    }

    /// Note input typed into the session: `/clear` or `/new` submitted,
    /// typed whole with its Enter or an Enter under a composer holding it.
    fn typed(&mut self, data: &str, composer: impl FnOnce() -> Option<String>, now: Instant, now_ms: u64) {
        let Some(typed) = data.strip_suffix('\r') else { return };
        let command = if typed.is_empty() { composer() } else { Some(typed.to_string()) };
        if command.is_some_and(|c| matches!(c.trim(), "/clear" | "/new")) {
            self.cancel_controls();
            self.asked = Some((now, now_ms));
        }
    }

    /// Whether the session has started a new chat, from what its screen shows now (`status::codex_new_chat`):
    /// the start screen after `/clear` or `/new` was submitted, or after the chat was seen. A resumed Codex
    /// shows only its start screen, never the chat, and a session that has just started, or whose terminal
    /// Ctrl+L cleared, shows it too.
    fn started_new(&mut self, screen: Option<bool>, now: Instant, now_ms: u64) -> bool {
        if self.asked.is_some_and(|(at, _)| now.saturating_duration_since(at) >= NEW_CHAT_CONFIRM) {
            self.asked = None;
        }
        match screen {
            Some(true) if self.asked.is_some() => true,
            Some(false) => {
                self.seen_ms = Some(now_ms);
                self.fresh_since = None;
                false
            }
            Some(true) if self.seen_ms.is_some() => {
                now.duration_since(*self.fresh_since.get_or_insert(now)) >= NEW_CHAT_HOLD
            }
            _ => {
                self.fresh_since = None;
                false
            }
        }
    }
}

use crate::providers::Launch;

/// Where and as what a new session starts, beyond its program and model
/// (protocol 9).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Placement {
    /// Automated dispatch rechecks resource admission at the spawn boundary.
    /// Owner-requested sessions remain available to diagnose a busy machine.
    pub resource_managed: bool,
    /// Receiver-owned pinned executable for a freshly prepared native thread.
    pub native_program: Option<PathBuf>,
    pub native_environment: Vec<(String, String)>,
    pub account_root: Option<PathBuf>,
    pub account: Option<String>,
    pub launch_key: Option<String>,
    pub launch_fingerprint: Option<String>,
    pub guard: Vec<String>,
    pub chore: Option<crate::work_queue::chores::Context>,
    /// A directory in place of the start directory.
    pub cwd: Option<PathBuf>,
    /// A scratch terminal's command in place of the owner's shell.
    pub command: Vec<String>,
    /// The owner's name for it from the start.
    pub name: Option<String>,
    /// What the deck runs it for (`SessionInfo::job`).
    pub job: Option<String>,
    /// The first message for a new conversation, passed on its command line.
    pub prompt: Option<String>,
    /// Extra arguments for the program, after its own command.
    pub args: Vec<String>,
}

#[path = "../plugins/providers/automation.rs"]
pub(crate) mod provider_automation;
#[cfg(test)]
use provider_automation::*;
use provider_automation::{AutoContinue, Retry};

/// A session's screen as last read and what it said, kept until its
/// terminal changes (`Hosted::generation`).
struct ScreenRead {
    generation: u64,
    lines: Vec<String>,
    cursor: Option<(usize, usize)>,
    signals: crate::status::ScreenSignals,
    /// Codex's new-chat screen and footer (`status::codex_new_chat`, `codex_footer`).
    new_chat: Option<bool>,
    footer: Vec<String>,
    /// Whether it shows its program's update notice (`status::update_notice`).
    update_notice: bool,
}

impl ScreenRead {
    fn new(tool: Tool, generation: u64, h: &Hosted, pid: Option<u32>) -> Self {
        let (lines, cursor) = {
            let guard = h.lock();
            (screen::screen_text(&guard.term), screen::text_cursor(&guard.term))
        };
        let signals = crate::providers::screen_signals(tool, &lines, pid);
        let (new_chat, footer) = if tool == Tool::Codex {
            (status::codex_new_chat(&lines, cursor), status::codex_footer(&lines, cursor))
        } else {
            (None, Vec::new())
        };
        let update_notice = status::update_notice(tool, &lines);
        Self { generation, lines, cursor, signals, new_chat, footer, update_notice }
    }
}

impl Sess {
    /// Record that the session is in `conversation` now. Leaving another one drops what was known of it, its
    /// title and worktree among it. Returns whether it changed.
    fn enter_conversation(&mut self, conversation: Option<String>) -> bool {
        if self.conversation == conversation {
            return false;
        }
        self.chat.cancel_controls();
        if let Some(h) = &self.hosted {
            h.revoke_guarded_input();
        }
        if let Some(old) = self.conversation.take() {
            if !self.left.contains(&old) {
                self.left.push(old);
            }
            self.forget_chat();
            self.owner_draft = true;
        }
        self.left.retain(|c| Some(c) != conversation.as_ref());
        self.conversation = conversation;
        true
    }

    /// Drop what was known of the chat the session was in.
    fn forget_chat(&mut self) {
        self.chat.cancel_controls();
        self.facts = SessionFacts::default();
        // Activity, pending finishes and questions belong to the chat we left.
        // In particular, a Stop hook's background-work flag otherwise keeps a
        // cleared Claude conversation working indefinitely. Only the launch's
        // hook capability carries over to the new chat.
        self.tracker = Tracker {
            finish_hook: self.tracker.finish_hook,
            persistent: self.tracker.persistent,
            completed_turns: self.tracker.completed_turns,
            ever_active: true,
            status: if self.alive { Status::Idle } else { Status::Ended },
            ..Tracker::new(util::now_ms(), self.alive)
        };
        self.term_title = None;
        // Drawn again from the next conversation, or the tool's name. The owner's name was for the chat that
        // ended.
        self.title.clear();
        self.name = None;
        self.recovery.draft = None;
        self.recovery.pending.clear();
        self.recovery.rollout = None;
        self.recovery.offset = 0;
        self.recovery.blocked = None;
        self.recovery.captured = false;
        self.recovery.bind_from_start = false;
        self.recovery.reported_block = false;
        self.recovery.draft_files.clear();
        self.recovery.draft_submit_requested = false;
        self.recovery.draft_restored = false;
        self.recovery.draft_submitted = false;
    }

    /// Count input typed into the program, from a deck, a phone or the
    /// service's own "continue".
    fn typed_input(&mut self) {
        self.tracker.user_input();
        self.input_changed();
    }

    fn input_changed(&mut self) {
        self.inputs += 1;
        self.retry.input_revision = self.hosted.as_ref().map(|h| h.lock().output_revision);
    }

    /// Whether an automatic message may start work now. It goes through the
    /// provider's own interface, so only the session's tracked state counts:
    /// never its screen or composer. The owner's unsent draft holds it.
    pub(super) fn native_idle(&self, now_ms: u64) -> bool {
        self.alive
            && self.hosted.is_some()
            && self.closing.is_none()
            && !self.owner_draft
            && !self.recovery.paused
            && self.recovery.phase.is_none()
            && matches!(self.tracker.status, Status::Idle | Status::Finished)
            && !self.tracker.is_working(now_ms)
            && self.tracker.needs_reason().is_none()
    }

    fn automatic_retry_input_ready(&self, now_ms: u64, completed_revoked_turn: bool) -> bool {
        if self.owner_draft
            || !self.alive
            || self.recovery.paused
            || self.recovery.phase.is_some()
            || (self.tracker.is_working(now_ms) && !completed_revoked_turn)
            || self.tracker.needs_reason().is_some()
        {
            return false;
        }
        let Some(hosted) = &self.hosted else { return false };
        let guard = hosted.lock();
        self.retry.input_revision.is_none_or(|revision| guard.output_revision > revision)
            && guard.parser.sync_timeout().sync_timeout().is_none()
            && status::empty_automatic_composer(self.tool, &guard.term)
    }

    /// Note input on its way to the program, before it arrives: Codex's
    /// `/clear` and `/new` start a new chat, and its composer is read before
    /// the Enter that submits it clears it.
    fn note_typed(&mut self, data: &str) {
        if self.tool != Tool::Codex {
            return;
        }
        let hosted = &self.hosted;
        let composer = || {
            let h = hosted.as_ref()?;
            let guard = h.lock();
            status::codex_composer(&screen::screen_text(&guard.term), screen::text_cursor(&guard.term))
        };
        self.chat.typed(data, composer, Instant::now(), util::now_ms());
    }

    /// A new chat began in this session (`/clear` or `/new`). Codex
    /// starts its conversation then, so the monitor looks for it from when
    /// the command was submitted, or else the old chat was last on screen.
    fn start_new_chat(&mut self) {
        let confirmed_clear = self.chat.asked.is_some();
        if !self.enter_conversation(None) {
            self.forget_chat();
        }
        if let Some(from) = self.chat.asked.map(|(_, ms)| ms).or(self.chat.seen_ms) {
            self.spawned_ms = from;
        }
        self.chat = ChatWatch::default();
        if confirmed_clear {
            self.owner_draft = false;
        }
    }

    /// A name given to the conversation (the owner's, or a generated one the monitor still holds current), else
    /// the original task. Claude's own terminal title names its topic, and comes before the task; Codex's names
    /// the program and folder, and comes after it. The session's title. `agents` are the Claude agents defined
    /// for the start directory and the owner. A title drawn from the chat of more than four words shows as its
    /// summary once `titles` has one; a name the owner gave shows as it is.
    fn display_title(&self, agents: &[String], titles: &mut Titles) -> String {
        if self.tool == Tool::Shell {
            // A shell titles its terminal itself (oh-my-zsh: the directory).
            return self
                .name
                .clone()
                .or_else(|| self.term_title.clone())
                .unwrap_or_else(|| self.tool.label().to_string());
        }
        let f = &self.facts;
        // Claude names an agent session's terminal after the agent, which names no conversation: every hosted
        // Claude read "implementer". Before its transcript names the agent, the defined agents do; a title kept
        // from then is dropped too.
        let names_agent = |t: &str| {
            self.tool == Tool::Claude && f.agent.iter().chain(agents).any(|a| a.eq_ignore_ascii_case(t.trim()))
        };
        let term = self.term_title.clone().filter(|t| !names_agent(t));
        let task = f.first_prompt.clone().or_else(|| f.latest_prompt.clone());
        let (first, second) = if self.tool == Tool::Claude { (term, task) } else { (task, term) };
        // A name the owner gave at the deck (`:rename`) comes before every
        // title drawn from the chat.
        if let Some(name) = &self.name {
            return name.clone();
        }
        f.meta_title
            .clone()
            .or_else(|| f.ai_title.clone())
            .or(first)
            .or(second)
            .or_else(|| f.first_prompt.clone())
            .map(|text| titles.short(&text))
            .unwrap_or_else(|| {
                if self.title.is_empty() || names_agent(&self.title) {
                    self.tool.label().to_string()
                } else {
                    self.title.clone()
                }
            })
    }

    /// Whether the session is asking a question, read from its screen now.
    /// Codex asks only on screen, and the periodic scan can be
    /// `SCREEN_SCAN_INTERVAL` old: a question that opened since would take
    /// typed text or an Enter as its answer (review CR-20261026-3fdeb2041f).
    /// Queued Codex questions do not count: Enter there queues the message.
    fn asking_now(&mut self, now_ms: u64) -> bool {
        if let Some(h) = &self.hosted {
            let lines = screen::screen_text(&h.lock().term);
            self.tracker.observe_screen(&crate::providers::screen_signals(self.tool, &lines, h.pid), now_ms);
        }
        let t = &self.tracker;
        t.blocks_input() || (t.status == Status::NeedsYou && !t.screen_queued_question)
    }

    fn detail(&self) -> String {
        if let Some(reason) = &self.recovery.blocked {
            return reason.clone();
        }
        if self.recovery.phase.is_some() {
            return "reconnecting Codex; input preserved".into();
        }
        match self.tracker.status {
            Status::NeedsYou => self.tracker.needs_reason().unwrap_or("input").to_string(),
            Status::Ended => match self.exit_code {
                Some(code) if code != 0 => format!("exit {code}"),
                _ if self.hosted.is_some() => "exited".to_string(),
                _ => "not running".to_string(),
            },
            // The phone says this instead of "Thinking…".
            Status::Working => self.tracker.waiting_on().map(|work| format!("waiting on {work}")).unwrap_or_default(),
            _ => String::new(),
        }
    }
}

struct JobRoute {
    original: u64,
    host: u16,
    at: Instant,
}
#[path = "../plugins/fleet_workspace/service.rs"]
mod workspace_placement;

#[derive(Default)]
struct SavedSessionOrder {
    stamp: Option<(u64, i64, i64)>,
    ids: Vec<u64>,
}

struct Service {
    attention: Option<attention::State>,
    work_queue: Option<crate::work_queue::store::Queue>,
    job_sessions: Option<crate::job_sessions::Jobs>,
    job_routes: HashMap<(u64, u64), JobRoute>,
    scheduler: crate::scheduler::Scheduler,
    messages: messaging::State,
    extensions: extensions::Runtime,
    opts: ServiceOptions,
    palette: Palette,
    palette_at: Instant,
    tx: Sender<Ev>,
    sessions: BTreeMap<u64, Sess>,
    clients: HashMap<u64, Client>,
    registry: Registry,
    registry_path: PathBuf,
    registry_dirty: bool,
    registry_saved: Instant,
    registry_writer: registry_writer::Writer,
    /// The snapshot cadence includes attempts that find no recovery changes.
    recovery_snapshot_at: Instant,
    hosted_refs: Arc<Mutex<Vec<HostedRef>>>,
    outside: Vec<OutsideInfo>,
    usage: Usage,
    /// Reads Grok's and Gemini's allowances after their turns end.
    quotas: Option<Arc<crate::providers::quotas::Refresh>>,
    /// The state last sent to the decks.
    last_state: Arc<DeckState>,
    /// How many times `last_state` was brought up to date (`state_changed`).
    state_builds: u64,
    session_order: RefCell<SavedSessionOrder>,
    view: (u16, u16),
    shutdown_at: Option<Instant>,
    stop_jobs_on_shutdown: bool,
    last_pump: Instant,
    last_upkeep: Option<Instant>,
    last_status: Instant,
    last_scan: Instant,
    last_sync_check: Instant,
    /// When ended clients' temporary folders are next swept, and whether a
    /// sweep is running (`client_temp.rs`).
    client_temp_due: Instant,
    client_temp_busy: Arc<AtomicBool>,
    /// Broadcast messages waiting for their Enter.
    pending_submits: Vec<PendingSubmit>,
    /// The rest of a phone's answer, one key press each: (due, session, key).
    pending_keys: Vec<(Instant, u64, PendingKey)>,
    /// Phone answers whose review step may still show.
    pending_reviews: Vec<ReviewWatch>,
    /// When the phone gateway was last looked for, and last started.
    remote_checked: Option<Instant>,
    remote_started: Option<Instant>,
    /// It was seen running since it was last started.
    remote_ran: bool,
    /// Starts in a row it exited before it was seen running.
    remote_failures: u32,
    /// Recent errors for the deck's F2 list, oldest first, one entry for
    /// each distinct text.
    errors: RefCell<Vec<LogEntry>>,
    error_clears: BTreeMap<String, u64>,
    /// The Claude agents defined for the start directory and the owner,
    /// whose names are no session title.
    claude_agents: Vec<String>,
    native: extensions::native::State,
    /// The socket and lock to pass on when updating; none in unit tests.
    fds: Option<Fds>,
    accept_paused: Arc<AtomicBool>,
    update_checked: Option<Instant>,
    /// A new binary that refused or failed the handover, not tried again.
    update_failed: Option<BinaryId>,
    /// Handing over to this binary once the drain since then is over, and
    /// whether a deck asked for it as a restart.
    updating: Option<(Instant, PathBuf, BinaryId, bool)>,
    /// A deck asked for a restart: hand over to the program at the
    /// service's own path, replaced or not, as soon as nothing is mid-way.
    restart_requested: bool,
    /// Move-ins whose resume has not happened yet; an update waits for them.
    moves_in_flight: usize,
    /// The other hosts this service drives as a hub, by tag.
    hosts: BTreeMap<u16, federation::HostSlot>,
    /// Outage timers awaiting their configured links after a hub handover.
    restored_host_outages: Vec<(HostConfig, federation::HostOutage)>,
    hosts_checked: Option<Instant>,
    claude_enter_policy: crate::hosts::bootstrap::LocalPolicy,
    placement_reserved: RefCell<HashMap<Option<String>, Vec<u64>>>,
    placed_pending: HashMap<u64, provider_placement::PlacedTarget>,
    native_new_pending: HashMap<u64, native_new::Pending>,
    native_new_queue: std::collections::VecDeque<native_new::Queued>,
    launch_capabilities: RefCell<Option<(Instant, Vec<String>)>>,
    native_resources: RefCell<(Option<Instant>, crate::resources::Sampler)>,
    native_identity: String,
    /// The hub repository's ref generation the mirror links follow, and
    /// whether its watcher was started (`hosts/mirror.rs`).
    mirror_refs: Arc<std::sync::atomic::AtomicU64>,
    mirror_watching: bool,
    /// The hosts as last sent to the decks.
    last_hosts: Vec<u8>,
    /// Conversation lists being gathered from the hosts, by deck.
    pending_lists: HashMap<u64, federation::PendingList>,
    broadcasts: broadcasts::State,
    conversation_scans: conversations::Scans,
    /// When the peer listener was last looked for, and last started.
    peer_checked: Option<Instant>,
    peer_started: Option<Instant>,
    /// What each deck's scripts publish (`ClientMsg::Panels`), by deck.
    panels: BTreeMap<u64, Vec<Panel>>,
    /// Phone clears waiting on their fresh native Codex, by its New request.
    native_clears: HashMap<u64, native_clear::Clearing>,
    /// Sessions started again after the service stopped under them whose
    /// turn it cut short: told to carry on once ready (`resume.rs`).
    pending_continues: Vec<resume::PendingContinue>,
    pending_restores: Vec<resume::PendingRestore>,
    pending_directories: HashMap<u64, directory::PendingDirectory>,
    /// Sessions of the deck's own program moving onto its installed build.
    program_moves: program_restart::State,
    /// The start directory's branches, and when they were last asked for.
    branches: Vec<BranchInfo>,
    branches_asked: Option<Instant>,
    /// A branch scan is running; another is not started until it reports.
    branches_busy: bool,
    /// When the programs' model lists were last fetched, and whether a
    /// fetch is running (`models::refresh`).
    model_lists_asked: Option<Instant>,
    model_lists_busy: Arc<AtomicBool>,
    /// When the start checkout's repository identity was last read.
    identity_asked: Option<Instant>,
    /// A repository identity read is running.
    identity_busy: bool,
}

impl Service {
    #[cfg(test)]
    fn new(opts: ServiceOptions, tx: Sender<Ev>) -> Self {
        Self::new_inner(opts, tx, false)
    }

    fn new_inner(opts: ServiceOptions, tx: Sender<Ev>, upgrading: bool) -> Self {
        let error_clears = errors::load(&opts.state_dir);
        let registry_path = opts.state_dir.join("sessions.json");
        let mut registry = Registry::load(&registry_path);
        let now = util::now_secs();
        let now_ms = util::now_ms();
        let mut sessions = BTreeMap::new();
        // A scratch terminal that had ended is not kept across a stop.
        let before = registry.sessions.len();
        registry.sessions.retain(|e| !(e.tool == Tool::Shell && e.ended_at.is_some()));
        let mut dirty = registry.sessions.len() != before;
        for entry in &mut registry.sessions {
            if entry.cwd.is_none() {
                // An older list left the start directory unrecorded. Record
                // it, so a later change to that setting cannot move it. This
                // fill-in is not a change of its own: the next real change
                // saved to the list keeps it, and Stop alone never writes it.
                entry.cwd = Some(opts.start_dir.to_string_lossy().into_owned());
            }
            if entry.ended_at.is_none() {
                // The last service never saw it end: it crashed, was killed
                // or the computer lost power. Its agent carries on at the
                // next start, as after a stop by a signal (`resume.rs`).
                entry.ended_at = Some(now);
                entry.resume = entry.recoverable();
                dirty = true;
            }
            sessions.insert(
                entry.id,
                Sess {
                    account_root: entry.account_root.as_ref().map(PathBuf::from),
                    account: entry.account.clone(),
                    id: entry.id,
                    tool: entry.tool,
                    hosted: None,
                    alive: false,
                    exit_code: None,
                    conversation: entry.conversation.clone(),
                    term_title: None,
                    facts: SessionFacts::default(),
                    tracker: Tracker::new(now_ms, false),
                    created_at: entry.created_at,
                    spawned_ms: now_ms,
                    closing: None,
                    remove_when_ended: false,
                    title: entry.title.clone(),
                    name: entry.name.as_deref().map(clean_name).filter(|n| !n.is_empty()),
                    retry: AutoContinue::default(),
                    screen_read: None,
                    update_notice: false,
                    left: Vec::new(),
                    chat: ChatWatch::default(),
                    inputs: 0,
                    recovery: recovery::ClientRecovery::cold(entry.recovery.clone(), entry.owner_paused),
                    owner_draft: true,
                    cwd: entry.cwd.as_ref().map(PathBuf::from),
                    command: entry.command.clone(),
                    job: entry.job.clone(),
                },
            );
        }
        let palette = Palette::load(&opts.config);
        let claude_agents = claude::agent_names(&[opts.start_dir.join(".claude"), opts.claude_dir.clone()]);
        let native = extensions::native::State::new(&opts);
        let mut scheduling = opts.config.scheduler.clone();
        if let Some((id, root)) = crate::scheduler::repository(&opts.start_dir) {
            scheduling.add_discovered_repository(id, root);
        }
        let scheduler = crate::scheduler::Scheduler::load(scheduling, &opts.state_dir, upgrading);
        let job_sessions =
            match crate::job_sessions::Jobs::load_with_retention(&opts.state_dir, opts.config.job_retention_days) {
                Ok(jobs) => Some(jobs),
                Err(error) => {
                    eprintln!("Job recovery disabled: {error:#}");
                    None
                }
            };
        let work_queue = match crate::work_queue::store::Queue::load(&opts.state_dir) {
            Ok(q) => Some(q),
            Err(_) => {
                eprintln!("Work queue recovery required");
                None
            }
        };
        let then = Instant::now();
        let native_identity = crate::remote::auth::sha256_hex(
            format!(
                "{}\0{}\0{}",
                util::hostname(),
                String::from_utf8_lossy(&fs::read("/etc/machine-id").unwrap_or_default()),
                opts.state_dir.to_string_lossy()
            )
            .as_bytes(),
        );
        let registry_writer = registry_writer::Writer::start(registry_path.clone(), tx.clone(), None);
        Service {
            attention: match attention::State::load(&opts.state_dir) {
                Ok(a) => Some(a),
                Err(e) => {
                    eprintln!("Attention unavailable: {e:#}");
                    None
                }
            },
            work_queue,
            scheduler,
            job_sessions,
            job_routes: HashMap::new(),
            extensions: extensions::Runtime::default(),
            opts,
            palette,
            palette_at: then,
            tx,
            sessions,
            clients: HashMap::new(),
            registry,
            registry_path,
            registry_dirty: dirty,
            registry_saved: then,
            registry_writer,
            recovery_snapshot_at: then,
            hosted_refs: Arc::new(Mutex::new(Vec::new())),
            outside: Vec::new(),
            usage: Usage::default(),
            quotas: None,
            last_state: Arc::default(),
            state_builds: 0,
            session_order: RefCell::new(SavedSessionOrder::default()),
            view: DEFAULT_VIEW,
            shutdown_at: None,
            stop_jobs_on_shutdown: false,
            last_pump: then,
            last_upkeep: None,
            last_status: then,
            last_scan: then,
            last_sync_check: then,
            client_temp_due: then,
            client_temp_busy: Arc::new(AtomicBool::new(false)),
            messages: messaging::State::default(),
            pending_submits: Vec::new(),
            pending_keys: Vec::new(),
            pending_reviews: Vec::new(),
            remote_checked: None,
            remote_started: None,
            remote_ran: false,
            remote_failures: 0,
            errors: RefCell::new(Vec::new()),
            error_clears,
            claude_agents,
            native,
            fds: None,
            accept_paused: Arc::new(AtomicBool::new(false)),
            update_checked: None,
            update_failed: None,
            updating: None,
            restart_requested: false,
            moves_in_flight: 0,
            hosts: BTreeMap::new(),
            restored_host_outages: Vec::new(),
            hosts_checked: None,
            claude_enter_policy: crate::hosts::bootstrap::LocalPolicy::default(),
            placement_reserved: RefCell::new(HashMap::new()),
            placed_pending: HashMap::new(),
            native_new_pending: HashMap::new(),
            native_new_queue: std::collections::VecDeque::new(),
            launch_capabilities: RefCell::new(None),
            native_resources: RefCell::new((None, crate::resources::Sampler::default())),
            native_identity,
            mirror_refs: Arc::default(),
            mirror_watching: false,
            last_hosts: Vec::new(),
            pending_lists: HashMap::new(),
            broadcasts: broadcasts::State::default(),
            conversation_scans: conversations::Scans::default(),
            peer_checked: None,
            peer_started: None,
            panels: BTreeMap::new(),
            native_clears: HashMap::new(),
            pending_continues: Vec::new(),
            pending_restores: Vec::new(),
            pending_directories: HashMap::new(),
            program_moves: program_restart::State::default(),

            branches: Vec::new(),
            branches_asked: None,
            branches_busy: false,
            model_lists_asked: None,
            model_lists_busy: Arc::new(AtomicBool::new(false)),
            identity_busy: false,
            identity_asked: None,
        }
    }

    /// A status tracker for a newly started session of `tool`. A program
    /// that reports the end of its turns finishes only on that report.
    fn new_tracker(&self, tool: Tool, attached: bool, now_ms: u64) -> Tracker {
        let mut tracker = Tracker::new(now_ms, true);
        tracker.finish_hook = match tool {
            Tool::Claude => self.opts.config.claude_hooks,
            Tool::Codex => self.opts.config.codex_notify && !attached,
            Tool::Antigravity | Tool::Grok | Tool::OpenCode | Tool::Shell | Tool::Dynamic(_) => false,
        };
        tracker.persistent = tool == Tool::Dynamic("dot");
        tracker
    }

    /// Keep an error for the F2 list: whether the same error is listed
    /// already, uncleared.
    fn log_error(&self, text: &str) -> bool {
        LogEntry::record(&mut self.errors.borrow_mut(), util::now_secs(), text, ERROR_LOG_LEN)
    }

    /// Report an error to every deck and keep it for the F2 list. The same
    /// error again, before it was cleared, alerts the phone only once.
    fn announce_error(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !self.log_error(&text) {
            self.alert_remote(AlertKind::Error, None, "cepheus", &text);
        }
        self.broadcast(&ServerMsg::Notice { text, error: true });
    }

    fn upkeep_claude_policy(&mut self, now: Instant) {
        // Check the timer before loading configuration: the event loop must
        // not parse every profile on each output wake-up.
        if self.shutdown_at.is_some() || self.updating.is_some() || !self.claude_enter_policy.due(now) {
            return;
        }
        let loaded = match &self.opts.config_file {
            Some(path) => Config::load_from(path),
            None => (self.opts.config.clone(), None),
        };
        let paths = crate::hosts::logins::LoginPaths {
            claude_dir: self.opts.claude_dir.clone(),
            codex_dir: self.opts.codex_dir.clone(),
        };
        if let Some(error) = self.claude_enter_policy.tick(now, &paths, &loaded) {
            self.announce_error(error);
        }
    }

    fn run_loop(&mut self, rx: Receiver<Ev>) {
        self.upkeep_claude_policy(Instant::now());
        self.load_service_plugins();
        loop {
            let timeout = self.next_timeout();
            match rx.recv_timeout(timeout) {
                Ok(ev) => self.handle(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            while let Ok(ev) = rx.try_recv() {
                self.handle(ev);
            }
            if STOP.load(Ordering::SeqCst) && self.opts.signals && self.shutdown_at.is_none() {
                self.begin_shutdown(false);
            }
            let now = Instant::now();
            if self.last_upkeep.is_none_or(|at| now.duration_since(at) >= UPKEEP_INTERVAL) {
                self.last_upkeep = Some(now);
                self.housekeeping(now);
                self.scheduler_tick();
                self.jobs_tick();
                self.work_tick();
                self.attention_tick();
                self.service_plugins_tick(now);
                if self.updating.as_ref().is_some_and(|(since, ..)| now.duration_since(*since) >= UPDATE_DRAIN) {
                    self.update_now();
                }
            }
            self.messages_due();
            self.pump_native_new();
            self.submit_due(now);
            self.keys_due(now);
            self.reviews_due(now);
            if now.duration_since(self.last_pump) >= FRAME_INTERVAL {
                self.pump_frames();
                self.last_pump = now;
            }
            if now.duration_since(self.last_status) >= STATUS_INTERVAL {
                self.refresh_status(now);
                self.last_status = now;
            }
            if let Some(at) = self.shutdown_at {
                let done = self.sessions.values().all(|s| !s.alive)
                    && self.native.dev.is_none()
                    && self.extensions.processes.is_empty()
                    && (!self.stop_jobs_on_shutdown || self.job_sessions.as_ref().is_none_or(|jobs| !jobs.busy()))
                    && (!self.stop_jobs_on_shutdown || self.work_queue.as_ref().is_none_or(|q| !q.refresh_jobs.busy()))
                    && !self.service_plugins_busy()
                    && self.native.installs == 0
                    && !self.messages.busy();
                let cleanup_done = self.extensions.workers.load(Ordering::SeqCst) == 0 && !self.messages.busy();
                if done || (cleanup_done && at.elapsed() > Duration::from_secs(8)) {
                    break;
                }
            }
        }
        if let Some(dev) = self.native.dev.as_mut() {
            dev.kill();
        }
        self.extensions.processes.kill_all();
        if self.shutdown_at.is_some() {
            // A program still running ends with this process; the next
            // service must not take it for one a crash left (`Service::new`).
            let now = util::now_secs();
            for entry in self.registry.sessions.iter_mut().filter(|e| e.ended_at.is_none()) {
                entry.ended_at = Some(now);
                self.registry_dirty = true;
            }
        }
        self.save_registry();
    }

    fn next_timeout(&self) -> Duration {
        // A client whose writer is still busy is woken by `Ev::FrameReady`,
        // not polled for; a queued remote-host frame is sent promptly.
        let stale = self.clients.values().any(|c| {
            !c.frame_busy.load(Ordering::SeqCst)
                && (c.pending_frame.is_some()
                    || c.attached
                        .and_then(|id| self.sessions.get(&id))
                        .and_then(|s| s.hosted.as_ref())
                        .is_some_and(|h| h.generation() != c.sent_gen))
        });
        let wait = if stale { FRAME_INTERVAL.saturating_sub(self.last_pump.elapsed()) } else { IDLE_WAIT };
        let submit = self
            .pending_submits
            .iter()
            .map(|p| p.at)
            .chain(self.pending_keys.iter().map(|(due, ..)| *due))
            .chain(self.pending_reviews.iter().map(|w| w.at))
            .map(|due| due.saturating_duration_since(Instant::now()))
            .min();
        submit.map_or(wait, |s| s.min(wait)).max(Duration::from_millis(1))
    }

    /// Answer the prompt a session waits on, for a phone, only while it is
    /// the prompt the phone was shown: no input reached the session since
    /// (`inputs`) and its question and options are the same (`fingerprint`).
    /// The check and the first key press happen in this one step of the
    /// loop, so an answer typed at the desk meanwhile, a repeated tap or an
    /// identical later prompt is refused (review AR-20261027-339e742b7b).
    /// With `choices` the answer is the set of options to leave ticked on a
    /// several-choice question (`Prompt::keys_several`), `choice` unused.
    fn answer(
        &mut self,
        id: u64,
        inputs: u64,
        fingerprint: &str,
        choice: usize,
        choices: &[usize],
        text: Option<&str>,
    ) -> Result<()> {
        anyhow::ensure!(!self.pending_directories.contains_key(&id), "the agent is changing directory");
        let s = self.sessions.get_mut(&id).filter(|s| s.alive).context("the session is not running")?;
        let h = s.hosted.as_ref().context("the session is not running")?;
        if s.inputs != inputs {
            bail!("the session took other input since the question was read; look again");
        }
        let (lines, modes) = {
            let guard = h.lock();
            (screen::screen_text(&guard.term), screen::encode_modes(*guard.term.mode()))
        };
        let shown = prompt::parse(&lines).ok_or_else(|| QuestionStale("no question is waiting".into()))?;
        if shown.fingerprint() != fingerprint {
            return Err(QuestionStale("the question changed; look again".into()).into());
        }
        // Typed text, rather than an arrow or Enter, is given time to arrive
        // before the Enter that submits it.
        let is_text = |k: &str| k != "\r" && !(k.starts_with('\x1b') && k.len() <= 4);
        let keys = if choices.is_empty() {
            shown.keys(choice, text, modes)?
        } else {
            shown.keys_several(choices, text, modes)?
        };
        let mut keys = keys.into_iter();
        h.follow_output();
        let mut pasted = false;
        if let Some(first) = keys.next() {
            pasted = is_text(&first);
            h.write(first.into_bytes());
        }
        // This answer is the session's newest input: the rest of an earlier
        // one is not pressed into it.
        self.pending_keys.retain(|(_, who, k)| *who != id || k.inputs.is_none());
        let mut due = Instant::now();
        let mut queued = 0;
        for key in keys {
            due += if pasted { SUBMIT_DELAY } else { KEY_GAP };
            pasted = is_text(&key);
            // The first key counts one input (`typed_input` below), each
            // queued key one more as it is pressed.
            self.pending_keys.push((due, id, PendingKey { key: key.into(), inputs: Some(inputs + 1 + queued) }));
            queued += 1;
        }
        s.typed_input();
        if shown.kind == "question" {
            // Each queued key counts one input as it is pressed (`keys_due`).
            self.pending_reviews.push(ReviewWatch {
                at: due + REVIEW_POLL,
                until: due + REVIEW_WAIT,
                id,
                inputs: s.inputs + queued,
            });
        }
        Ok(())
    }

    /// Confirm Claude Code's "Review your answers" step once it shows after a
    /// phone's answer (`ReviewWatch`); give up when other input reached the
    /// session or the step has not shown by the watch's end.
    fn reviews_due(&mut self, now: Instant) {
        if self.pending_reviews.is_empty() {
            return;
        }
        for w in std::mem::take(&mut self.pending_reviews) {
            if w.at > now {
                self.pending_reviews.push(w);
                continue;
            }
            let Some(s) = self.sessions.get_mut(&w.id).filter(|s| s.alive && s.inputs == w.inputs) else { continue };
            let Some(h) = &s.hosted else { continue };
            let lines = screen::screen_text(&h.lock().term);
            if prompt::parse(&lines).is_some_and(|p| p.is_answer_review() && p.selected == 0) {
                h.write(b"\r".to_vec());
                s.input_changed();
            } else if now < w.until {
                self.pending_reviews.push(ReviewWatch { at: now + REVIEW_POLL, ..w });
            }
        }
    }

    /// Press the keys of a phone's answer that are due, while nothing else
    /// reached the session since the answer began.
    fn keys_due(&mut self, now: Instant) {
        if self.pending_keys.is_empty() {
            return;
        }
        let (due, waiting): (Vec<_>, Vec<_>) = self.pending_keys.drain(..).partition(|(at, ..)| *at <= now);
        self.pending_keys = waiting;
        let now_ms = util::now_ms();
        let mut superseded = Vec::new();
        for (_, id, PendingKey { key, inputs }) in due {
            if let Some(expected) = inputs {
                let Some(s) = self.sessions.get(&id).filter(|s| s.alive) else { continue };
                if superseded.contains(&id) || s.inputs != expected {
                    superseded.push(id);
                    continue;
                }
            }
            if matches!(&key, QueuedKey::SendNow)
                && self.sessions.get_mut(&id).is_some_and(|s| s.alive && !s.asking_now(now_ms))
            {
                if let Err(error) = self.ensure_claude_enter_policy(id) {
                    self.announce_error(format!("{}: {error:#}", self.label_of(id)));
                    continue;
                }
            }
            let Some(s) = self.sessions.get_mut(&id).filter(|s| s.alive) else { continue };
            // An ordinary answer can use the same Enter bytes. Guard only a
            // delayed send-now if a question opened after its message's Enter.
            let bytes = match key {
                QueuedKey::SendNow if s.asking_now(now_ms) => continue,
                QueuedKey::SendNow => SEND_NOW_KEYS.as_bytes().to_vec(),
                QueuedKey::Answer(key) => key.into_bytes(),
            };
            s.input_changed();
            if let Some(h) = &s.hosted {
                h.write(bytes);
            }
        }
        if !superseded.is_empty() {
            self.pending_keys.retain(|(_, id, k)| k.inputs.is_none() || !superseded.contains(id));
        }
    }

    /// Type `text` into every listed session that is ready for it, then press
    /// Enter in each after `SUBMIT_DELAY`. A session waiting on a question is
    /// skipped: the Enter would answer that question instead.
    /// Keep a file a deck pasted from its own clipboard in `pasted/` beside
    /// the registry, where the session's program can read it, and paste its
    /// path into the session as typing would (`ClientMsg::PasteFile`).
    fn paste_file(&mut self, id: u64, name: &str, data: &str) -> Result<()> {
        anyhow::ensure!(!self.pending_directories.contains_key(&id), "the agent is changing directory");
        let bytes = util::unbase64(data).context("the deck sent a damaged file")?;
        if bytes.len() as u64 > remote::uploads::FILE_MAX {
            bail!("it is more than {} MiB", remote::uploads::FILE_MAX >> 20);
        }
        let s = self.sessions.get_mut(&id).filter(|s| s.alive).context("the session is not running")?;
        let h = s.hosted.as_ref().context("the session is not running")?;
        let path = crate::hosts::peer::keep_upload(&self.opts.state_dir.join("pasted"), name, &bytes)?;
        let modes =
            if h.lock().term.mode().contains(TermMode::BRACKETED_PASTE) { screen::M_BRACKETED_PASTE } else { 0 };
        s.owner_draft = true;
        let codex = s.tool == Tool::Codex;
        let encoded = crate::keys::encode_paste(&path.to_string_lossy(), modes);
        if !codex {
            return self.client_input(id, &encoded);
        }
        anyhow::ensure!(
            !s.recovery.draft_submit_requested,
            "The image draft is already waiting to submit; try again after it is accepted"
        );
        anyhow::ensure!(
            s.recovery.can_queue(&path.to_string_lossy()),
            "The reconnect input buffer is full; the uploaded file was preserved"
        );
        self.capture_client_input(id, &crate::keys::encode_message(""));
        let s = self.sessions.get_mut(&id).unwrap();
        // Only PasteFile establishes attachment ownership. An existing path
        // in the text composer may be ordinary text, regardless of its type.
        s.recovery.draft_files.push(path.to_string_lossy().into_owned());
        anyhow::ensure!(self.checkpoint_recovery(true), "Could not save attachment recovery state; delivery is held");
        let s = self.sessions.get_mut(&id).unwrap();
        if s.recovery.phase.is_none() {
            if let Some(h) = &s.hosted {
                h.follow_output();
                h.write(encoded.into_bytes());
            }
            s.recovery.draft_restored = true;
            s.typed_input();
        }
        Ok(())
    }

    fn client_input(&mut self, id: u64, data: &str) -> Result<()> {
        self.client_input_with_receipt(id, data, None)
    }

    fn client_input_with_receipt(
        &mut self,
        id: u64,
        data: &str,
        done: Option<Box<dyn FnOnce(bool) + Send>>,
    ) -> Result<()> {
        if done.is_some() {
            let session = self.sessions.get(&id).context("target unavailable")?;
            anyhow::ensure!(
                session.alive
                    && session.closing.is_none()
                    && session.recovery.phase.is_none()
                    && !session.recovery.paused
                    && session.recovery.draft_files.is_empty()
                    && !session.recovery.draft_submit_requested,
                "target input is changing or recovering"
            );
        }
        let pasted = data.strip_prefix("\x1b[200~").and_then(|s| s.strip_suffix("\x1b[201~")).is_some();
        let submission = submits(data) && !pasted;
        let previous_boot = self
            .registry
            .sessions
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| (entry.working, entry.boot_pending, entry.owner_paused));
        if !submission {
            self.note_boot_input(id, data, false);
        }
        if self.pause_recovery_input(id, data) {
            self.broadcast(&ServerMsg::Notice {
                text: format!("{}: recovery delivery paused; input is preserved. Submit to resume.", self.label_of(id)),
                error: false,
            });
            return Ok(());
        }
        if self.sessions.get(&id).is_some_and(|s| !s.recovery.draft_files.is_empty()) && submission {
            let Some(prefix) = data.strip_suffix('\r').filter(|s| !s.chars().any(char::is_control)) else {
                bail!("Submit the restored image draft with Enter");
            };
            if !prefix.is_empty() {
                anyhow::ensure!(
                    !self.sessions[&id].recovery.draft_submit_requested,
                    "The image draft is already waiting to submit"
                );
                anyhow::ensure!(self.sessions[&id].recovery.can_queue(prefix), "The image draft input buffer is full");
                let previous = self.sessions[&id].recovery.clone();
                self.capture_client_input(id, prefix);
                if !self.checkpoint_recovery(true) {
                    self.sessions.get_mut(&id).unwrap().recovery = previous;
                    self.snapshot_recovery();
                    bail!(
                        "Could not save the image caption; caption was not accepted, retry it after storage recovers"
                    );
                }
                let s = self.sessions.get_mut(&id).unwrap();
                if s.recovery.draft_restored && matches!(s.recovery.phase, None | Some(recovery::Phase::Replaying)) {
                    if let Some(h) = &s.hosted {
                        if crate::providers::recovery::codex_reconnect_failed(&screen::screen_text(&h.lock().term)) {
                            s.recovery.draft_restored = false;
                        } else {
                            h.write(prefix.as_bytes().to_vec());
                        }
                    }
                    s.typed_input();
                }
            }
            self.note_boot_input(id, data, true);
            self.resume_recovery_input(id);
            let s = self.sessions.get_mut(&id).unwrap();
            s.recovery.draft_submit_requested = true;
            if s.recovery.phase.is_none() {
                s.recovery.phase = Some(recovery::Phase::Replaying);
                s.recovery.phase_at_ms = util::now_ms();
            }
            if !self.checkpoint_recovery(true) {
                self.sessions.get_mut(&id).unwrap().recovery.draft_submit_requested = false;
                self.pending_submits.retain(|p| p.id != id);
                bail!("Could not save image submission intent; the image remains an unsent draft");
            }
            self.pending_continues.retain(|p| p.id != id);
            return Ok(());
        }
        anyhow::ensure!(
            !self.sessions.get(&id).is_some_and(|s| s.recovery.draft_submit_requested),
            "The image draft is already waiting to submit; edits can resume after it is accepted"
        );
        // A bare Enter explicitly releases held delivery. It does not turn
        // the owner's unsent draft into another queued message by itself.
        if data == "\r" && self.sessions.get(&id).is_some_and(|s| s.recovery.paused) {
            self.note_boot_input(id, data, true);
            self.resume_recovery_input(id);
            return Ok(());
        }
        if self.sessions.get(&id).is_some_and(|s| s.recovery.phase.is_some())
            && !(data == "\r" || pasted || data.strip_suffix('\r').unwrap_or(data).chars().all(|c| !c.is_control()))
        {
            bail!("Editing keys must wait for reconnection; held input is preserved");
        }
        if self.sessions.get(&id).is_some_and(|s| s.recovery.phase.is_some() && !s.recovery.can_queue(data)) {
            bail!("The reconnect input buffer is full; wait for reconnection before typing more");
        }
        let resume = submission && self.sessions.get(&id).is_some_and(|s| s.recovery.paused);
        let previous = self.sessions.get(&id).map(|s| s.recovery.clone());
        if submission {
            self.note_boot_input(id, data, true);
        }
        self.capture_client_input(id, data);
        // Only a Codex client's captured input is in the deck's custody; for
        // other agents a submission is saved promptly without holding the
        // loop that echoes keys on a disk sync.
        let custody = self.sessions.get(&id).is_some_and(|s| s.tool == Tool::Codex);
        let durable = if (submission || pasted) && !custody {
            self.queue_registry_save();
            true
        } else {
            self.checkpoint_recovery(submission || pasted)
        };
        if !durable {
            if let (Some(s), Some(previous)) = (self.sessions.get_mut(&id), previous) {
                s.recovery = previous;
            }
            if let (Some(entry), Some((working, pending, paused))) =
                (self.registry.sessions.iter_mut().find(|entry| entry.id == id), previous_boot)
            {
                entry.working = working;
                entry.boot_pending = pending;
                entry.owner_paused = paused;
            }
            self.snapshot_recovery();
            bail!("Could not save recovery state; input was not accepted, retry after storage recovers");
        }
        if submission {
            self.pending_continues.retain(|p| p.id != id);
        }
        if resume {
            self.resume_recovery_input(id);
        }
        if let Some(s) = self.sessions.get_mut(&id) {
            if s.recovery.phase.is_some() {
                return Ok(());
            }
            if s.alive {
                if !data.is_empty() {
                    let inline = data.strip_suffix('\r').filter(|text| !text.chars().any(char::is_control));
                    let submitting = inline.is_some()
                        && !s.asking_now(util::now_ms())
                        && s.hosted.as_ref().is_some_and(|h| {
                            status::owner_composer_submission(s.tool, &h.lock().term, inline.unwrap())
                        });
                    s.owner_draft = !submitting;
                }
                s.note_typed(data);
            }
            if let (true, Some(h)) = (s.alive, &s.hosted) {
                h.follow_output();
                if let Some(done) = done {
                    let output_revision = h.lock().output_revision;
                    h.write_guarded(
                        data.as_bytes().to_vec(),
                        Instant::now() + Duration::from_millis(150),
                        h.input_epoch(),
                        Arc::new(Mutex::new(1)),
                        1,
                        move |emu| emu.output_revision == output_revision,
                        done,
                    )?;
                } else {
                    h.write(data.as_bytes().to_vec());
                }
                s.typed_input();
                if submission {
                    s.recovery.replay_submitted = None;
                    s.tracker.clear_finished();
                }
            }
        }
        Ok(())
    }

    fn automatic_input_pending(&self, id: u64) -> bool {
        self.messages.active(id)
            || self.pending_submits.iter().any(|pending| pending.id == id)
            || self.pending_keys.iter().any(|(_, session, _)| *session == id)
            || self.pending_reviews.iter().any(|pending| pending.id == id)
    }

    fn submit_due(&mut self, now: Instant) {
        if self.pending_submits.is_empty() {
            return;
        }
        let (due, waiting): (Vec<_>, Vec<_>) = self.pending_submits.drain(..).partition(|p| p.at <= now);
        self.pending_submits = waiting;
        let now_ms = util::now_ms();
        let mut withheld = Vec::new();
        let mut cancelled = Vec::new();
        for mut p in due {
            let (id, cid) = (p.id, p.cid);
            let Some(s) = self.sessions.get_mut(&id).filter(|s| s.alive) else {
                continue;
            };
            if s.recovery.paused {
                continue;
            }
            if s.inputs != p.inputs || now >= p.until {
                if s.recovery.phase.is_none() {
                    s.recovery.replay_submitted = None;
                }
                let why = if s.inputs != p.inputs { "its input changed" } else { "submission timed out" };
                cancelled.push((id, cid, why));
                continue;
            }
            // A question that opened since the paste would take the Enter as
            // its answer, for example approving a permission prompt.
            if s.asking_now(now_ms) {
                if s.recovery.phase.is_none() {
                    s.recovery.replay_submitted = None;
                }
                withheld.push((id, cid));
                continue;
            }
            // Claude Code 2.1.317 catches Enter during an image paste, then
            // clears that deferred key when its image-reading promise resolves.
            // Wait until its footer no longer says "Pasting…": on a loaded
            // machine Claude drew that footer 3.7 s after the paste, so a fixed
            // delay lost the only Enter.
            let pasting = s.tool == Tool::Claude
                && s.hosted.as_ref().is_some_and(|h| {
                    let lines = screen::screen_text(&h.lock().term);
                    status::live_tail(&lines, 2).iter().any(|line| {
                        let hint = line.trim().rsplit(" · ").next().unwrap_or("");
                        matches!(hint, "Pasting…" | "Pasting...")
                    })
                });
            if pasting {
                p.at = now + PASTE_POLL;
                self.pending_submits.push(p);
                continue;
            }
            let _ = s;
            if !self.sessions.get(&id).is_some_and(|s| {
                s.recovery.phase == Some(recovery::Phase::Replaying) || s.recovery.replay_submitted.is_some()
            }) {
                self.capture_client_input(id, "\r");
            }
            if !self.checkpoint_recovery(true) {
                p.at = now + PASTE_POLL;
                self.pending_submits.push(p);
                continue;
            }
            let Some(s) = self.sessions.get_mut(&id) else { continue };
            s.note_typed("\r");
            s.input_changed();
            s.tracker.clear_finished();
            if let Some(h) = &s.hosted {
                h.write(b"\r".to_vec());
            }
            if s.recovery.draft_submit_requested && s.recovery.draft_restored && !s.recovery.draft_files.is_empty() {
                s.recovery.draft_submitted = true;
            }
            if let Some(message) =
                s.recovery.replay_submitted.take().filter(|m| crate::providers::recovery::codex_slash_command(m))
            {
                if let Some(at) = s.recovery.pending.iter().position(|pending| pending == &message) {
                    s.recovery.pending.remove(at);
                }
            }
        }
        for (id, cid) in withheld {
            self.notice(cid, withheld_submit(&self.label_of(id)), true);
        }
        for (id, cid, why) in cancelled {
            self.notice(
                cid,
                format!(
                    "{} was not submitted because {why}; review the typed message before pressing Enter",
                    self.label_of(id)
                ),
                true,
            );
        }
    }

    /// A prior native submission cannot safely be promoted by replaying it.
    /// Initial owner Claude messages already use the provider's now priority.
    fn send_now(&mut self, id: u64) -> Result<()> {
        anyhow::ensure!(self.sessions.get(&id).is_some_and(|s| s.alive), "the session is not running");
        bail!("Native Send now is unavailable; no terminal input was sent")
    }

    fn ensure_claude_enter_policy(&self, id: u64) -> Result<()> {
        let s = self.sessions.get(&id).filter(|s| s.alive).context("the session is not running")?;
        let root = s.account_root.as_deref().unwrap_or(&self.opts.claude_dir);
        let ready =
            crate::providers::claude_keybindings::compliant_target(root).context("Claude Enter policy is held")?;
        anyhow::ensure!(ready, "Claude Enter policy is not ready; repair this session's keybindings before Send now");
        Ok(())
    }

    /// Drop a broadcast's pending Enter because the session now asks a question.
    fn withhold_submit(&mut self, id: u64) {
        // A question can arrive after Enter was queued but before the native
        // writer accepts it. Revoke that action at the actual write boundary.
        if let Some(hosted) = self.sessions.get(&id).and_then(|session| session.hosted.as_ref()) {
            hosted.revoke_guarded_input();
        }
        let (dropped, kept): (Vec<_>, Vec<_>) = self.pending_submits.drain(..).partition(|p| p.id == id);
        self.pending_submits = kept;
        for p in dropped {
            self.notice(p.cid, withheld_submit(&self.label_of(p.id)), true);
        }
    }

    fn send(&self, client: u64, msg: &ServerMsg) {
        if let Some(c) = self.clients.get(&client) {
            // Older native clients cannot deserialize the new receipt variant.
            // They still receive truthful admission, without claiming a new turn.
            if c.version.is_some_and(|version| version < 21) {
                if let ServerMsg::MessageSent {
                    id,
                    request,
                    error,
                    delivered: Some(crate::protocol::Delivered::Steered),
                } = msg
                {
                    let compatible = ServerMsg::MessageSent {
                        id: *id,
                        request: *request,
                        error: error.clone(),
                        delivered: Some(crate::protocol::Delivered::Submitted),
                    };
                    let _ = c.out.send(Out::Msg(encode(&compatible)));
                    return;
                }
            }
            let _ = c.out.send(Out::Msg(encode(msg)));
        }
    }

    /// How messages name session `id` (`protocol::session_label`).
    fn label_of(&self, id: u64) -> String {
        self.sessions.get(&id).map_or_else(|| "a closed session".to_string(), |s| session_label(s.tool, &s.title))
    }

    fn notice(&self, client: u64, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error {
            self.log_error(&text);
        }
        self.send(client, &ServerMsg::Notice { text, error });
    }

    fn broadcast(&self, msg: &ServerMsg) {
        let buf = encode(msg);
        for c in self.clients.values().filter(|c| c.version.is_some() && c.role != ClientRole::Hook) {
            let _ = c.out.send(Out::Msg(buf.clone()));
        }
    }

    /// Pass a notification to the phone gateways. Only `remote` clients get
    /// it: an older deck cannot read the message and would drop the
    /// connection.
    fn alert_remote(&self, kind: AlertKind, id: Option<u64>, title: &str, reason: &str) {
        self.alert_remote_for(kind, id, title, reason, None);
    }

    /// `alert_remote`, about the owner message the phone names `message`.
    fn alert_remote_for(&self, kind: AlertKind, id: Option<u64>, title: &str, reason: &str, message: Option<&str>) {
        let desk_focused = self.clients.values().any(|c| c.role == ClientRole::Tui && c.focused);
        let buf = encode(&ServerMsg::Alert(Alert {
            kind,
            id,
            title: title.to_string(),
            reason: reason.to_string(),
            desk_focused,
            message: message.map(str::to_owned),
        }));
        // A hub (`peer`) shows this host's alerts as its own.
        for c in self.clients.values().filter(|c| matches!(c.role, ClientRole::Remote | ClientRole::Peer)) {
            let _ = c.out.send(Out::Msg(buf.clone()));
        }
    }

    /// Send the deck state to every deck, each as its protocol can read it.
    /// A deck that takes patches gets only the parts that changed since the
    /// state it holds, encoded once for every deck holding that same state.
    fn broadcast_state(&mut self, state: &Arc<DeckState>) {
        // The state as each protocol version sees it, the current one shared.
        let mut views: HashMap<u32, Arc<DeckState>> = HashMap::new();
        let mut whole: HashMap<u32, Vec<u8>> = HashMap::new();
        let mut patches: Vec<(Arc<DeckState>, Option<Vec<u8>>)> = Vec::new();
        for c in self.clients.values_mut().filter(|c| c.role != ClientRole::Hook) {
            let Some(version) = c.version else { continue };
            let version = version.min(PROTOCOL_VERSION);
            let view = views
                .entry(version)
                .or_insert_with(|| {
                    if version < PROTOCOL_VERSION {
                        Arc::new(state_for(version, state))
                    } else {
                        state.clone()
                    }
                })
                .clone();
            let buf = match c.state_base.take() {
                Some(base) => {
                    let at = match patches.iter().position(|(seen, _)| Arc::ptr_eq(seen, &base)) {
                        Some(at) => at,
                        None => {
                            let patch = view.diff(&base);
                            let buf = (!patch.is_empty()).then(|| encode(&ServerMsg::StatePatch(patch)));
                            patches.push((base, buf));
                            patches.len() - 1
                        }
                    };
                    patches[at].1.clone()
                }
                None => {
                    Some(whole.entry(version).or_insert_with(|| encode(&ServerMsg::State((*view).clone()))).clone())
                }
            };
            if takes_patches(c.role, version) {
                c.state_base = Some(view);
            }
            if let Some(buf) = buf {
                let _ = c.out.send(Out::Msg(buf));
            }
        }
    }

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::FrameReady => {}
            Ev::NativeNewPrepared { nonce, result } => self.native_new_prepared(nonce, result),
            Ev::NativeNewReady { client, id, request, nonce, result } => {
                self.native_new_ready(client, id, request, nonce, result)
            }
            Ev::NativePeer { client, endpoint } => {
                if let Some(c) = self.clients.get_mut(&client) {
                    c.native_endpoint = Some(endpoint);
                }
            }
            Ev::NativePrepared { client, prepared } => self.attention_native_prepared(client, *prepared),
            Ev::NativeDone { client, session, rule, written } => {
                if let Some(a) = &mut self.attention {
                    a.native_pending.remove(&client);
                }
                let _ = self.attention_audit(session, if written { "native_allowed" } else { "native_neutral" }, rule);
            }
            Ev::SupervisorDone { session, rule, written } => {
                let _ = self.attention_audit(session, if written { "submitted" } else { "unknown" }, Some(rule));
            }
            Ev::MessageDelivered { id, batch, result } => self.native_message_delivered(id, batch, result),
            Ev::ClaudeFilesChecked { id, batch, result } => self.claude_files_checked(id, batch, result),
            Ev::PluginJob(done) => self.service_plugin_job(done),
            Ev::PluginReply { client, request, data, error } => {
                self.plugin_reply(client, request, data, error);
            }
            Ev::ClientNew { id, out, frame_busy } => {
                self.clients.insert(
                    id,
                    Client {
                        out,
                        frame_busy,
                        role: ClientRole::Control,
                        version: None,
                        attached: None,
                        sent_gen: 0,
                        focused: true,
                        upstreams: HashMap::new(),
                        pending_frame: None,
                        native_endpoint: None,
                        state_base: None,
                    },
                );
            }
            Ev::ClientGone(id) => {
                if let Some(endpoint) = self.clients.get(&id).and_then(|c| c.native_endpoint.as_ref()) {
                    endpoint.canceled.store(true, Ordering::SeqCst);
                }
                self.conversation_scans.forget(id);
                self.pending_lists.remove(&id);
                self.clients.remove(&id);
                self.broadcasts.pending.retain(|(client, _), _| *client != id);
                self.job_routes.retain(|(client, _), _| *client != id);
                self.panels.remove(&id);
            }
            Ev::Installed { client, ok, lines } => {
                self.native.installs = self.native.installs.saturating_sub(1);
                let host = self.opts.config.local_host_label();
                self.send(client, &ServerMsg::Installed { host, ok, lines });
            }
            Ev::ScanReady { client, scan, error } => self.scan_ready(client, scan, error),
            Ev::Notice { client, text, error } => match client {
                Some(cid) => self.notice(cid, text, error),
                None => {
                    if error {
                        self.log_error(&text);
                    }
                    self.broadcast(&ServerMsg::Notice { text, error });
                }
            },
            Ev::Branches(branches) => {
                self.branches_busy = false;
                self.branches = branches;
            }
            Ev::RegistrySaved(result) => {
                self.registry_writer.in_flight = self.registry_writer.in_flight.saturating_sub(1);
                if let Err(err) = result {
                    self.registry_writer.failed();
                    self.registry_dirty = true;
                    eprintln!("cepheus: saving {}: {err}", self.registry_path.display());
                }
            }
            Ev::ApiCost(result) => extensions::native::api_cost_done(self, result),
            Ev::RepositoryIdentity(found) => {
                self.identity_busy = false;
                if let Some((id, root)) = found {
                    if self.scheduler.rediscover_repository(id, root) {
                        self.state_changed();
                    }
                }
            }
            Ev::Client(id, msg) => self.handle_client(id, msg),
            Ev::Session(ev) => self.handle_session(ev),
            Ev::Monitor(report) => self.apply_report(*report),
            Ev::Conversations(client, query, request, items) => {
                self.conversations_part(client, 0, Some(query), request, items);
                self.conversations_scanned(client);
            }
            Ev::Host(ev) => self.on_host(*ev),
            Ev::MoveInDone { client, tool, conversation, error } => {
                self.moves_in_flight = self.moves_in_flight.saturating_sub(1);
                match error {
                    Some(err) => self.notice(client, format!("Move-in failed: {err}"), true),
                    None => match self.resume(tool, conversation) {
                        Ok(id) => self.send(client, &ServerMsg::Created { id, request: None }),
                        Err(err) => self.notice(client, format!("Move-in failed: {err:#}"), true),
                    },
                }
            }
        }
        self.pump_native_new();
    }

    fn handle_session(&mut self, ev: SessionEvent) {
        match ev {
            SessionEvent::Output(id) => {
                if let Some(h) = self.sessions.get(&id).and_then(|s| s.hosted.as_ref()) {
                    h.clear_notified();
                }
            }
            SessionEvent::Exited(id, code) => {
                self.client_temp_due = self.client_temp_due.min(Instant::now() + CLIENT_TEMP_AFTER_EXIT);
                if self.recovering_exit(id) {
                    return;
                }
                self.scheduler.ended(id);
                let now_ms = util::now_ms();
                let mut failed = None;
                let shutting_down = self.shutdown_at.is_some();
                let mut failed_start = false;
                if let Some(s) = self.sessions.get_mut(&id) {
                    let ended_by_deck = s.closing.is_some() || s.remove_when_ended || shutting_down;
                    // A program that ended by itself before it had a conversation failed to start: its row has
                    // nothing to reopen, so it leaves the list, and its error keeps what its screen said.
                    failed_start = !ended_by_deck && !has_conversation(s, &self.opts);
                    // A program the deck did not end that exits with an error.
                    if let Some(code) = code.filter(|c| *c != 0 && !ended_by_deck) {
                        let mut text = format!("{} exited with status {code}", session_label(s.tool, &s.title));
                        if let Some(reason) = s.hosted.as_ref().and_then(last_words).filter(|_| failed_start) {
                            text = format!("{text}: {reason}");
                        }
                        failed = Some(text);
                    }
                    s.alive = false;
                    s.exit_code = code;
                    s.closing = None;
                    s.tracker.update(false, false, now_ms);
                    if let Some(h) = &s.hosted {
                        h.bump();
                    }
                }
                if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == id) {
                    entry.ended_at = Some(util::now_secs());
                }
                self.registry_dirty = true;
                self.save_registry();
                if self.pending_directories.contains_key(&id) {
                    self.finish_directory_change(id);
                    return;
                }
                self.program_exited(id);
                if failed_start || self.sessions.get(&id).is_some_and(|s| s.remove_when_ended) {
                    let _ = self.forget(id);
                }
                if let Some(text) = failed {
                    self.log_error(&text);
                }
                self.update_refs();
            }
            SessionEvent::Title(id, title) => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.term_title = title.as_deref().and_then(status::clean_title);
                }
            }
            SessionEvent::Clipboard(id, text) => {
                let msg = encode(&ServerMsg::Clipboard { text });
                for c in self.clients.values().filter(|c| c.attached == Some(id)) {
                    let _ = c.out.send(Out::Msg(msg.clone()));
                }
            }
            SessionEvent::Bell(id) => {
                let msg = encode(&ServerMsg::Bell { id });
                for c in self.clients.values().filter(|c| c.attached == Some(id)) {
                    let _ = c.out.send(Out::Msg(msg.clone()));
                }
            }
            SessionEvent::ColorRequest(id, index, format) => {
                if let Some(h) = self.sessions.get(&id).and_then(|s| s.hosted.as_ref()) {
                    let set = h.lock().term.colors()[index];
                    let color = set.unwrap_or_else(|| {
                        let (r, g, b) = self.palette.get(index);
                        Rgb { r, g, b }
                    });
                    h.write(format(color).into_bytes());
                }
            }
            SessionEvent::SizeRequest(id, format) => {
                if let Some(h) = self.sessions.get(&id).and_then(|s| s.hosted.as_ref()) {
                    let size = WindowSize { num_lines: h.rows, num_cols: h.cols, cell_width: 8, cell_height: 16 };
                    h.write(format(size).into_bytes());
                }
            }
        }
    }

    fn handle_client(&mut self, cid: u64, msg: ClientMsg) {
        // Native hook sockets are one-request local capabilities, never a way
        // to route controls or prompts to another service.
        if matches!(&msg, ClientMsg::PermissionRequest { .. }) {
            if let ClientMsg::PermissionRequest { session, spawned_ms, token, payload } = msg {
                self.attention_native_request(cid, session, spawned_ms, &token, payload);
            }
            return;
        }
        // Claude's owner input plugin speaks only for the session it runs
        // under (`claude_input.rs`), never through another service.
        match msg {
            ClientMsg::OwnerInputListen { conversation, stop } => {
                return self.owner_input_listen(cid, &conversation, stop)
            }
            ClientMsg::OwnerInputReceipt { message, outcome, reason } => {
                return self.owner_input_receipt(cid, &message, outcome, reason)
            }
            _ => {}
        }
        if self.clients.get(&cid).is_some_and(|c| c.role == ClientRole::Hook)
            && !matches!(&msg, ClientMsg::Hello { .. } | ClientMsg::Hook { .. })
        {
            return;
        }

        if let ClientMsg::Fleet { request, action, args } = &msg {
            if action == "attention-supervisor-rules"
                && args["rules"]
                    .as_array()
                    .is_some_and(|rules| rules.iter().any(|r| r["pattern"]["kind"] == "native_exact"))
                && self.clients.get(&cid).and_then(|c| c.version).is_none_or(|v| v < 16)
            {
                self.send(
                    cid,
                    &ServerMsg::FleetResult {
                        request: *request,
                        data: Value::Null,
                        error: Some("Update needed: native permission rules require protocol 16".into()),
                    },
                );
                return;
            }
            if crate::attention::is_action(action)
                && self.clients.get(&cid).and_then(|c| c.version).is_none_or(|v| v < 15)
            {
                self.send(
                    cid,
                    &ServerMsg::FleetResult {
                        request: *request,
                        data: Value::Null,
                        error: Some("Update needed: attention requires protocol 15".into()),
                    },
                );
                return;
            }
        }
        if let ClientMsg::Fleet { request, action, .. } = &msg {
            if action == "attention-send" && self.clients.get(&cid).and_then(|c| c.version).is_none_or(|v| v < 17) {
                self.send(
                    cid,
                    &ServerMsg::FleetResult {
                        request: *request,
                        data: Value::Null,
                        error: Some("Update needed: identity-safe attention send requires protocol 17".into()),
                    },
                );
                return;
            }
        }
        let Some(msg) = self.route_client(cid, msg) else { return };
        match msg {
            ClientMsg::PermissionRequest { .. }
            | ClientMsg::OwnerInputListen { .. }
            | ClientMsg::OwnerInputReceipt { .. } => unreachable!("handled before routing"),
            ClientMsg::SetOrder { ids } => match self.set_session_order(&ids) {
                Ok(()) => self.notice(cid, crate::protocol::ORDER_SAVED_NOTICE, false),
                Err(error) => self.notice(cid, format!("Could not save session order: {error}"), true),
            },
            ClientMsg::Fleet { request, action, args } => self.fleet_reply(cid, request, &action, &args),
            ClientMsg::PluginList { request } => self.send(
                cid,
                &ServerMsg::PluginResult {
                    request,
                    data: serde_json::json!({"plugins":self.service_plugin_commands()}),
                    error: None,
                },
            ),
            ClientMsg::Plugin { request, plugin, command, args } => {
                self.handle_plugin(cid, request, &plugin, &command, &args)
            }
            ClientMsg::Hello { version, role } => {
                if let Some(c) = self.clients.get_mut(&cid) {
                    c.role = role;
                    c.version = Some(version);
                }
                self.send(
                    cid,
                    &ServerMsg::Welcome {
                        version: PROTOCOL_VERSION,
                        pid: std::process::id(),
                        start_dir: self.opts.start_dir.to_string_lossy().into_owned(),
                    },
                );
                if role == ClientRole::Hook {
                    return;
                }
                // The hosts come first: a deck loads its scripts on its first state, and the unmerged-branches
                // count read no hosts' branches there.
                self.send_hosts(cid);
                let state = Arc::new(state_for(version, &self.build_state()));
                self.send(cid, &ServerMsg::State((*state).clone()));
                if let Some(c) = self.clients.get_mut(&cid) {
                    c.state_base = takes_patches(role, version.min(PROTOCOL_VERSION)).then_some(state);
                }
                // A deck open since before an update talks to the updated
                // service: it is the deck that is old, not the service.
                if version < PROTOCOL_VERSION && role == ClientRole::Tui {
                    self.notice(
                        cid,
                        format!(
                            "This Cepheus window is out of date. Reopen it to use the updated service.\n\
                             Window protocol {version}; service protocol {PROTOCOL_VERSION}."
                        ),
                        false,
                    );
                } else if version > PROTOCOL_VERSION {
                    self.notice(
                        cid,
                        format!(
                            "The Cepheus service needs an update before this client can use its newer features.\n\
                             Client protocol {version}; service protocol {PROTOCOL_VERSION}."
                        ),
                        true,
                    );
                }
            }
            ClientMsg::Attach { id, cols, rows } => {
                let previous = self.clients.get(&cid).and_then(|c| c.attached);
                if let Some(c) = self.clients.get_mut(&cid) {
                    c.attached = id;
                    c.sent_gen = 0;
                }
                self.apply_view(cols, rows);
                if previous != id {
                    if let Some(prev) = previous {
                        self.send_focus(prev, false);
                    }
                    if let Some(new) = id {
                        self.send_focus(new, true);
                    }
                }
                self.last_status = Instant::now() - STATUS_INTERVAL;
            }
            ClientMsg::Resize { cols, rows } => self.apply_view(cols, rows),
            ClientMsg::History { id, request, epoch, cols, rows } => {
                let result = (|| -> Result<screen::HistorySnapshot> {
                    let s = self.sessions.get(&id).context("Session is no longer available")?;
                    let h = s.hosted.as_ref().context("Session has no terminal history")?;
                    anyhow::ensure!(
                        h.history_epoch == epoch && (h.cols, h.rows) == (cols, rows),
                        "Terminal changed; try history again"
                    );
                    let emu = h.lock();
                    Ok(screen::build_history(&emu.term, emu.output_revision))
                })();
                let (snapshot, error) = match result {
                    Ok(snapshot) => (Some(snapshot), None),
                    Err(error) => (None, Some(error.to_string())),
                };
                self.send(cid, &ServerMsg::History { id, request, epoch, cols, rows, snapshot, error });
            }
            ClientMsg::ResizeTerminal { id, cols, rows } => {
                let resized = (|| -> Result<(Vec<String>, u64)> {
                    if !(20..=500).contains(&cols) || !(5..=300).contains(&rows) {
                        bail!("invalid terminal dimensions");
                    }
                    let session = self.sessions.get_mut(&id).context("no such terminal")?;
                    if session.tool != Tool::Shell || !session.alive {
                        bail!("only a live shell can be resized separately");
                    }
                    let hosted = session.hosted.as_mut().context("the terminal is unavailable")?;
                    hosted.resize(cols, rows);
                    let lines = screen::screen_text(&hosted.lock().term);
                    Ok((lines, session.inputs))
                })();
                match resized {
                    Ok((lines, inputs)) => self.send(cid, &ServerMsg::ScreenText { id, lines, inputs }),
                    Err(err) => self.notice(cid, format!("Could not resize terminal: {err:#}"), true),
                }
            }
            ClientMsg::Input { id, data } => {
                if self.pending_directories.contains_key(&id) {
                    self.notice(cid, "The agent is changing directory; wait for it to restart before typing.", true);
                    return;
                }
                if matches!(data.as_str(), "\x1b" | "\x03") {
                    self.messages.cancel(id);
                    if let Some(session) = self.sessions.get(&id) {
                        session.chat.cancel_controls();
                    }
                }
                if let Err(error) = self.client_input(id, &data) {
                    self.notice(cid, error.to_string(), true);
                }
            }
            // The owner's Stop. A Claude Esc removes the owner's queued
            // messages and refuses every later one until someone types at
            // the terminal, so Claude's plugin ends the turn instead and the
            // owner's messages stay. Other sessions get the Esc.
            ClientMsg::Stop { id } => {
                if self.pending_directories.contains_key(&id) {
                    self.notice(cid, "The agent is changing directory; wait for it to restart before typing.", true);
                    return;
                }
                if let Some(session) = self.sessions.get(&id) {
                    session.chat.cancel_controls();
                }
                if self.owner_input_stop(id) {
                    return;
                }
                self.messages.cancel(id);
                if let Err(error) = self.client_input(id, "\x1b") {
                    self.notice(cid, error.to_string(), true);
                }
            }
            ClientMsg::Broadcast { ids, text, running_only } => {
                // `route_client` already sends running-only broadcasts here.
                if running_only {
                    self.start_fleet_broadcast(cid, &ids, &text);
                } else {
                    self.broadcast_message(cid, &ids, &text);
                }
            }
            ClientMsg::BroadcastPart { request, ids, text } => {
                self.begin_broadcast_part(cid, request, ids.as_deref(), &text, false);
            }
            ClientMsg::SendMessage { id, request, text, attachments } => {
                self.send_message(cid, id, request, &text, attachments)
            }
            ClientMsg::PasteFile { id, name, data } => {
                if let Err(err) = self.paste_file(id, &name, &data) {
                    self.notice(cid, format!("Could not paste the image: {err:#}"), true);
                }
            }
            ClientMsg::Scroll { id, op } => {
                if let Some(h) = self.sessions.get(&id).and_then(|s| s.hosted.as_ref()) {
                    h.scroll(op);
                }
            }
            // `route_client` passed on every start naming another host.
            ClientMsg::Create { tool, model, effort, cwd, command, name, request, .. } => {
                let place = Placement { cwd: cwd.map(PathBuf::from), command, name, ..Placement::default() };
                if matches!(tool, Tool::Codex | Tool::OpenCode) {
                    self.start_native_new(cid, tool, StartModel { model, effort }, place, request);
                    return;
                }
                match self.create_with(tool, None, &StartModel { model, effort }, &place) {
                    Ok(id) => self.send(cid, &ServerMsg::Created { id, request }),
                    Err(err) => self.notice(cid, format!("Could not start {}: {err:#}", tool.label()), true),
                }
            }
            ClientMsg::Close { id } => self.close_and_remove(id),
            ClientMsg::Reopen { id } => match self.reopen(id) {
                Ok(()) => self.send(cid, &ServerMsg::Created { id, request: None }),
                Err(err) => self.notice(cid, format!("Could not reopen: {err:#}"), true),
            },
            ClientMsg::ChangeDirectory { id, path } => {
                if let Err(err) = self.change_directory(cid, id, &path) {
                    self.directory_reply(cid, id, None, Some(format!("{err:#}")));
                }
            }
            ClientMsg::Forget { id } => {
                if let Err(err) = self.forget(id) {
                    self.notice(cid, format!("{err:#}"), true);
                }
            }
            ClientMsg::Resume { tool, conversation, .. } => match self.resume(tool, conversation) {
                Ok(id) => self.send(cid, &ServerMsg::Created { id, request: None }),
                Err(err) => self.notice(cid, format!("Could not resume: {err:#}"), true),
            },
            ClientMsg::MoveIn { tool, pid, conversation } => self.move_in(cid, tool, pid, conversation),
            ClientMsg::ListConversations { query, request } => self.list_conversations(cid, query, request),
            ClientMsg::Focus { focused } => {
                let attached = self.clients.get_mut(&cid).and_then(|c| {
                    c.focused = focused;
                    c.attached
                });
                if let Some(id) = attached {
                    self.send_focus(id, focused);
                }
            }
            ClientMsg::Hook { session, tool, payload } => self.handle_hook(session, tool, &payload),
            ClientMsg::Screen { id, tail } => {
                let session = self.sessions.get(&id);
                let lines = session
                    .and_then(|s| s.hosted.as_ref())
                    .map(|h| {
                        let screen = h.lock();
                        match tail {
                            Some(lines) => screen::tail_text(&screen.term, lines),
                            None => screen::screen_text(&screen.term),
                        }
                    })
                    .unwrap_or_default();
                let inputs = session.map_or(0, |s| s.inputs);
                self.send(cid, &ServerMsg::ScreenText { id, lines, inputs });
            }
            ClientMsg::Answer { id, inputs, prompt, choice, choices, text } => {
                let result = self.answer(id, inputs, &prompt, choice, &choices, text.as_deref());
                let stale = result.as_ref().is_err_and(crate::protocol::is_question_stale);
                let error = result.err().map(|e| format!("{e:#}"));
                self.send(cid, &ServerMsg::Answered { id, error, stale });
            }
            ClientMsg::SendNow { id } => {
                let error = self.send_now(id).err().map(|e| format!("{e:#}"));
                self.send(cid, &ServerMsg::SentNow { id, error });
            }
            ClientMsg::Shutdown => self.request_shutdown(cid),
            ClientMsg::DevServer { run } => self.toggle_dev_server(cid, run),
            ClientMsg::Restart => self.request_restart(cid),
            // ; every deck's list follows with the next state.
            ClientMsg::ClearErrors => {
                if self.clear_errors(None) && self.clients.get(&cid).is_some_and(|c| c.version.is_some_and(|v| v >= 14))
                {
                    self.send(cid, &ServerMsg::ErrorsCleared);
                }
            }
            ClientMsg::ClearErrorsThrough { clears } => {
                self.clear_errors(Some(clears));
            }
            ClientMsg::Rename { id, name } => {
                if let Err(err) = self.rename(id, name.as_deref()) {
                    self.notice(cid, format!("{err:#}"), true);
                }
            }
            ClientMsg::Panels { panels } => {
                let panels = crate::protocol::clean_panels(panels);
                if panels.is_empty() {
                    self.panels.remove(&cid);
                } else {
                    self.panels.insert(cid, panels);
                }
            }
            ClientMsg::Install { request, .. } => self.start_install(cid, request),
            ClientMsg::SecurityScan { scope, .. } => self.start_scan(cid, scope),
            ClientMsg::SyncSkills => self.sync_skills(Some(cid)),
        }
    }

    /// Name session `id`'s current chat, or with no name (or a blank one)
    /// go back to the title drawn from it. The name is free text a deck
    /// sends: it is drawn in terminals, so it keeps no control characters
    /// and at most `MAX_NAME` characters.
    fn rename(&mut self, id: u64, name: Option<&str>) -> Result<()> {
        let name = name.map(clean_name).filter(|n| !n.is_empty());
        let Some(s) = self.sessions.get_mut(&id) else { bail!("That session is gone.") };
        s.name = name;
        s.title = s.display_title(&self.claude_agents, &mut self.native.titles);
        if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == id) {
            entry.name = s.name.clone();
            entry.title = s.title.clone();
        }
        self.registry_dirty = true;
        Ok(())
    }

    /// Restart in place: the program runs again in this process with every session, as an update does.
    fn request_restart(&mut self, cid: u64) {
        if !self.opts.update || self.fds.is_none() {
            self.notice(
                cid,
                "This service cannot restart in place: it was started without its socket to pass on.",
                true,
            );
            return;
        }
        if self.shutdown_at.is_some() {
            self.notice(cid, "The service is stopping; start cepheus again once it has.", true);
            return;
        }
        self.restart_requested = true;
        self.broadcast(&ServerMsg::Notice {
            text: "Restarting the service; every session keeps running.".into(),
            error: false,
        });
        self.check_update(Instant::now());
    }

    fn send_focus(&self, id: u64, focused: bool) {
        if let Some(h) = self.sessions.get(&id).filter(|s| s.alive).and_then(|s| s.hosted.as_ref()) {
            let wants = h.lock().term.mode().contains(TermMode::FOCUS_IN_OUT);
            if wants {
                h.write(if focused { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() });
            }
        }
    }

    fn apply_view(&mut self, cols: u16, rows: u16) {
        let view = (cols.max(20), rows.max(5));
        if view == self.view {
            return;
        }
        self.view = view;
        for s in self.sessions.values_mut().filter(|s| s.alive) {
            if let Some(h) = s.hosted.as_mut() {
                h.resize(view.0, view.1);
            }
        }
    }

    fn handle_hook(&mut self, session: u64, tool: Tool, payload: &Value) {
        if payload["kind"] == "attention_permission" {
            let _ = self.attention_permission(session, tool, payload);
            return;
        }
        let Some(s) = self.sessions.get_mut(&session) else { return };
        if s.tool != tool || !s.alive {
            return;
        }
        let text = |k: &str| payload.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let event = text("hook_event_name");
        match tool {
            Tool::Claude if matches!(event.as_str(), "Stop" | "SubagentStart" | "SubagentStop") => {
                // Only the conversation this session is in; never another's.
                let session_id = text("session_id");
                if session_id.is_empty() || s.conversation.as_deref() == Some(session_id.as_str()) {
                    if crate::activity::claude_hook(&mut s.facts.claude_agents, payload, util::now_secs()) {
                        s.facts.subagents = crate::activity::claude_listed(&s.facts.claude_agents);
                    }
                    // A sub-agent's turn is not the session's.
                    if event == "Stop" {
                        match background_work(payload) {
                            Some(work) => s.tracker.turn_waits(work),
                            None => s.tracker.turn_complete(),
                        }
                    }
                }
            }
            Tool::Antigravity | Tool::Grok | Tool::OpenCode | Tool::Shell | Tool::Dynamic(_) => return,
            Tool::Claude => {
                let kind = text("notification_type");
                let message = util::one_line(&text("message"));
                let needs =
                    matches!(kind.as_str(), "permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog")
                        || (kind.is_empty() && message.to_lowercase().contains("permission"));
                if needs {
                    s.tracker.hook_asks(if message.is_empty() { kind } else { message });
                    self.withhold_submit(session);
                }
            }
            Tool::Codex => {
                if text("type") != "agent-turn-complete" {
                    return;
                }
                let thread = payload.get("thread-id").or_else(|| payload.get("thread_id")).and_then(Value::as_str);
                if thread.is_some_and(|id| s.left.iter().any(|left| left == id)) {
                    return;
                }
                // Codex also reports turns of threads it runs for itself
                // (sub-agents, and the temporary thread that names a
                // conversation). Only a thread a person started is the
                // session's conversation, and only its turn ends the session's.
                let own = match thread {
                    None => true,
                    Some(t) if s.conversation.as_deref() == Some(t) => true,
                    Some(t) => {
                        valid_conversation_id(t)
                            && codex::user_rollout(s.account_root.as_deref().unwrap_or(&self.opts.codex_dir), t)
                                .is_some()
                    }
                };
                if !own {
                    return;
                }
                s.tracker.turn_complete();
                if let Some(thread) = thread.filter(|t| s.conversation.as_deref() != Some(*t)) {
                    s.enter_conversation(Some(thread.to_string()));
                    if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == session) {
                        entry.conversation = Some(thread.to_string());
                    }
                    self.registry_dirty = true;
                    self.update_refs();
                }
            }
        }
        self.last_status = Instant::now() - STATUS_INTERVAL;
    }

    /// The command line for a hosted program, the conversation it will be
    /// in when known, and whether it runs without the deck's turn hook.
    #[cfg(test)]
    fn command_for(&self, tool: Tool, resume: Option<&str>) -> Result<Launch> {
        self.launch_for(tool, resume, &Placement::default())
    }

    /// `command_for` with what `place` adds: a terminal's own command, and
    /// a new conversation's extra arguments and first message.
    fn launch_for(&self, tool: Tool, resume: Option<&str>, place: &Placement) -> Result<Launch> {
        if tool == Tool::Shell {
            // : run on a terminal, the shell is interactive and reads the startup files a kitty window does.
            let argv = if place.command.is_empty() { self.opts.config.shell() } else { place.command.clone() };
            anyhow::ensure!(!argv.is_empty(), "no shell to run");
            return Ok(Launch { argv, conversation: None, attached: false });
        }
        let helper = self.opts.config.attention.prompt_avoidance.then_some(&self.opts.exe);
        let permission_paths = if tool == Tool::Claude && self.opts.config.claude_hooks {
            attention_native::materialize(&self.opts.state_dir).map(|helper| (helper.clone(), helper)).ok()
        } else {
            None
        };
        let permission = permission_paths
            .as_ref()
            .map(|(interpreter, helper)| crate::providers::native::PermissionHook { interpreter, helper });
        let mut launch = if let Some(root) = place.account_root.as_deref().filter(|_| tool == Tool::Codex) {
            crate::providers::native::launch_with_hooks(
                &self.opts.config,
                &self.opts.exe,
                root,
                tool,
                resume,
                helper.map(PathBuf::as_path),
                permission,
            )?
        } else {
            crate::providers::native::launch_with_hooks(
                &self.opts.config,
                &self.opts.exe,
                &self.opts.codex_dir,
                tool,
                resume,
                helper.map(PathBuf::as_path),
                permission,
            )?
        };
        if tool == Tool::Codex {
            if let Some(program) = &place.native_program {
                launch.argv[0] = program.to_string_lossy().into_owned();
            }
            if let Some(cwd) = &place.cwd {
                launch.argv.extend(["--cd".into(), cwd.to_string_lossy().into_owned()]);
            }
        }
        if tool == Tool::Claude {
            // Owner messages reach Claude through its own prompt interface,
            // by the plugin this folder holds (`claude_input.rs`).
            let plugin = claude_input::materialize(&self.opts.state_dir)?;
            launch.argv.extend(["--plugin-dir".into(), plugin.to_string_lossy().into_owned()]);
        }
        if resume.is_none() {
            launch.argv.extend(place.args.iter().cloned());
            if let Some(prompt) = place.prompt.as_ref().filter(|p| !p.trim().is_empty()) {
                // After `--` where the program takes it, so a message that
                // starts with a dash is never read as an option.
                if matches!(tool, Tool::Claude | Tool::Codex | Tool::Dynamic(_)) {
                    launch.argv.push("--".to_string());
                }
                launch.argv.push(prompt.clone());
            }
        } else if let Some(prompt) = place.prompt.as_ref().filter(|p| !p.trim().is_empty()) {
            // A resumed conversation's next message goes in through the
            // provider's own resume command (`claude --resume ID PROMPT`,
            // `codex resume ID PROMPT`), never typed into its terminal.
            anyhow::ensure!(
                matches!(tool, Tool::Claude | Tool::Codex),
                "{} cannot take a message with its resume command",
                tool.label()
            );
            launch.argv.push("--".to_string());
            launch.argv.push(prompt.clone());
        }
        Ok(launch)
    }

    fn agent_command(&self, tool: Tool, resume: Option<&str>) -> Result<Launch> {
        crate::providers::native::launch(&self.opts.config, &self.opts.exe, &self.opts.codex_dir, tool, resume)
    }

    fn spawn_hosted(
        &self,
        id: u64,
        tool: Tool,
        resume: Option<&str>,
        start: &StartModel,
        place: &Placement,
        spawned_ms: u64,
    ) -> Result<(Hosted, Launch)> {
        if tool.is_agent() && place.resource_managed {
            anyhow::ensure!(
                self.scheduler.settings.probe.read().admits(),
                "Start held: memory or CPU headroom is short; retry when resources are free"
            );
        }
        let mut launch = self.launch_for(tool, resume, place)?;
        let flags = if matches!(tool, Tool::Dynamic(_)) {
            self.opts
                .config
                .providers
                .iter()
                .find(|p| p.id == tool.key())
                .context("provider is not registered on this host")?
                .flags(start)?
        } else if tool.is_agent() {
            model_flags(tool, start)?
        } else {
            Vec::new()
        };
        if !flags.is_empty() && !launch.argv.is_empty() {
            // A configured command may use a launcher such as uv or npx.
            // Its own prefix arguments precede the agent's model options.
            let at = if matches!(tool, Tool::Dynamic(_)) {
                self.opts.config.providers.iter().find(|p| p.id == tool.key()).map_or(1, |p| p.command.len())
            } else {
                1
            };
            launch.argv.splice(at..at, flags);
        }
        if !place.guard.is_empty() {
            let context = place.chore.as_ref().context("registered chore guard context missing")?;
            let mut argv = context.guard(&place.guard);
            argv.push("--".into());
            argv.extend(launch.argv.clone());
            launch.argv = argv;
        }
        let token = self.attention_spawn_identity(id, tool, spawned_ms, &launch.argv)?;
        let argv = &launch.argv;
        let mut env_set = vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ];
        env_set.push(("CEPHEUS_ATTENTION_SPAWNED_MS".into(), spawned_ms.to_string()));
        env_set.push(("CEPHEUS_ATTENTION_TOKEN".into(), token.unwrap_or_default()));
        if tool == Tool::Claude {
            // The program Claude's owner input plugin runs (`claude_input.rs`).
            env_set.push(("CEPHEUS_OWNER_INPUT".into(), self.opts.exe.to_string_lossy().into_owned()));
        }
        let permission = if tool == Tool::Claude && self.opts.config.claude_hooks {
            attention_native::materialize(&self.opts.state_dir).ok()
        } else {
            None
        };
        let mut native_argv = String::new();
        let mut native_token = String::new();
        if let Some(helper) = &permission {
            native_argv = serde_json::to_string(&attention_native::argv(helper)?)?;
            native_token = attention_native::spawn_identity(&self.opts.state_dir, id, spawned_ms, helper)?;
        }
        env_set.push((
            "CEPHEUS_PERMISSION_HOOK".into(),
            permission.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        ));
        env_set.push(("CEPHEUS_PERMISSION_HOOK_TOKEN".into(), native_token));
        env_set.push(("CEPHEUS_PERMISSION_HOOK_ARGV".into(), native_argv));
        let spawn_cwd = place.cwd.as_deref().unwrap_or(&self.opts.start_dir);
        env_set.push((
            "CEPHEUS_PROMPT_AVOIDANCE_SKIP".into(),
            if crate::extensions::conventions::repository_owns(
                self.opts.config.conventions.prompt_avoidance.as_ref(),
                tool,
                spawn_cwd,
            ) {
                "1"
            } else {
                ""
            }
            .into(),
        ));
        if tool.is_agent() {
            env_set.extend(crate::client_temp::vars(&format!("s{id}"))?);
        }
        if let Some(chore) = &place.chore {
            env_set.extend(chore.env());
        }
        if let Some(account) = place.account.as_deref() {
            let settings = crate::providers::accounts::Settings::from_config(&self.opts.config)?;
            let profile = settings.profile(account, tool)?;
            anyhow::ensure!(
                place.account_root.as_ref() == Some(&profile.directory),
                "account directory changed; restore its original profile before resuming"
            );
            env_set.extend(profile.launch_env());
        }
        if tool == Tool::Codex && place.native_program.is_some() {
            env_set.push((
                "CODEX_HOME".into(),
                place.account_root.as_deref().unwrap_or(&self.opts.codex_dir).to_string_lossy().into_owned(),
            ));
        }
        // Under both names: a session's tools may come from a checkout or a
        // host install from before the rename, which read only the old one.
        let socket = self.opts.socket.to_string_lossy().into_owned();
        for (suffix, value) in [("SESSION", id.to_string()), ("SOCKET", socket)] {
            for name in paths::env_names(suffix) {
                env_set.push((name, value.clone()));
            }
        }
        if tool == Tool::Shell {
            // A service started by systemd has no `SHELL`; zsh plugins and
            // programs started from the terminal look for it.
            if let Some(shell) = argv.first().filter(|p| p.starts_with('/')) {
                env_set.push(("SHELL".to_string(), shell.clone()));
            }
        }
        // The existing owner-controlled CLI environment remains trusted. The
        // native PermissionRequest exec descriptor pins its own loader env
        // after workspace settings without changing hosted CLI dependencies.
        let mut env_remove = child_env_remove(std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()));
        if tool == Tool::Codex && place.native_program.is_some() {
            env_remove.extend(crate::providers::messages::MANAGED_CODEX_ENV.iter().map(|key| key.to_string()));
            env_set.extend(place.native_environment.iter().cloned());
        }
        let cwd = place.cwd.as_deref().unwrap_or(&self.opts.start_dir);
        anyhow::ensure!(cwd.is_dir(), "Working directory {} is unavailable; launch is held", cwd.display());
        let hosted = Hosted::spawn(
            SpawnSpec {
                id,
                argv,
                cwd,
                env_set: &env_set,
                env_remove: &env_remove,
                cols: self.view.0,
                rows: self.view.1,
                scrollback: self.opts.config.scrollback,
            },
            self.tx.clone(),
        )?;
        Ok((hosted, launch))
    }

    /// What a fresh start of `tool` runs with: the model and effort it names,
    /// else the saved launch settings (`providers::launch`, `:launch`).
    /// `account` is the account folder it starts in, whose model list
    /// decides whether a saved effort suits its model.
    pub(super) fn launch_start(&self, tool: Tool, start: &StartModel, account: Option<&Path>) -> Result<StartModel> {
        if !self.opts.config.plugin_enabled(launch::PLUGIN) {
            return Ok(start.clone());
        }
        let settings = launch::load(&launch::path(&self.opts.state_dir))?;
        let mut homes = self.model_homes();
        if let Some(account) = account {
            match tool {
                Tool::Claude => homes.claude = account.into(),
                Tool::Codex => homes.codex = account.into(),
                _ => {}
            }
        }
        Ok(launch::resolve(tool, start, &settings, || crate::remote::models::all(&homes)))
    }

    /// Where this service's programs keep their model lists.
    pub(super) fn model_homes(&self) -> crate::remote::models::Homes {
        crate::remote::models::Homes {
            claude: self.opts.claude_dir.clone(),
            codex: self.opts.codex_dir.clone(),
            grok: self.opts.grok_home.clone(),
            antigravity: self.opts.antigravity_dir.clone(),
            state: self.opts.state_dir.clone(),
        }
    }

    /// Fetch Antigravity's and opencode's model lists again on a thread when
    /// the last fetch is older than `age`. The real service only: unit tests
    /// never run the owner's programs.
    pub(super) fn refresh_model_lists(&mut self, now: Instant, age: Duration) {
        if !self.opts.monitor
            || self.shutdown_at.is_some()
            || self.model_lists_busy.load(Ordering::SeqCst)
            || self.model_lists_asked.is_some_and(|at| now.duration_since(at) < age)
        {
            return;
        }
        self.model_lists_asked = Some(now);
        self.model_lists_busy.store(true, Ordering::SeqCst);
        let busy = self.model_lists_busy.clone();
        let (config, cwd, state) = (self.opts.config.clone(), self.opts.start_dir.clone(), self.opts.state_dir.clone());
        let started = thread::Builder::new().name("model-lists".into()).spawn(move || {
            if let Err(error) = crate::remote::models::refresh(&config, &cwd, &state) {
                eprintln!("cepheus: could not save the programs' model lists: {error:#}");
            }
            busy.store(false, Ordering::SeqCst);
        });
        if started.is_err() {
            self.model_lists_busy.store(false, Ordering::SeqCst);
        }
    }

    #[cfg(test)]
    fn create(&mut self, tool: Tool, resume: Option<String>) -> Result<u64> {
        self.create_with(tool, resume, &StartModel::default(), &Placement::default())
    }

    fn reconcile_launch(&mut self, key: &str, fingerprint: Option<&String>) -> Result<Option<u64>> {
        if let Some(receipt) = self.work_retired_receipt(key)? {
            anyhow::ensure!(
                receipt.fingerprint.as_ref() == fingerprint,
                "interactive launch key belongs to a different payload"
            );
            return Ok(Some(receipt.session));
        }
        let Some(entry) = self.registry.sessions.iter().find(|e| e.launch_key.as_deref() == Some(key)).cloned() else {
            self.work_validate_new_key(key)?;
            return Ok(None);
        };
        anyhow::ensure!(
            entry.launch_fingerprint.as_ref() == fingerprint,
            "interactive launch key belongs to a different payload"
        );
        if entry.launch_pending && self.sessions.get(&entry.id).is_some_and(|s| s.alive) {
            let index = self.registry.sessions.iter().position(|e| e.id == entry.id).unwrap();
            self.registry.sessions[index].launch_pending = false;
            if let Err(e) = crate::job_sessions::store::write(&self.registry_path, &self.registry) {
                self.registry.sessions[index].launch_pending = true;
                return Err(e);
            }
        } else {
            anyhow::ensure!(!entry.launch_pending, "interactive launch outcome unknown; reserved session {}", entry.id);
        }
        Ok(Some(entry.id))
    }

    fn create_with(
        &mut self,
        tool: Tool,
        resume: Option<String>,
        start: &StartModel,
        place: &Placement,
    ) -> Result<u64> {
        let mut effective = place.clone();
        if let Some(account) = effective.account.as_deref() {
            let accounts = crate::providers::accounts::Settings::from_config(&self.opts.config)?;
            let profile = accounts.profile(account, tool)?;
            anyhow::ensure!(
                effective.account_root.as_ref().is_none_or(|root| root == &profile.directory),
                "account directory changed"
            );
            effective.account_root = Some(profile.directory.clone());
        }
        let place = &effective;
        // A fresh conversation takes the saved launch settings for what its
        // start left unset; a resumed one keeps its own.
        let fresh;
        let start = if resume.is_none() && tool.is_agent() {
            fresh = self.launch_start(tool, start, place.account_root.as_deref())?;
            &fresh
        } else {
            start
        };
        if let Some(id) = &resume {
            anyhow::ensure!(valid_conversation_id(id), "invalid conversation id");
        }
        if let Some(dir) = &place.cwd {
            anyhow::ensure!(dir.is_absolute() && dir.is_dir(), "{} is not a directory here", dir.display());
        }
        anyhow::ensure!(
            place.command.is_empty() || tool == Tool::Shell,
            "only a terminal session runs a command of its own"
        );
        if let Some(key) = &place.launch_key {
            if let Some(id) = self.reconcile_launch(key, place.launch_fingerprint.as_ref())? {
                return Ok(id);
            }
        }
        // Connecting a cloud provider is idempotent across desktop and phone
        // clients. The service owns this decision before spawning either PTY.
        if self.opts.config.providers.iter().any(|p| p.id == tool.key() && p.cloud) {
            anyhow::ensure!(
                place.launch_key.is_none(),
                "Cloud providers do not support new queued interactive sessions; connect manually."
            );
            if let Some(session) = self.sessions.values().find(|s| s.alive && s.tool == tool) {
                return Ok(session.id);
            }
        }
        let id = self.registry.next_id.max(1);
        if let Some(key) = &place.launch_key {
            let mut entry = Entry::started(
                id,
                tool,
                None,
                place.name.clone().unwrap_or_else(|| tool.label().into()),
                util::now_secs(),
            );
            entry.launch_key = Some(key.clone());
            entry.launch_fingerprint = place.launch_fingerprint.clone();
            entry.launch_pending = true;
            entry.cwd = Some(place.cwd.as_deref().unwrap_or(&self.opts.start_dir).to_string_lossy().into_owned());
            self.registry.next_id = id + 1;
            self.registry.sessions.push(entry);
            // This is a fallible pre-spawn barrier, including parent-directory fsync.
            // A failure never reaches the PTY launcher.
            if let Err(error) = crate::job_sessions::store::write(&self.registry_path, &self.registry) {
                self.registry.sessions.retain(|e| e.id != id);
                self.registry.next_id = id;
                return Err(error);
            }
        }
        let now_ms = util::now_ms();
        let (hosted, launch) = self.spawn_hosted(id, tool, resume.as_deref(), start, place, now_ms)?;
        if resume.is_none() {
            self.reserve_placement(None);
        }
        let conversation = launch.conversation;
        self.registry.next_id = id + 1;
        let now = util::now_secs();
        let mut tracker = self.new_tracker(tool, launch.attached, now_ms);
        if tool == Tool::Shell {
            // A terminal has no turns: it is idle, never "working".
            tracker.status = Status::Idle;
        }
        let name = place.name.as_deref().map(clean_name).filter(|n| !n.is_empty());
        let title = name.clone().unwrap_or_else(|| tool.label().to_string());
        // The exact directory is recorded even when it is the start
        // directory, so a later change to that setting never moves a
        // session that is restored or reopened.
        let cwd_text = Some(place.cwd.as_deref().unwrap_or(&self.opts.start_dir).to_string_lossy().into_owned());
        self.sessions.insert(
            id,
            Sess {
                account_root: place.account_root.clone(),
                account: place.account.clone(),
                id,
                tool,
                hosted: Some(hosted),
                alive: true,
                exit_code: None,
                conversation: conversation.clone(),
                term_title: None,
                facts: SessionFacts::default(),
                tracker,
                created_at: now,
                spawned_ms: now_ms,
                closing: None,
                // A scratch terminal goes when its shell exits.
                remove_when_ended: tool == Tool::Shell,
                title: title.clone(),
                name: name.clone(),
                retry: AutoContinue::default(),
                screen_read: None,
                update_notice: false,
                left: Vec::new(),
                chat: ChatWatch::default(),
                inputs: 0,
                recovery: recovery::ClientRecovery::default(),
                owner_draft: resume.is_some(),
                cwd: place.cwd.clone(),
                command: place.command.clone(),
                job: place.job.clone(),
            },
        );
        let mut entry = Entry::started(id, tool, conversation, title, now);
        entry.start_model = start.model.clone();
        entry.start_effort = start.effort.clone();
        entry.name = name;
        entry.cwd = cwd_text;
        entry.command = place.command.clone();
        entry.job = place.job.clone();
        entry.account = place.account.clone();
        entry.account_root = place.account_root.as_ref().map(|p| p.to_string_lossy().into_owned());
        entry.launch_key = place.launch_key.clone();
        entry.launch_fingerprint = place.launch_fingerprint.clone();
        if let Some(index) = self.registry.sessions.iter().position(|e| e.id == id) {
            self.registry.sessions[index] = entry;
        } else {
            self.registry.sessions.push(entry);
        }
        self.registry_dirty = true;
        if place.launch_key.is_some() {
            if let Err(error) = crate::job_sessions::store::write(&self.registry_path, &self.registry) {
                if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == id) {
                    entry.launch_pending = true;
                }
                return Err(error);
            }
            self.registry_dirty = false;
        } else {
            self.save_registry();
        }
        self.update_refs();
        self.last_status = Instant::now() - STATUS_INTERVAL;
        Ok(id)
    }

    fn reopen(&mut self, id: u64) -> Result<()> {
        self.reopen_with(id, None)
    }

    /// Start an ended session again in its own conversation and directory.
    /// `prompt` is a message its provider's resume command submits at start.
    fn reopen_with(&mut self, id: u64, prompt: Option<String>) -> Result<()> {
        let s = self.sessions.get(&id).context("no such session")?;
        if s.alive {
            return Ok(());
        }
        let tool = s.tool;
        // A terminal starts its command again; an agent resumes its chat.
        let conversation = match tool {
            Tool::Shell => None,
            _ if s.conversation.is_none() && self.registry.sessions.iter().any(|e| e.id == id && e.recoverable()) => {
                None
            }
            _ => {
                let c = s.conversation.clone().context("this session has no recorded conversation to resume")?;
                anyhow::ensure!(valid_conversation_id(&c), "invalid conversation id");
                Some(c)
            }
        };
        anyhow::ensure!(
            prompt.is_none() || conversation.is_some(),
            "a continuation needs the session's own conversation"
        );
        let place = Placement {
            account_root: s.account_root.clone(),
            account: s.account.clone(),
            cwd: s.cwd.clone(),
            command: s.command.clone(),
            prompt,
            ..Placement::default()
        };
        let now_ms = util::now_ms();
        let (hosted, launch) =
            self.spawn_hosted(id, tool, conversation.as_deref(), &StartModel::default(), &place, now_ms)?;
        let conversation = conversation.or(launch.conversation);
        let mut tracker = self.new_tracker(tool, launch.attached, now_ms);
        if tool == Tool::Shell {
            tracker.status = Status::Idle;
        }
        if let Some(s) = self.sessions.get_mut(&id) {
            s.hosted = Some(hosted);
            s.conversation = conversation.clone();
            s.alive = true;
            s.exit_code = None;
            s.closing = None;
            s.tracker = tracker;
            s.spawned_ms = now_ms;
            s.retry = AutoContinue::default();
            s.owner_draft = true;
            s.chat = ChatWatch::default();
        }
        if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == id) {
            entry.ended_at = None;
            entry.conversation = conversation;
        }
        self.registry_dirty = true;
        self.save_registry();
        self.update_refs();
        Ok(())
    }

    fn resume(&mut self, tool: Tool, conversation: String) -> Result<u64> {
        let roots: std::collections::BTreeSet<_> = self
            .registry
            .sessions
            .iter()
            .filter(|entry| entry.tool == tool && entry.conversation.as_deref() == Some(conversation.as_str()))
            .map(|entry| entry.account_root.clone())
            .collect();
        anyhow::ensure!(
            roots.len() <= 1,
            "conversation exists in multiple account profiles; choose its original profile explicitly"
        );
        if let Some(s) =
            self.sessions.values().find(|s| s.tool == tool && s.conversation.as_deref() == Some(conversation.as_str()))
        {
            let id = s.id;
            if !s.alive {
                self.reopen(id)?;
            }
            return Ok(id);
        }
        if let Some(entry) = self
            .registry
            .sessions
            .iter()
            .find(|entry| {
                entry.tool == tool
                    && entry.conversation.as_deref() == Some(conversation.as_str())
                    && entry.account.is_some()
            })
            .cloned()
        {
            let place = Placement {
                account: entry.account,
                account_root: entry.account_root.map(PathBuf::from),
                cwd: entry.cwd.map(PathBuf::from),
                name: entry.name,
                ..Placement::default()
            };
            let id = self.create_with(tool, Some(conversation), &StartModel::default(), &place)?;
            self.registry.sessions.retain(|old| old.id != entry.id);
            self.registry_dirty = true;
            self.save_registry();
            return Ok(id);
        }
        let cwd = self
            .registry
            .sessions
            .iter()
            .find(|e| e.tool == tool && e.conversation.as_deref() == Some(&conversation))
            .and_then(|e| e.cwd.as_ref())
            .map(PathBuf::from);
        self.create_with(tool, Some(conversation), &StartModel::default(), &Placement { cwd, ..Placement::default() })
    }

    fn forget(&mut self, id: u64) -> Result<()> {
        let s = self.sessions.get(&id).context("no such session")?;
        anyhow::ensure!(!s.alive, "close the session before forgetting it");
        let preserve_profile = s.account.is_some() && has_conversation(s, &self.opts);
        // Owner close may beat the next work-queue poll. Consume the persisted
        // turn generation while this session still carries its exact identity.
        self.work_save_turn_receipts(id)?;
        if self.work_queue.is_some() {
            self.work_observe_dispatches(util::now_secs());
        }
        if let Some(q) = &self.work_queue {
            let session = &self.sessions[&id];
            anyhow::ensure!(
                !q.records.values().any(|r| r
                    .info
                    .dispatch
                    .as_ref()
                    .is_some_and(|d| d.kind == "session" && d.id == id)
                    && r.turn_anchor.is_some_and(|at| session.tracker.completed_turns > at)
                    && !matches!(r.info.state.as_str(), "done" | "failed")),
                "queue completion receipt could not be persisted before forgetting session"
            );
        }
        self.sessions.remove(&id);
        self.registry.sessions.retain(|e| e.id != id || preserve_profile);
        self.registry_dirty = true;
        self.save_registry();
        self.last_status = Instant::now() - STATUS_INTERVAL;
        Ok(())
    }

    /// Close from the deck (`Alt+x`, `ctl close`): end a live session's program and drop the session once it
    /// exits, or drop an ended one now. The conversation stays resumable. Signal shutdown closes programs but
    /// retains their records for recovery; public Stop refuses while any session is live or awaiting recovery.
    fn close_and_remove(&mut self, id: u64) {
        self.cancel_directory_change(id, "The session was closed.");
        match self.sessions.get_mut(&id) {
            Some(s) if s.alive => {
                s.remove_when_ended = true;
                self.close(id);
            }
            Some(_) => {
                let _ = self.forget(id);
            }
            None => {}
        }
    }

    fn close(&mut self, id: u64) {
        self.messages.cancel(id);
        if let Some(session) = self.sessions.get(&id) {
            session.chat.cancel_controls();
        }
        if let Some(s) = self.sessions.get_mut(&id) {
            if let (true, Some(h)) = (s.alive, &s.hosted) {
                h.signal(libc::SIGHUP);
                s.closing = Some((Instant::now(), 1));
            }
        }
    }

    fn request_shutdown(&mut self, client: u64) {
        if self.build_state().sessions.iter().any(|session| session.alive) {
            self.notice(client, crate::protocol::STOP_REFUSED_NOTICE, true);
            return;
        }
        // Cold recovery owns a session even before its next PTY starts. A
        // removed row is only history; Close may leave a stale timer behind.
        let recovering = self.registry.sessions.iter().any(|entry| {
            self.sessions.contains_key(&entry.id)
                && (entry.resume
                    || entry.boot_pending
                    || self.pending_restores.iter().any(|pending| pending.id == entry.id)
                    || self.pending_continues.iter().any(|pending| pending.id == entry.id))
        });
        if recovering {
            self.notice(client, "Cannot stop the service while session recovery is pending. Let recovery finish, or explicitly Close the saved sessions to cancel it.", true);
            return;
        }
        // The event loop owns session creation and this decision together.
        // Reply before cancellation, job, plugin or resume mutations begin.
        self.notice(client, crate::protocol::STOP_ACCEPTED_NOTICE, false);
        self.begin_shutdown(true);
    }

    /// Drain an accepted empty public Stop or a signal/system shutdown.
    /// Public Stop reaches this only after live and cold recovery checks.
    /// A signal records recovery intent before closing its running PTYs.
    fn begin_shutdown(&mut self, owner: bool) {
        if self.shutdown_at.is_some() {
            return;
        }
        if let Some(engine) = self.extensions.engine.as_mut() {
            engine.emit_json("shutdown", &Value::Null);
        }
        self.extensions.cancel.store(true, Ordering::SeqCst);
        for session in self.sessions.values() {
            session.chat.cancel_controls();
        }
        if let Some(engine) = self.extensions.engine.as_ref() {
            engine.cancel_jobs();
        }
        self.extensions.processes.stop_all();
        for id in self.pending_directories.keys().copied().collect::<Vec<_>>() {
            self.cancel_directory_change(id, "The service is stopping.");
        }
        if owner {
            if let Some(queue) = self.work_queue.as_mut() {
                let _ = queue.set_enabled(false);
                let _ = queue.stop_source_jobs();
            }
            if let Some(jobs) = self.job_sessions.as_mut() {
                let ids: Vec<_> = jobs.records.keys().copied().collect();
                for id in ids {
                    let _ = jobs.stop(id);
                }
            }
        }
        self.stop_jobs_on_shutdown = owner;
        self.note_for_resume(owner);
        self.shutdown_at = Some(Instant::now());
        self.pump_native_new();
        let ids: Vec<u64> = self.sessions.values().filter(|s| s.alive).map(|s| s.id).collect();
        for id in ids {
            self.close(id);
        }
        if let Some(dev) = self.native.dev.as_mut() {
            dev.stop();
        }
    }

    fn move_in(&mut self, cid: u64, tool: Tool, pid: u32, conversation: Option<String>) {
        let Some(outside) = self.outside.iter().find(|o| o.pid == pid && o.tool == tool).cloned() else {
            self.notice(cid, "That session is no longer running outside the deck.", true);
            return;
        };
        // Shared-server Codex holds no rollout and its resume picker leaves
        // no ID in argv. Resolve its process-owned screen on this host.
        let displayed = if tool == Tool::Codex && self.opts.config.plugin_enabled("desktop") {
            external::kitty_process_screen(pid)
                .map(|screen| crate::monitor::codex_screen_conversation(&self.opts.codex_dir, &outside.cwd, &screen))
        } else {
            None
        };
        let identified = displayed.clone().unwrap_or_else(|| outside.conversation.clone());
        let Some(identified) = identified else {
            self.notice(
                cid,
                "Its conversation could not be identified automatically. The outside session was left running.",
                true,
            );
            return;
        };
        if displayed.is_none() && conversation.as_ref().is_some_and(|id| id != &identified) {
            self.notice(
                cid,
                "Its conversation changed; try moving it again. The outside session was left running.",
                true,
            );
            return;
        }
        let conversation = identified;
        if !valid_conversation_id(&conversation) {
            self.notice(cid, "Invalid conversation id.", true);
            return;
        }
        if self.sessions.values().any(|s| s.alive && s.conversation.as_deref() == Some(conversation.as_str())) {
            self.notice(cid, "That conversation is already open in the deck.", true);
            return;
        }
        self.notice(cid, format!("Moving {} pid {pid} into the deck…", tool.label()), false);
        let tx = self.tx.clone();
        let desktop = self.opts.config.plugin_enabled("desktop");
        let started = thread::Builder::new().name("move-in".into()).spawn(move || {
            let error = end_outside(&outside, desktop).err();
            let _ = tx.send(Ev::MoveInDone { client: cid, tool, conversation, error });
        });
        if started.is_ok() {
            self.moves_in_flight += 1;
        }
    }

    fn apply_report(&mut self, report: Report) {
        let mut refs_changed = false;
        for (id, mut facts) in report.facts {
            let Some(s) = self.sessions.get_mut(&id) else { continue };
            if !s.alive {
                continue;
            }
            if facts.known_pid.is_some() && facts.known_pid != s.hosted.as_ref().and_then(|h| h.pid) {
                continue;
            }
            // The monitor gathers facts from a snapshot taken before it ran. A
            // hook that has moved the session to another conversation since
            // (Codex `/new` or `/resume`) makes them describe the old one.
            if s.conversation != facts.known_conversation {
                continue;
            }
            if s.tool == Tool::Antigravity && facts.conversation.as_ref().is_some_and(|c| s.left.contains(c)) {
                continue;
            }
            if let Some(conv) = facts.conversation.clone().filter(|c| valid_conversation_id(c)) {
                if s.enter_conversation(Some(conv.clone())) {
                    if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == id) {
                        entry.conversation = Some(conv);
                    }
                    self.registry_dirty = true;
                    refs_changed = true;
                }
            }
            if s.tool == Tool::Antigravity {
                if let Some(at) = facts.cleared_ms.filter(|at| *at >= s.spawned_ms) {
                    s.start_new_chat();
                    s.spawned_ms = at;
                    if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == id) {
                        entry.conversation = None;
                    }
                    self.registry_dirty = true;
                    refs_changed = true;
                    continue;
                }
            }
            for kill in &facts.memory_kills {
                let event = format!("{}:{}:{}", id, s.conversation.as_deref().unwrap_or(""), kill.id);
                if kill.at == 0
                    || self.scheduler.memory_events.contains_key(&event)
                    || kill.at.saturating_add(2).saturating_mul(1000) < s.spawned_ms
                    || kill.at > util::now_secs().saturating_add(5)
                {
                    continue;
                }
                let key = crate::scheduler::memory_key(id, &kill.command);
                if self.scheduler.memory_dismissed.get(&key).is_some_and(|at| kill.at <= *at) {
                    continue;
                }
                self.scheduler.memory_events.insert(event, kill.at);
                if self.scheduler.reruns.len() < 1000
                    && !self.scheduler.reruns.iter().any(|r| r.session == id && r.command == kill.command)
                {
                    let authoritative = kill.authoritative
                        || crate::extensions::conventions::repository_owns(
                            self.opts.config.conventions.memory_reruns.as_ref(),
                            s.tool,
                            facts
                                .worktree
                                .as_ref()
                                .map(|w| Path::new(&w.path))
                                .or(s.cwd.as_deref())
                                .unwrap_or(&self.opts.start_dir),
                        );
                    let ticket = self.scheduler.id();
                    self.scheduler.reruns.push(crate::scheduler::Rerun {
                        cwd: kill
                            .cwd
                            .clone()
                            .or_else(|| s.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()))
                            .or_else(|| Some(self.opts.start_dir.to_string_lossy().into_owned())),
                        conversation: s.conversation.clone(),
                        id: ticket,
                        session: id,
                        command: kill.command.clone(),
                        at: kill.at,
                        authoritative,
                    });
                }
            }
            while self.scheduler.memory_events.len() > 10000 {
                if let Some(key) =
                    self.scheduler.memory_events.iter().min_by_key(|(_, at)| *at).map(|(key, _)| key.clone())
                {
                    self.scheduler.memory_events.remove(&key);
                }
            }
            if s.recovery.phase.is_none() && !s.recovery.awaiting_continue && !s.recovery.paused {
                s.tracker.log_busy(facts.busy, util::now_ms());
                s.tracker.observe_goal(facts.goal_active);
                if let Some(end) = &facts.turn_end {
                    s.tracker.turn_recorded(&end.id, end.at_ms, end.superseded);
                }
            }
            // Claude's sub-agents are what its hooks reported (`handle_hook`).
            if s.tool == Tool::Claude {
                facts.claude_agents = std::mem::take(&mut s.facts.claude_agents);
                facts.subagents = crate::activity::claude_listed(&facts.claude_agents);
            }
            s.facts = facts;
        }
        let _ = self.scheduler.save();
        self.outside = report.outside;
        self.usage = report.usage;
        if refs_changed {
            self.update_refs();
        }
    }

    /// Start the phone gateway when the config asks for it and none runs.
    /// It exits by itself when `remote` is turned off.
    fn supervise_remote(&mut self, now: Instant) {
        let Some(path) = self.opts.config_file.clone() else { return };
        if self.shutdown_at.is_some()
            || self.updating.is_some()
            || self.remote_checked.is_some_and(|at| now.duration_since(at) < REMOTE_CHECK_INTERVAL)
        {
            return;
        }
        self.remote_checked = Some(now);
        if !Config::load_from(&path).0.remote {
            return;
        }
        let dir = remote::store::remote_dir(&self.opts.state_dir);
        if remote::running(&dir) {
            self.remote_ran = true;
            self.remote_failures = 0;
            return;
        }
        // A gateway that keeps exiting before it was seen running is
        // started less often, and reported once.
        if let Some(at) = self.remote_started {
            let wait = if self.remote_failures >= 3 { REMOTE_RETRY_SLOW } else { REMOTE_RETRY };
            if now.duration_since(at) < wait {
                return;
            }
            if !self.remote_ran {
                self.remote_failures += 1;
                if self.remote_failures == 3 {
                    let log = self.opts.state_dir.join("remote.log");
                    self.log_error(&format!("The phone gateway keeps stopping; see {}", log.display()));
                }
            }
        }
        self.remote_started = Some(now);
        self.remote_ran = false;
        if let Err(err) = remote::spawn_detached(&current_exe_clean(), &self.opts.state_dir) {
            self.log_error(&format!("Could not start the phone gateway: {err:#}"));
        }
    }

    /// Read the start repository's branches on a thread now and then, and
    /// the saved scans, for the branches list and `:scans`. The real service
    /// only: unit tests leave the owner's repository and folders alone.
    fn refresh_lists(&mut self, now: Instant) {
        if !self.opts.monitor || self.shutdown_at.is_some() {
            return;
        }
        self.refresh_model_lists(now, MODEL_LISTS_INTERVAL);
        if !self.branches_busy && self.branches_asked.is_none_or(|at| now.duration_since(at) >= BRANCHES_INTERVAL) {
            self.branches_asked = Some(now);
            let dir = self.opts.start_dir.clone();
            let tx = self.tx.clone();
            self.branches_busy = thread::Builder::new()
                .name("branches".into())
                .spawn(move || {
                    let _ = tx.send(Ev::Branches(crate::gitwt::branches(&dir)));
                })
                .is_ok();
        }
        if !self.identity_busy && self.identity_asked.is_none_or(|at| now.duration_since(at) >= IDENTITY_INTERVAL) {
            self.identity_asked = Some(now);
            let dir = self.opts.start_dir.clone();
            let tx = self.tx.clone();
            self.identity_busy = thread::Builder::new()
                .name("repository-identity".into())
                .spawn(move || {
                    let _ = tx.send(Ev::RepositoryIdentity(crate::scheduler::repository(&dir)));
                })
                .is_ok();
        }
        self.refresh_scan_list(now);
    }

    fn housekeeping(&mut self, now: Instant) {
        self.upkeep_claude_policy(now);
        self.upkeep_plugin_config(now);
        extensions::native::tick(self, now);
        self.check_update(now);
        self.move_replaced_programs(now);
        self.supervise_remote(now);
        self.supervise_peer(now);
        self.sync_hosts(now);
        self.host_outages_due(now);
        self.lists_due(now);
        self.broadcasts_due(now);
        self.refresh_lists(now);
        self.restores_due(now);
        self.continues_due(now);
        self.checkpoint_recovery(false);
        self.sweep_client_temp(now);
        if now.duration_since(self.last_sync_check) >= SYNC_CHECK_INTERVAL {
            self.last_sync_check = now;
            for s in self.sessions.values().filter(|s| s.alive) {
                if let Some(h) = &s.hosted {
                    h.check_sync_timeout(now);
                }
            }
        }
        for s in self.sessions.values_mut() {
            let (Some((since, stage)), true, Some(h)) = (s.closing, s.alive, s.hosted.as_ref()) else { continue };
            let elapsed = now.duration_since(since);
            if stage == 1 && elapsed > Duration::from_secs(3) {
                h.signal(libc::SIGTERM);
                s.closing = Some((since, 2));
            } else if stage == 2 && elapsed > Duration::from_secs(6) {
                h.signal(libc::SIGKILL);
                s.closing = Some((since, 3));
            }
        }
    }

    /// Remove the temporary folders of agent clients that have ended
    /// (`client_temp::sweep`), on a thread of its own: a client's folder can
    /// hold a large build's files. The first runs at start, so folders left by
    /// clients that ended while no service ran go too.
    fn sweep_client_temp(&mut self, now: Instant) {
        if now < self.client_temp_due || self.client_temp_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        self.client_temp_due = now + CLIENT_TEMP_SWEEP_INTERVAL;
        let busy = self.client_temp_busy.clone();
        let started = thread::Builder::new().name("client-temp".into()).spawn(move || {
            match crate::client_temp::sweep(&paths::client_temp_root(), util::now_ms()) {
                Ok(swept) => {
                    for failure in swept.failed {
                        eprintln!("cepheus: {failure}");
                    }
                }
                Err(error) => eprintln!("cepheus: sweeping agent clients' temporary folders: {error:#}"),
            }
            busy.store(false, Ordering::SeqCst);
        });
        if started.is_err() {
            self.client_temp_busy.store(false, Ordering::SeqCst);
        }
    }

    fn pump_frames(&mut self) {
        self.pump_host_frames();
        let mut cache: HashMap<(u64, bool), Vec<u8>> = HashMap::new();
        for c in self.clients.values_mut() {
            let Some(id) = c.attached else { continue };
            let Some(s) = self.sessions.get(&id) else { continue };
            let Some(h) = s.hosted.as_ref() else { continue };
            let generation = h.generation();
            if generation == c.sent_gen || c.frame_busy.load(Ordering::SeqCst) {
                continue;
            }
            let local_history = c.role == ClientRole::Tui && c.version.is_some_and(|v| v >= screen::HISTORY_PROTOCOL);
            let buf = cache
                .entry((id, local_history))
                .or_insert_with(|| {
                    let frame = {
                        let emu = h.lock();
                        let mut frame = if local_history {
                            screen::build_live_frame(&emu.term)
                        } else {
                            screen::build_frame(&emu.term)
                        };
                        frame.epoch = h.history_epoch;
                        frame.revision = emu.output_revision;
                        frame
                    };
                    encode(&ServerMsg::Frame { id, frame })
                })
                .clone();
            c.frame_busy.store(true, Ordering::SeqCst);
            if c.out.send(Out::Frame(buf)).is_ok() {
                c.sent_gen = generation;
            }
        }
    }

    fn refresh_status(&mut self, now: Instant) {
        let now_ms = util::now_ms();
        self.bind_client_rollouts();
        // Summaries that came since the last pass reach the titles below.
        extensions::native::collect(self);
        // Answer colour queries from the current theme after a theme switch.
        if now.duration_since(self.palette_at) >= PALETTE_RELOAD_INTERVAL {
            self.palette_at = now;
            self.palette = Palette::load(&self.opts.config);
        }
        let scan = now.duration_since(self.last_scan) >= SCREEN_SCAN_INTERVAL;
        if scan {
            self.last_scan = now;
        }
        let focused: HashSet<u64> = self
            .clients
            .values()
            .filter(|c| c.role == ClientRole::Tui && c.focused)
            .filter_map(|c| c.attached)
            .collect();
        let retry_usage = self.usage.clone();
        let account_settings = crate::providers::accounts::Settings::from_config(&self.opts.config).ok();
        let invalid_profiles: HashSet<u64> = self
            .sessions
            .values()
            .filter(|s| s.account.is_some())
            .filter(|s| {
                account_settings
                    .as_ref()
                    .and_then(|settings| settings.profile(s.account.as_deref().unwrap(), s.tool).ok())
                    .is_none_or(|profile| Some(&profile.directory) != s.account_root.as_ref())
            })
            .map(|s| s.id)
            .collect();
        let profile_usage: HashMap<u64, Usage> = self
            .sessions
            .values()
            .filter(|s| s.account.is_some())
            .map(|s| {
                let windows = account_settings
                    .as_ref()
                    .and_then(|settings| settings.profile(s.account.as_deref().unwrap(), s.tool).ok())
                    .filter(|profile| Some(&profile.directory) == s.account_root.as_ref())
                    .map(|p| p.usage(now_ms / 1000))
                    .unwrap_or_default();
                let usage = match s.tool {
                    Tool::Claude => Usage { claude: windows, ..Usage::default() },
                    Tool::Codex => Usage { codex: windows, ..Usage::default() },
                    _ => Usage::default(),
                };
                (s.id, usage)
            })
            .collect();
        let pending_input: HashSet<u64> = self
            .pending_submits
            .iter()
            .map(|pending| pending.id)
            .chain(self.pending_keys.iter().map(|(_, id, _)| *id))
            .chain(self.pending_reviews.iter().map(|pending| pending.id))
            .collect();
        let mut notes = Vec::new();
        let mut retries = Vec::new();
        let mut update_notices = Vec::new();
        let mut refs_changed = false;
        for s in self.sessions.values_mut() {
            if s.tool == Tool::Shell {
                // A terminal has no turns, questions or chat to follow.
                let next = if s.alive { Status::Idle } else { Status::Ended };
                if s.tracker.status != next {
                    s.tracker.status = next;
                    s.tracker.since = now_ms / 1000;
                }
                let title = s.display_title(&self.claude_agents, &mut self.native.titles);
                if title != s.title {
                    s.title = title;
                }
                continue;
            }
            if scan && s.alive {
                // A screen is locked and parsed again only once its terminal
                // changed; what it said goes on counting while it waits.
                let pid = s.hosted.as_ref().and_then(|h| h.pid);
                let read = s.hosted.as_ref().map(|h| {
                    let generation = h.generation();
                    match s.screen_read.take().filter(|read| read.generation == generation) {
                        Some(mut read) => {
                            if !crate::providers::signals_follow_screen(s.tool) {
                                read.signals = crate::providers::screen_signals(s.tool, &read.lines, pid);
                            }
                            read
                        }
                        None => ScreenRead::new(s.tool, generation, h, pid),
                    }
                });
                if let Some(read) = read {
                    let ScreenRead { lines, cursor, signals, new_chat, footer, update_notice, .. } = &read;
                    let (lines, cursor) = (lines.as_slice(), *cursor);
                    s.tracker.observe_screen(signals, now_ms);
                    if *update_notice && !s.update_notice {
                        s.update_notice = true;
                        update_notices.push(s.id);
                    }
                    // `/clear` and `/new` start a conversation no hook names
                    // until its first turn ends, or never when attached.
                    if s.tool == Tool::Codex && s.chat.started_new(*new_chat, now, now_ms) {
                        s.start_new_chat();
                        if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == s.id) {
                            entry.conversation = None;
                        }
                        self.registry_dirty = true;
                        refs_changed = true;
                    }
                    if s.tool == Tool::Codex && !footer.is_empty() && *footer != s.chat.footer {
                        s.chat.footer = footer.clone();
                        refs_changed = true;
                    }
                    let patterns = provider_automation::patterns(s.tool, &self.opts.config);
                    if !self.messages.active(s.id)
                        && !self.opts.config.auto_continue.is_empty()
                        && (!patterns.is_empty() || matches!(s.tool, Tool::Claude | Tool::Codex))
                    {
                        if invalid_profiles.contains(&s.id) {
                            s.retry.quota_reset = None;
                            s.retry.quota_unknown = true;
                        }
                        let completed_revoked_turn = s.tracker.tool_busy == Some(false)
                            && status::stopped_on_error(lines, cursor, patterns).is_some_and(|pattern| {
                                pattern.eq_ignore_ascii_case("application network permission was revoked")
                            })
                            && !signals.working;
                        let input_ready = !pending_input.contains(&s.id)
                            && s.automatic_retry_input_ready(now_ms, completed_revoked_turn);
                        if let Some(retry) = s.retry_after_error(
                            lines,
                            cursor,
                            patterns,
                            provider_automation::RetryContext {
                                now,
                                now_ms,
                                usage: profile_usage.get(&s.id).unwrap_or(&retry_usage),
                                threshold: self.scheduler.settings.quota_percent,
                                input_ready,
                            },
                        ) {
                            if matches!(retry, Retry::Typed(_)) {
                                self.pending_submits.push(PendingSubmit::new(now, s.id, 0, s.inputs));
                            }
                            retries.push(retry);
                        }
                    }
                    s.screen_read = Some(read);
                }
            }
            let title = s.display_title(&self.claude_agents, &mut self.native.titles);
            if title != s.title {
                s.title = title.clone();
                if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == s.id) {
                    entry.title = title;
                    self.registry_dirty = true;
                }
            }
            // A new chat drops the owner's name (`forget_chat`).
            if let Some(entry) = self.registry.sessions.iter_mut().find(|e| e.id == s.id && e.name != s.name) {
                entry.name = s.name.clone();
                self.registry_dirty = true;
            }
            // Trackers handed over from older builds lack the flag.
            s.tracker.persistent |= s.tool == Tool::Dynamic("dot");
            let turns = s.tracker.completed_turns;
            if let Some(note) = s.tracker.update(s.alive, focused.contains(&s.id), now_ms) {
                notes.push((s.id, s.tool.label().to_string(), s.title.clone(), note));
            }
            if s.tracker.completed_turns > turns {
                if let Some(quotas) = &self.quotas {
                    quotas.turn_ended(s.tool);
                }
            }
        }
        if refs_changed {
            self.update_refs();
        }
        self.track_working();
        self.recover_clients(now_ms);
        self.watch_scans();
        for retry in retries {
            match retry {
                Retry::Typed(text) => {
                    self.log_error(&text);
                    self.broadcast(&ServerMsg::Notice { text, error: false });
                }
                Retry::GaveUp(text) => self.announce_error(text),
            }
        }
        // A phone sees the session finished, or asking, before its alert.
        self.state_changed();
        // Once per session, when its program first says a newer version of
        // it is out; a service plugin may update it (`agent-update`).
        if let Some(engine) = self.extensions.engine.as_mut().filter(|_| !update_notices.is_empty()) {
            let sessions = self.last_state.sessions.iter();
            for session in sessions.filter(|s| s.host.is_none() && update_notices.contains(&s.id)) {
                engine.emit_json("update_available", &serde_json::to_value(session).unwrap_or_default());
            }
        }
        for (id, who, title, note) in notes {
            match note {
                Notify::NeedsYou(reason) => {
                    self.alert_remote(AlertKind::NeedsYou, Some(id), &title, &reason);
                    if self.opts.config.notifications && self.opts.config.plugin_enabled("desktop") {
                        external::notify(&format!("{who} needs you"), &format!("{title}\n{reason}"))
                    }
                }
                Notify::Finished => {
                    self.alert_remote(AlertKind::Finished, Some(id), &title, "");
                    if self.opts.config.notifications && self.opts.config.plugin_enabled("desktop") {
                        external::notify(&format!("{who} finished"), &title)
                    }
                }
            }
        }
        if self.registry_dirty && self.registry_saved.elapsed() > Duration::from_secs(2) {
            self.queue_registry_save();
        }
    }

    fn build_state(&self) -> DeckState {
        let mut state = self.own_state();
        self.merge_hosts(&mut state);
        state.hosts = self.host_infos();
        state
    }

    fn native_host(&self) -> crate::protocol::HostInfo {
        let mut cache = self.native_resources.borrow_mut();
        if cache.0.is_none_or(|at| at.elapsed() >= Duration::from_secs(2)) {
            cache.1.sample(&self.opts.start_dir);
            cache.0 = Some(Instant::now());
        }
        let resources = &cache.1.now;
        let primary = self.branches.iter().find(|branch| branch.main);
        crate::protocol::HostInfo {
            name: util::hostname(),
            label: self.opts.config.local_host_label(),
            online: true,
            role: Default::default(),
            reason: None,
            start_dir: self.opts.start_dir.to_string_lossy().into_owned(),
            stats: Some(crate::protocol::HostStats {
                cpu: resources.cpu,
                memory: resources.memory,
                disk_used: resources.disk.map(|disk| disk.used),
                disk_available: resources.disk.map(|disk| disk.available),
                worktrees: self.branches.iter().filter(|branch| !branch.main).count(),
                branch: primary.map(|branch| branch.branch.clone()),
                head: primary.and_then(|branch| branch.head.clone()),
                branches: self.branches.clone(),
            }),
        }
    }

    fn launchable_tools(&self) -> Vec<String> {
        let mut cache = self.launch_capabilities.borrow_mut();
        if cache.as_ref().is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(30)) {
            *cache = Some((Instant::now(), crate::scheduler::placement::available_tools(&self.opts.config)));
        }
        cache.as_ref().unwrap().1.clone()
    }

    /// The same order.json used by paths::order_file(), rooted in this
    /// service's state directory so native test services remain isolated.
    fn session_order_file(&self) -> PathBuf {
        self.opts.state_dir.join("order.json")
    }

    fn saved_session_order(&self) -> Vec<u64> {
        let path = self.session_order_file();
        let stamp = util::file_stamp(&path);
        let mut cached = self.session_order.borrow_mut();
        if stamp != cached.stamp {
            cached.stamp = stamp;
            let load = || -> Result<Vec<u64>> {
                const MAX_BYTES: u64 = 1024 * 1024;
                let mut bytes = Vec::new();
                fs::File::open(&path)?.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_BYTES {
                    bail!("session order file is too large");
                }
                let ids: Vec<u64> = serde_json::from_slice(&bytes)?;
                if ids.len() > crate::protocol::MAX_SESSION_ORDER {
                    bail!("too many session order entries");
                }
                let mut seen = HashSet::new();
                Ok(ids.into_iter().filter(|id| seen.insert(*id)).collect())
            };
            if let Ok(ids) = load() {
                cached.ids = ids;
            }
        }
        cached.ids.clone()
    }

    fn set_session_order(&mut self, ids: &[u64]) -> Result<()> {
        if ids.len() > crate::protocol::MAX_SESSION_ORDER {
            bail!("too many session order entries");
        }
        // A client's order can name a session that ended a moment ago; keep
        // the live ones rather than failing the whole save.
        let state = self.build_state();
        let known: HashSet<_> = state.sessions.iter().map(|s| s.id).collect();
        let live: HashSet<_> = state.sessions.iter().filter(|s| s.alive).map(|s| s.id).collect();
        let saved = self.saved_session_order();
        if ids.iter().any(|id| !known.contains(id) && !saved.contains(id)) {
            bail!("unknown session in session order");
        }
        let mut seen = HashSet::new();
        let order: Vec<_> = ids
            .iter()
            .chain(&saved)
            .copied()
            .filter(|id| live.contains(id) && seen.insert(*id))
            .take(crate::protocol::MAX_SESSION_ORDER)
            .collect();
        // Every deck sorting by itself asks for the same order; only a
        // different one is written and sent to the decks.
        if order == saved {
            return Ok(());
        }
        util::write_atomic(&self.session_order_file(), &serde_json::to_vec(&order)?)
            .context("could not write session order")?;
        *self.session_order.borrow_mut() =
            SavedSessionOrder { stamp: util::file_stamp(&self.session_order_file()), ids: order };
        self.state_changed();
        Ok(())
    }

    fn own_state(&self) -> DeckState {
        DeckState {
            attention: self.attention_snapshot(),
            attention_capable: self.attention.is_some(),
            work_queue: self.work_state(),
            work_queue_capable: self.work_queue.is_some(),
            available_tools: Some(self.launchable_tools()),
            native_host: Some(self.native_host()),

            hosts: Vec::new(),
            session_order: self.saved_session_order(),
            host_schedulers: Default::default(),
            scheduler: self.scheduler_snapshot(),
            jobs: self.job_sessions.as_ref().map(|j| j.snapshot()).unwrap_or_default(),
            job_capable: self.job_sessions.is_some(),
            processes: self.extensions.processes.snapshot(),
            providers: self.opts.config.providers.iter().map(crate::providers::Info::from).collect(),
            sessions: self
                .sessions
                .values()
                .filter(|session| !self.placed_pending.contains_key(&session.id))
                .map(|s| SessionInfo {
                    spawned_ms: s.spawned_ms,
                    account_root: s.account_root.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    metadata: {
                        let mut metadata = self.extensions.metadata.get(&s.id).cloned().unwrap_or_default();
                        metadata.insert(
                            "fleet-workspace".into(),
                            serde_json::json!({"source": self.native_identity, "session": s.id}),
                        );
                        metadata.insert("_deck_completed_turns".into(), serde_json::json!(s.tracker.completed_turns));
                        // Reserved core metadata wins over plugin annotations.
                        metadata.insert(
                            "_deck_terminal_epoch".into(),
                            serde_json::json!(s.hosted.as_ref().map(|h| h.history_epoch)),
                        );
                        metadata
                    },
                    id: s.id,
                    tool: s.tool,
                    title: if s.name.is_none() {
                        self.extensions
                            .titles
                            .get(&s.id)
                            .map(|(_, title)| title.clone())
                            .unwrap_or_else(|| s.title.clone())
                    } else {
                        s.title.clone()
                    },
                    status: s.tracker.status,
                    detail: s.detail(),
                    pid: s.hosted.as_ref().and_then(|h| h.pid).filter(|_| s.alive),
                    conversation: s.conversation.clone(),
                    worktree: s.facts.worktree.clone(),
                    containers: s.facts.containers.clone(),
                    review: s.alive && s.facts.review,
                    alive: s.alive,
                    has_screen: s.hosted.is_some(),
                    created_at: s.created_at,
                    status_since: s.tracker.since,
                    host: None,
                    subagents: if s.alive { s.facts.subagents.clone() } else { Vec::new() },
                    activity: s.facts.activity.clone().filter(|_| s.alive),
                    last_active: s.facts.last_active,
                    cwd: s.cwd.as_ref().map(|d| d.to_string_lossy().into_owned()),
                    job: s.job.clone(),
                })
                .collect(),
            outside: self.outside.clone(),
            usage: self.usage.clone(),
            start_dir: self.opts.start_dir.to_string_lossy().into_owned(),
            dev_server: self.dev_server_state(),
            errors: self.errors.borrow().iter().cloned().collect(),
            error_clears: self.error_clears.clone(),
            panels: self.panel_list(),
            scans: self.scan_infos(),
            branches: self.branches.clone(),
        }
    }

    /// The scans kept here, newest first: the running ones as followed now,
    /// then the saved ones as last read from the scans folder.
    /// The scripts' panels from every deck connected, once each: two
    /// decks running the same scripts publish the same sections.
    fn panel_list(&self) -> Vec<Panel> {
        let mut out: Vec<Panel> = Vec::new();
        for panel in self.panels.values().flatten() {
            match out.iter_mut().find(|p| p.id == panel.id) {
                Some(seen) => *seen = panel.clone(),
                None => out.push(panel.clone()),
            }
        }
        out.truncate(crate::protocol::MAX_PANELS);
        out
    }

    fn update_refs(&self) {
        let refs: Vec<HostedRef> = self
            .sessions
            .values()
            .filter(|s| s.alive && crate::providers::takes_broadcast(s.tool))
            .map(|s| HostedRef {
                account_root: s.account_root.clone(),
                id: s.id,
                tool: s.tool,
                pid: s.hosted.as_ref().and_then(|h| h.pid),
                conversation: s.conversation.clone(),
                spawned_ms: s.spawned_ms,
                alive: s.alive,
                left: s.left.clone(),
                footer: s.chat.footer.clone(),
                cwd: s.cwd.as_ref().map(|d| d.to_string_lossy().into_owned()),
            })
            .collect();
        if let Ok(mut guard) = self.hosted_refs.lock() {
            *guard = refs;
        }
    }

    /// Whether the service is in the middle of something an update would
    /// cut short: a session closing, a broadcast's Enter still due, or a
    /// session moving in.
    fn mid_action(&self) -> bool {
        self.sessions.values().any(|s| s.closing.is_some())
            || self.sessions.values().any(|s| {
                s.recovery.phase.is_some()
                    && !(s.recovery.paused && s.recovery.phase == Some(recovery::Phase::Replaying))
            })
            || self.messages.busy()
            || !self.broadcasts.pending.is_empty()
            || !self.pending_restores.is_empty()
            || !self.pending_continues.is_empty()
            || !self.pending_submits.is_empty()
            || !self.pending_keys.is_empty()
            || !self.pending_reviews.is_empty()
            || self.moves_in_flight > 0
            || !self.pending_directories.is_empty()
            || self.native.installs > 0
            || self.service_plugins_busy()
    }

    /// Begin an update once this service's binary has been replaced: stop
    /// taking connections and let events already on their way arrive for
    /// `UPDATE_DRAIN`, then hand over (`update_now`).
    fn check_update(&mut self, now: Instant) {
        if !self.opts.update || self.fds.is_none() || self.updating.is_some() || self.shutdown_at.is_some() {
            return;
        }
        let asked = self.restart_requested;
        if !asked && self.update_checked.is_some_and(|at| now.duration_since(at) < UPDATE_CHECK_INTERVAL) {
            return;
        }
        self.update_checked = Some(now);
        let found = handover::replaced_binary().or_else(|| handover::binary_at(&current_exe_clean()).filter(|_| asked));
        let Some((exe, id)) = found else {
            if asked {
                self.restart_requested = false;
                let exe = current_exe_clean();
                self.announce_error(format!("The service could not restart: {} is not a program.", exe.display()));
            }
            return;
        };
        // A restart asked for tries again a build that refused an update.
        if (!asked && self.update_failed == Some(id)) || self.mid_action() {
            return;
        }
        self.restart_requested = false;
        self.accept_paused.store(true, Ordering::SeqCst);
        self.updating = Some((now, exe, id, asked));
    }

    /// Run the new binary in this process with every session. Returns only
    /// when that did not happen; this build then carries on as before.
    fn update_now(&mut self) {
        let Some((_, exe, id, asked)) = self.updating.take() else { return };
        if self.mid_action() || self.shutdown_at.is_some() {
            self.accept_paused.store(false, Ordering::SeqCst);
            // A restart waits for whatever began meanwhile, then goes ahead.
            self.restart_requested = asked && self.shutdown_at.is_none();
            return;
        }
        let err = self.hand_over(&exe);
        self.update_failed = Some(id);
        self.accept_paused.store(false, Ordering::SeqCst);
        let what = if asked { "restart".to_string() } else { format!("update itself to {}", exe.display()) };
        self.announce_error(format!(
            "The service could not {what}: {err:#}. It keeps running this build; stop and start it to update."
        ));
    }

    /// Pause every session and write what the next build needs, then run
    /// `exe` in place of this build. Returns the reason when it did not.
    fn hand_over(&mut self, exe: &Path) -> anyhow::Error {
        let Some(fds) = self.fds else { return anyhow::anyhow!("this service has no socket to pass on") };
        let handover = match self.prepare_handover(fds) {
            Ok(handover) => handover,
            Err(err) => {
                self.resume_sessions();
                return err;
            }
        };
        let path = handover::handover_path(&self.opts.socket);
        let err = match handover.write(&path).and_then(|()| handover::check(exe, &path)) {
            Ok(()) => {
                self.registry_dirty = true;
                self.save_registry();
                let inherited: Vec<RawFd> = [fds.listener, fds.lock]
                    .into_iter()
                    .chain(handover.sessions.iter().map(|s| s.fd))
                    .chain(handover.scheduler_fds.iter().map(|(_, fd)| *fd))
                    .collect();
                let err = match inherited.iter().try_for_each(|fd| pty::set_cloexec(*fd, false)) {
                    Ok(()) => {
                        let err = std::process::Command::new(exe).arg("service").arg("--handover").arg(&path).exec();
                        anyhow::Error::new(err).context(format!("running {}", exe.display()))
                    }
                    Err(err) => anyhow::Error::new(err).context("passing the terminals on"),
                };
                for fd in inherited {
                    let _ = pty::set_cloexec(fd, true);
                }
                err
            }
            Err(err) => err,
        };
        let _ = fs::remove_file(&path);
        self.resume_sessions();
        err
    }

    /// Stop every live session's terminal threads and describe the service
    /// for the next build.
    fn prepare_handover(&mut self, fds: Fds) -> Result<Handover> {
        self.prepare_handover_at(fds, Instant::now())
    }

    fn prepare_handover_at(&mut self, fds: Fds, now: Instant) -> Result<Handover> {
        if let Some(a) = &mut self.attention {
            a.store.save()?;
        }
        let ago = |at: &Instant| now.saturating_duration_since(*at).as_millis() as u64;
        let mut sessions = Vec::new();
        for s in self.sessions.values_mut() {
            let (true, Some(h)) = (s.alive, s.hosted.as_mut()) else { continue };
            let pid = h.pid.with_context(|| format!("{} has no process id", session_label(s.tool, &s.title)))?;
            let fd = h.pause()?;
            let screen = screen::replay(&h.lock().term);
            sessions.push(SessionHandover {
                account_root: s.account_root.clone(),
                account: s.account.clone(),
                id: s.id,
                tool: s.tool,
                pid,
                fd,
                cols: h.cols,
                rows: h.rows,
                screen,
                conversation: s.conversation.clone(),
                term_title: s.term_title.clone(),
                title: s.title.clone(),
                created_at: s.created_at,
                spawned_ms: s.spawned_ms,
                inputs: Some(s.inputs),
                tracker: s.tracker.clone(),
                facts: s.facts.clone(),
                remove_when_ended: s.remove_when_ended,
                retry_seen_ago_ms: s.retry.seen.as_ref().map(ago),
                retry_sent_ago_ms: s.retry.sent.iter().map(ago).collect(),
                retry_gave_up: s.retry.gave_up,
                retry_quota_reset: s.retry.quota_reset,
                retry_quota_unknown: s.retry.quota_unknown,
                owner_draft: s.owner_draft,
                left: s.left.clone(),
                update_notice: s.update_notice,
                name: s.name.clone(),
                cwd: s.cwd.clone(),
                command: s.command.clone(),
                job: s.job.clone(),
                recovery: s.recovery.clone(),
            });
        }
        Ok(Handover {
            host_outages: self.host_outage_handover(now),
            scheduler: self.scheduler.saved(),
            scheduler_fds: self.scheduler.descriptors(),
            extensions: crate::handover::ExtensionHandover {
                processes: self.extensions.processes.snapshot(),
                metadata: self.extensions.metadata.clone(),
                titles: self.extensions.titles.clone(),
            },
            version: handover::HANDOVER_VERSION,
            listener_fd: fds.listener,
            lock_fd: fds.lock,
            view: self.view,
            errors: self.errors.borrow().iter().cloned().collect(),
            usage: self.usage.clone(),
            dev: self.native.dev.as_ref().map(|d| DevHandover {
                pid: d.pid,
                started_at: d.started_at,
                log: d.log.clone(),
                stopping: d.stopping(),
            }),
            sessions,
        })
    }

    /// Start the sessions' terminal threads again after a handover that did
    /// not happen.
    fn resume_sessions(&mut self) {
        let mut failed = Vec::new();
        for s in self.sessions.values_mut() {
            if let Some(h) = s.hosted.as_mut() {
                if let Err(err) = h.resume() {
                    failed.push(format!("{}: {err:#}", session_label(s.tool, &s.title)));
                }
            }
        }
        for text in failed {
            self.log_error(&text);
        }
    }

    /// Carry on from the previous build of this process: its sessions, dev
    /// server, error list and usage.
    fn adopt(&mut self, handover: Handover) {
        self.adopt_at(handover, Instant::now());
    }

    fn adopt_at(&mut self, handover: Handover, now: Instant) {
        self.restore_host_outages(handover.host_outages, now);
        self.scheduler.restore(handover.scheduler, &handover.scheduler_fds);
        if let Err(error) = self.extensions.processes.adopt(handover.extensions.processes, &self.opts.state_dir) {
            self.announce_error(format!("Plugin handover: {error:#}"));
        }
        self.extensions.metadata = handover.extensions.metadata;
        self.extensions.titles = handover.extensions.titles;
        self.view = handover.view;
        self.usage = handover.usage;
        *self.errors.borrow_mut() = handover.errors.into_iter().collect();
        if let Some(d) = handover.dev {
            self.native.dev = Some(DevProc::adopt(d.pid, d.started_at, d.log, d.stopping));
        }
        let at = |ms: u64| now.checked_sub(Duration::from_millis(ms));
        let scrollback = self.opts.config.scrollback;
        for h in handover.sessions {
            let spec =
                AdoptSpec { id: h.id, fd: h.fd, pid: h.pid, cols: h.cols, rows: h.rows, scrollback, screen: h.screen };
            let hosted = match Hosted::adopt(spec, self.tx.clone()) {
                Ok(hosted) => hosted,
                Err(err) => {
                    // Nothing can serve its terminal: end the program rather
                    // than leave it writing to no one.
                    // SAFETY: kill has no memory-safety preconditions; the
                    // program leads its own process group.
                    unsafe {
                        libc::kill(-(h.pid as libc::pid_t), libc::SIGHUP);
                    }
                    self.log_error(&format!(
                        "{} could not be kept across the update: {err:#}",
                        session_label(h.tool, h.name.as_deref().unwrap_or(&h.title))
                    ));
                    continue;
                }
            };
            match self.registry.sessions.iter_mut().find(|e| e.id == h.id) {
                Some(entry) => {
                    entry.ended_at = None;
                    entry.resume = false;
                    entry.conversation = h.conversation.clone();
                    entry.name = h.name.clone();
                }
                None => {
                    let mut entry = Entry::started(h.id, h.tool, h.conversation.clone(), h.title.clone(), h.created_at);
                    entry.name = h.name.clone();
                    entry.cwd = h.cwd.as_ref().map(|d| d.to_string_lossy().into_owned());
                    entry.command = h.command.clone();
                    entry.job = h.job.clone();
                    self.registry.sessions.push(entry);
                }
            }
            self.registry.next_id = self.registry.next_id.max(h.id + 1);
            self.sessions.insert(
                h.id,
                Sess {
                    account_root: h.account_root.clone(),
                    account: h.account.clone(),
                    id: h.id,
                    tool: h.tool,
                    hosted: Some(hosted),
                    alive: true,
                    exit_code: None,
                    conversation: h.conversation,
                    term_title: h.term_title,
                    facts: h.facts,
                    tracker: h.tracker,
                    created_at: h.created_at,
                    spawned_ms: h.spawned_ms,
                    closing: None,
                    remove_when_ended: h.remove_when_ended,
                    title: h.title,
                    name: h.name,
                    retry: AutoContinue {
                        seen: h.retry_seen_ago_ms.and_then(at),
                        sent: h.retry_sent_ago_ms.into_iter().filter_map(at).collect(),
                        gave_up: h.retry_gave_up,
                        quota_reset: h.retry_quota_reset,
                        quota_unknown: h.retry_quota_unknown,
                        // Adopted terminals restart their output revision at zero;
                        // require fresh output independently of input custody.
                        input_revision: Some(0),
                    },
                    screen_read: None,
                    update_notice: h.update_notice,
                    left: h.left,
                    chat: ChatWatch::default(),
                    inputs: h.inputs.unwrap_or_default(),
                    recovery: h.recovery,
                    owner_draft: h.owner_draft,
                    cwd: h.cwd,
                    command: h.command,
                    job: h.job,
                },
            );
        }
        self.registry_dirty = true;
        self.save_registry();
        self.update_refs();
    }

    /// Make the session list durable before returning: a barrier for input
    /// whose recovery record must survive a crash before an agent gets it.
    fn save_registry(&mut self) -> bool {
        self.recovery_snapshot_at = Instant::now();
        self.snapshot_recovery();
        if !self.registry_dirty && self.registry_writer.in_flight == 0 {
            return true;
        }
        let saved = match self.registry.to_bytes().map_err(|e| e.to_string()) {
            Ok(bytes) => self.registry_writer.barrier(bytes),
            Err(err) => Err(err),
        };
        let saved = match saved {
            Ok(()) => {
                self.registry_dirty = false;
                true
            }
            Err(err) => {
                eprintln!("cepheus: saving {}: {err}", self.registry_path.display());
                false
            }
        };
        self.registry_saved = Instant::now();
        saved
    }

    /// Hand the session list to the registry writer without waiting: draft
    /// checkpoints and status changes never hold up the loop on a disk sync.
    fn queue_registry_save(&mut self) {
        self.recovery_snapshot_at = Instant::now();
        self.snapshot_recovery();
        if !self.registry_dirty {
            return;
        }
        let bytes = match self.registry.to_bytes() {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("cepheus: saving {}: {err}", self.registry_path.display());
                return;
            }
        };
        if self.registry_writer.current(&bytes) {
            self.registry_dirty = false;
            return;
        }
        match self.registry_writer.queue(bytes) {
            Ok(()) => self.registry_dirty = false,
            Err(err) => eprintln!("cepheus: saving {}: {err}", self.registry_path.display()),
        }
        self.registry_saved = Instant::now();
    }
}

/// End a session running outside the deck so its conversation can be
/// resumed inside: hang up, then terminate, then close its kitty window.
fn end_outside(o: &OutsideInfo, desktop: bool) -> std::result::Result<(), String> {
    if !procs::alive_with_start(o.pid, Some(o.start_ticks)) {
        return Ok(());
    }
    let env = procs::environ(o.pid);
    let kitty = env.get("KITTY_LISTEN_ON").cloned().zip(env.get("KITTY_WINDOW_ID").cloned());
    for (signal, wait) in [(libc::SIGHUP, 4000u64), (libc::SIGTERM, 3000)] {
        if !procs::alive_with_start(o.pid, Some(o.start_ticks)) {
            break;
        }
        // SAFETY: kill has no memory-safety preconditions; the start time
        // check above guards against a reused pid.
        unsafe {
            libc::kill(o.pid as libc::pid_t, signal);
        }
        let deadline = Instant::now() + Duration::from_millis(wait);
        while Instant::now() < deadline && procs::alive_with_start(o.pid, Some(o.start_ticks)) {
            thread::sleep(Duration::from_millis(100));
        }
    }
    if procs::alive_with_start(o.pid, Some(o.start_ticks)) {
        return Err(format!("pid {} did not exit", o.pid));
    }
    if let Some((listen_on, window)) = kitty.filter(|_| desktop) {
        external::kitty_close_window(&listen_on, &window);
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/service.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "tests/service_stop.rs"]
mod stop_tests;

#[cfg(test)]
#[path = "tests/broadcast_scope.rs"]
mod broadcast_scope_tests;

#[cfg(test)]
#[path = "tests/provider_service.rs"]
mod provider_tests;

#[cfg(test)]
#[path = "tests/session_clear.rs"]
mod clear_tests;

#[cfg(test)]
#[path = "tests/session_placement.rs"]
mod placement_tests;

#[cfg(test)]
#[path = "tests/directory.rs"]
mod directory_tests;

#[cfg(test)]
#[path = "tests/program_restart.rs"]
mod program_restart_tests;

#[cfg(test)]
#[path = "tests/native_messages.rs"]
mod native_message_tests;
#[cfg(test)]
#[path = "tests/native_new.rs"]
mod native_new_tests;

#[cfg(test)]
#[path = "tests/message_delivery.rs"]
mod message_delivery_tests;

#[cfg(test)]
#[path = "tests/service_fleet_interface.rs"]
mod fleet_interface_tests;

#[cfg(test)]
#[path = "tests/service_order.rs"]
mod order_tests;

#[cfg(test)]
#[path = "tests/boot_recovery.rs"]
mod boot_recovery_tests;

#[cfg(test)]
#[path = "tests/conversation_search.rs"]
mod conversation_search_tests;

#[cfg(test)]
#[path = "tests/plugin_config.rs"]
mod plugin_config_tests;
