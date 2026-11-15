//! The terminal UI: a sidebar of sessions and usage beside the selected
//! session's real screen, driven like Neovim: Normal mode picks (and `:q`),
//! Insert mode types into the session, and Ctrl-w moves between the two as it
//! moves between Neovim windows. `:` runs actions from the list; optional
//! hotkeys come from the config file (`hotkeys.rs`).

use std::collections::HashMap;
use std::io::{self, BufReader, Stdout, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange,
    EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::config::Config;
#[cfg(test)]
use crate::external;
use crate::gitwt::{self, BranchStatus};
use crate::hotkeys::{Chord, Hotkey, Keymap};
use crate::pairing::Pairing;
#[cfg(test)]
use crate::protocol::OutsideInfo;
use crate::protocol::{
    read_msg, session_label, write_msg, ClientMsg, ClientRole, Conversation, DeckState, HostInfo, LogEntry, ScrollOp,
    ServerMsg, SessionInfo, StatePatch, Status, Tool, PROTOCOL_VERSION,
};
use crate::providers::launch;
use crate::remote::models::{self as model_list, ToolModels};
use crate::resources::Sampler;
use crate::screen::{ScreenFrame, M_ALTERNATE_SCROLL, M_ALT_SCREEN, M_MOUSE_ANY};
use crate::selection::TextSelection;
use crate::theme::Theme;
use crate::{client, input, keys, paths, remote, util, view};

pub mod attention;
pub(crate) mod attention_commands;
mod attention_store;
mod command_history;
mod commands;
pub mod dot_chat;
pub mod job_sessions;
pub mod line_edit;
mod navigation;
pub mod phone;
mod scripting;
mod scroll;
mod session_scroll;
pub mod work_queue;

pub use commands::{choose_matches, choose_snap, AskRendered, BranchRow, Completion};
pub use navigation::DialogViewport;

// Snapshots remain inline to preserve the existing native API.
#[allow(clippy::large_enum_variant)]
pub enum AppEv {
    /// Connection and input events carry the registry generation they belong to.
    Linked(u64, Box<AppEv>),
    Server(ServerMsg),
    Input(Event),
    Disconnected,
    /// A new connection after the old one closed, as it does when the
    /// service updates itself in place (`handover.rs`).
    Reconnected(UnixStream),
    /// No new connection: the service has stopped.
    ReconnectFailed,
    /// An older service stopped for a restart did not come back; why.
    RestartFailed(String),
    /// Something the deck did on a thread of its own failed: kept for F2.
    Error(String),
    /// A script's job finished (`lua.rs`).
    Lua(crate::lua::JobDone),
    /// A quick question's answer (`ask.rs`).
    Answered(crate::ask::Asked),
    /// The answer so far, and the tool its program is using, as it streams.
    AskProgress {
        text: String,
        activity: Option<String>,
    },
}

/// Opens a connection to the service; tests pass their own.
pub type Connector = Arc<dyn Fn() -> io::Result<UnixStream> + Send + Sync>;

/// How the deck reaches the service again when its connection closes.
pub struct Link {
    pub tx: Sender<AppEv>,
    pub connect: Connector,
    /// Connects, starting the service when none answers: after an older
    /// service was stopped to restart it.
    pub start: Connector,
}

/// How long the deck waits for an older service it stopped to exit.
const OLD_SERVICE_EXIT_WAIT: Duration = Duration::from_secs(30);

/// A restart of the service the deck asked for.
struct Restart {
    at: Instant,
    /// The service's process when it was asked.
    pid: u32,
    /// An older service, which cannot restart in place, was stopped: the
    /// sessions to reopen once the new one lists them. None in place.
    reopen: Option<Vec<u64>>,
    /// The new service has welcomed the deck.
    welcomed: bool,
}

/// An error-clear request remains pending until the service confirms it. Re-send on a new connection if the
/// previous connection closed before acknowledgement.
struct PendingClear {
    /// The service's clear counters when the owner cleared.
    over: std::collections::BTreeMap<String, u64>,
    /// The current connection carried the request.
    sent: bool,
}

impl PendingClear {
    /// `clears` show this clear or a later one applied; either emptied the
    /// errors the owner saw.
    fn applied_in(&self, clears: &std::collections::BTreeMap<String, u64>) -> bool {
        *clears != self.over && self.over.iter().all(|(origin, count)| clears.get(origin).is_some_and(|c| c >= count))
    }
}

/// How long a restart in place may take before a reconnect is no longer
/// taken for its end.
const RESTART_WAIT: Duration = Duration::from_secs(60);

/// How long the deck keeps trying to reconnect.
const RECONNECT_WAIT: Duration = Duration::from_secs(10);
/// At most this many reconnects a minute, so a service that keeps dropping
/// the deck ends it instead of looping.
const RECONNECT_LIMIT: usize = 3;

/// Carry one connection: the service's messages go to `tx` as events, and
/// what is sent on the returned channel goes to the service, starting with
/// the deck's hello.
fn open_link(stream: UnixStream, tx: Sender<AppEv>, generation: u64) -> io::Result<(Sender<ClientMsg>, UnixStream)> {
    let control = stream.try_clone()?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    thread::Builder::new().name("deck-read".into()).spawn(move || loop {
        match read_msg::<_, ServerMsg>(&mut reader) {
            Ok(Some(msg)) => {
                if tx.send(AppEv::Linked(generation, Box::new(AppEv::Server(msg)))).is_err() {
                    break;
                }
            }
            _ => {
                let _ = tx.send(AppEv::Linked(generation, Box::new(AppEv::Disconnected)));
                break;
            }
        }
    })?;
    let (out_tx, out_rx) = mpsc::channel::<ClientMsg>();
    thread::Builder::new().name("deck-write".into()).spawn(move || {
        for msg in out_rx {
            if write_msg(&mut writer, &msg).is_err() {
                break;
            }
        }
    })?;
    let _ = out_tx.send(ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Tui });
    Ok((out_tx, control))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sel {
    Session(u64),
    Job(u64),
    // Legacy outside selections remain covered by compatibility fixtures only.
    #[cfg(test)]
    Outside(u32),
}

/// Modes as in the owner's Neovim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// The deck has the keys: arrows pick a row, Enter opens it.
    Normal,
    /// The selected live session has every key, Esc included, except the
    /// Ctrl-w window keys, history page keys and configured hotkeys.
    Insert,
    /// A `:` command is being typed.
    Command,
}

#[derive(Clone, Debug)]
pub enum Action {
    Shutdown,
    /// Stop an older service, start it again and reopen its sessions.
    Restart,
}

pub enum Dialog {
    Attention {
        tab: attention::Tab,
        sel: usize,
    },
    AttentionSend,
    WorkQueue {
        tab: work_queue::Tab,
        sel: usize,
    },
    /// The program highlighted, and for one that offers them the model and
    /// effort chosen (0 is the program's own default).
    New {
        sel: usize,
        model: usize,
        effort: usize,
    },
    Confirm {
        text: String,
        action: Action,
    },
    Picker {
        items: Vec<Conversation>,
        sel: usize,
        filter: String,
        loading: bool,
        search_due: Option<Instant>,
        search_request: Option<u64>,
    },
    /// One message for many sessions: the text and each live session with
    /// whether it is included.
    Broadcast {
        text: String,
        targets: Vec<(u64, bool)>,
        sel: usize,
    },
    Help,
    /// The error list (F2), scrolled down by `scroll` entries.
    Errors {
        scroll: usize,
    },
    /// Every machine's CPU, memory and disk (`:machines`).
    Machines,
    /// What each model has left of its limits (`:usage`).
    Usage,
    /// Pairing the phone app (`:pair`).
    Pair(Box<Pairing>),
    /// Every branch on every host (F3): the one highlighted, and the words
    /// typed to find one.
    Branches {
        sel: usize,
        filter: String,
    },
    /// Quick questions (F4): the question typed, who answers it (none: the
    /// deck decides) and how far up the answers are scrolled.
    Ask {
        draft: crate::edit::Draft,
        route: Option<crate::ask::Route>,
        scroll: usize,
    },
    /// The security scans (`:scans`).
    Scans {
        sel: usize,
    },
    /// Lines to read: what an install would do or did on each host, the
    /// skills, the hosts, the options. `confirm` is sent on `y`; `waiting`
    /// hosts have yet to answer.
    Report {
        title: String,
        lines: Vec<String>,
        scroll: usize,
        confirm: Option<Vec<ClientMsg>>,
        waiting: usize,
    },
    /// A script's picker (`deck.ui.pick`).
    Choose {
        pick: Box<crate::lua::Pick>,
        sel: usize,
        filter: String,
        callback: u64,
    },
}

/// The programs the New dialog starts, in its order: the main two, then Antigravity, then the "Other" group.
pub fn new_tools(cfg: &Config) -> Vec<Tool> {
    crate::providers::tools()
        .into_iter()
        .filter(|tool| cfg.plugins.get(&format!("provider-{}", tool.key())) != Some(&false))
        .filter(|tool| !crate::providers::info(*tool).is_some_and(|p| p.cloud))
        .collect()
}

/// The key the New dialog gives each of `NEW_TOOLS`.
pub fn new_key(tool: Tool) -> char {
    match tool {
        Tool::Claude => 'c',
        Tool::Codex => 'x',
        Tool::Antigravity => 'a',
        Tool::Grok => 'g',
        Tool::OpenCode => 'o',
        Tool::Shell => 't',
        Tool::Dynamic(_) => '\0',
    }
}

/// The New dialog's group that begins at `tool`, by its heading ("" for a
/// group drawn with a gap and no heading).
pub fn new_group(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Antigravity => Some(""),
        Tool::Grok => Some("Other"),
        _ => None,
    }
}

/// The models a program offers in the New dialog (`remote::models`, as the
/// phone offers them), or none for a program that starts with its own.
pub fn model_choices(models: &[ToolModels], tool: Tool) -> Option<&ToolModels> {
    if !tool.is_agent() {
        return None;
    }
    let name = tool.wire_key();
    models.iter().find(|m| m.tool == name && !(m.models.is_empty() && m.efforts.is_empty()))
}

/// The efforts model choice `model` takes: that model's own, else the
/// program's. The default (0) takes the default model's.
pub fn effort_choices(choices: &ToolModels, model: usize) -> &[String] {
    let chosen = match model {
        0 => choices.default_model.as_ref().and_then(|d| choices.models.iter().find(|m| &m.id == d)),
        i => choices.models.get(i - 1),
    };
    match chosen {
        Some(m) if !m.efforts.is_empty() => &m.efforts,
        _ => &choices.efforts,
    }
}

/// "opus, high effort", for a notice.
fn model_words(model: Option<&str>, effort: Option<&str>) -> String {
    match (model, effort) {
        (Some(m), Some(e)) => format!("{m}, {e} effort"),
        (Some(m), None) => m.to_string(),
        (None, Some(e)) => format!("{e} effort"),
        (None, None) => "its default model".to_string(),
    }
}

/// How often the resource line is sampled.
const RESOURCE_INTERVAL: Duration = Duration::from_secs(2);

/// The start directory's branch, where it stands against its upstream and
/// how many worktrees sit beside it. A thread of its own reads `git` only
/// when the repository changes (`gitwt::watch`), never on a timer, so a
/// slow repository never holds up the screen.
#[derive(Default)]
pub struct GitWatch {
    pub now: Option<BranchStatus>,
    watch: Option<gitwt::watch::Watch>,
}

impl GitWatch {
    /// Take the newest reading, watching `dir` from the first call and
    /// again whenever the start directory moves. True when what the header
    /// shows changed.
    pub fn poll(&mut self, dir: &Path) -> bool {
        if self.watch.as_ref().is_none_or(|watch| watch.dir() != dir) {
            self.watch = gitwt::watch::Watch::start(dir).ok();
        }
        match self.watch.as_ref().and_then(gitwt::watch::Watch::latest) {
            Some(status) if status != self.now => {
                self.now = status;
                true
            }
            _ => false,
        }
    }
}

pub struct App {
    out: Sender<ClientMsg>,
    pub cfg: Config,
    pub state: DeckState,
    pub current: Option<Sel>,
    pub attention: attention::View,
    pub jobs: job_sessions::View,
    pub dot: dot_chat::View,
    pub work_queue: work_queue::View,
    fleet_serial: u64,
    owner_sends: HashMap<u64, u64>,
    pub frame: Option<(u64, ScreenFrame)>,
    history: session_scroll::History,
    pub selection: Option<TextSelection>,
    wheel_scroll: scroll::ScrollGesture,
    /// The last screen of each session selected before, shown again when it is selected until the service sends
    /// its screen.
    last_frames: HashMap<u64, ScreenFrame>,
    pub mode: Mode,
    /// The `:` command being typed, with its cursor.
    pub command: line_edit::LineEdit,
    pub dialog: Option<Dialog>,
    pub dialog_viewport: DialogViewport,
    pub sidebar: bool,
    pub notice: Option<(String, bool, Instant)>,
    /// Notices that were not errors, oldest first, for the phone's command
    /// results; F2 lists only errors.
    pub messages: Vec<LogEntry>,
    pub started: Instant,
    /// Sidebar rows that select something when clicked: (first line, lines,
    /// selection). The highlighted row is taller: it shows its whole title.
    pub row_hits: Vec<(u16, u16, Sel)>,
    pub theme: Theme,
    theme_checked: Instant,
    pane: Rect,
    sent_view: (u16, u16),
    sent_attach: Option<Option<u64>>,
    cursor_shape: Option<u8>,
    focused: bool,
    quit: bool,
    disconnected: bool,
    dirty: bool,
    have_state: bool,
    /// Sessions closed with :close, kept off the list whatever the service
    /// reports; the flag records that the service was asked to forget it.
    closed: HashMap<u64, bool>,
    /// When Ctrl-w (the window hotkey) was pressed and how, while the deck
    /// waits for the window key that follows it.
    pub window_pending: Option<(Instant, KeyEvent)>,
    /// The hotkeys, from the config file's `"keys"`.
    pub keys: Keymap,
    /// The config file the deck follows for changed keys, and its size and
    /// time when last read (none in tests that do not ask).
    config_file: Option<PathBuf>,
    config_stamp: Option<(u64, Option<SystemTime>)>,
    coordinator_stamp: Option<(u64, i64, i64)>,
    coordinator_target: Result<Option<crate::config::HostConfig>, String>,
    /// This machine's clipboard, which Ctrl+V pastes into a session on
    /// another host (`clipboard.rs`).
    pub clipboard: crate::clipboard::Reader,
    /// The protocol of the service this deck talks to (0 until it says),
    /// and its process.
    pub service_version: u32,
    service_pid: u32,
    /// A restart asked for and not yet over.
    restart: Option<Restart>,
    /// Why the deck ended, when it is not the usual closed connection.
    pub quit_reason: Option<String>,
    /// CPU, memory and disk use for the top of the sidebar.
    pub resources: Sampler,
    resources_at: Option<Instant>,
    /// The other hosts the service drives, with their stats (protocol 7);
    /// empty when it drives none.
    pub hosts: Vec<HostInfo>,
    /// The start directory's branch for the header, as the owner's shell prompt shows it.
    pub git: GitWatch,
    /// Errors noticed by the deck itself, beside the service's.
    pub local_errors: Vec<LogEntry>,
    /// The newest error shown in F2, and errors cleared there.
    pub errors_seen_at: u64,
    pub errors_cleared_at: u64,
    /// The owner's clear until the service shows it applied: the errors it
    /// cleared stay hidden and a new connection carries it again.
    pending_clear: Option<PendingClear>,
    /// The dev button's place on screen: (row, first column, end column).
    pub dev_hit: Option<(u16, u16, u16)>,
    /// Session ids in the order Super+↑ and Super+↓ gave them, and the file
    /// that keeps it for the next deck (none in tests that do not ask).
    order: Vec<u64>,
    order_file: Option<PathBuf>,
    /// Creation replies can precede the peer snapshot that makes their IDs valid.
    creation_anchors: Vec<(u64, u64)>,
    /// The order file as this deck last read or wrote it.
    order_stamp: Option<(u64, i64, i64)>,
    /// The phone gateway's files, for the pairing screen (none in tests
    /// that do not ask, so no test touches the owner's).
    pub remote_dir: Option<PathBuf>,
    /// Reconnecting after the connection closes (none in most tests).
    link: Option<Link>,
    link_stream: Option<UnixStream>,
    link_generation: Arc<AtomicU64>,
    changing_coordinator: bool,
    reconnects: Vec<Instant>,
    /// :stop was confirmed: a closed connection means the service stopped.
    stopping_service: bool,
    /// Where the programs keep their model lists and settings, for the New
    /// dialog's models (none in tests that do not ask, so no test reads the
    /// owner's).
    pub model_dirs: Option<model_list::Homes>,
    /// The New dialog's models, read as it opens.
    pub new_models: Vec<ToolModels>,
    /// The New dialog's target: automatic at protocol 14, local for older
    /// services; h chooses an explicit machine.
    pub new_host: Option<String>,
    /// The command line's completion (Tab).
    pub completion: Option<Completion>,
    /// The lines entered at `:`, for `↑` and `↓` there.
    command_history: command_history::CommandHistory,
    /// Where the keys were before the scratch terminal (:scratch) took them.
    scratch_back: Option<Sel>,
    /// The question being answered, as it streams in.
    pub asking: Option<commands::Asking>,
    /// Questions asked while another is answered, each with the route Tab
    /// chose for it, asked in turn.
    pub ask_queue: std::collections::VecDeque<(String, Option<crate::ask::Route>)>,
    /// The conversation a follow-up continues; `/clear` ends it.
    pub ask_thread: Option<crate::ask::Thread>,
    /// Questions asked and their answers, oldest first, and the folder that
    /// keeps them (none in tests that do not ask, so no test writes the
    /// owner's); read when F4 first opens.
    pub ask_history: Vec<crate::ask::Asked>,
    pub ask_dir: Option<PathBuf>,
    /// Counts changes to the history, and the answers drawn for the last.
    pub ask_generation: u64,
    pub ask_rendered: Option<commands::AskRendered>,
    /// An explicit Lua navigation request, including reselecting the current session.
    pub script_selection: Option<u64>,
    ask_loaded: bool,
    /// The scripts (`lua.rs`), and where they load from (none: Lua is off,
    /// as in tests that do not ask).
    pub lua: Option<crate::lua::Engine>,
    lua_setup: Option<(crate::lua::Places, crate::lua::JobSink)>,
    /// The scripts wait for the service's first state.
    lua_waiting: bool,
    /// What the scripts draw: sections, badges by session, the sidebar's
    /// order (`deck.opt.sidebar`), their keys and commands.
    pub lua_sections: Vec<crate::lua::Section>,
    /// Machines the scripts add to the resource table (`deck.ui.machine`).
    pub lua_machines: Vec<crate::lua::Machine>,
    pub lua_badges: HashMap<u64, crate::lua::Styled>,
    pub sidebar_order: Option<Vec<String>>,
    pub sidebar_hidden: Vec<String>,
    pub lua_keys: Vec<crate::lua::Keymap>,
    pub user_commands: Vec<crate::lua::UserCommand>,
    /// Each session as the scripts last heard of it.
    lua_seen: HashMap<u64, crate::lua::Seen>,
    /// The state the scripts were last given.
    lua_view: Option<std::rc::Rc<scripting::ViewState>>,
    lua_depth: u8,
    /// The panels last sent for the phone.
    sent_panels: Vec<crate::protocol::Panel>,
}

/// How long the deck waits for the key after Ctrl-w before giving the Ctrl-w
/// to the program (Claude and Codex delete a word with it), as Neovim's
/// `timeoutlen` does.
pub const WINDOW_KEY_WAIT: Duration = Duration::from_secs(1);

/// A file's size and modification time, to notice it changing.
fn file_stamp(path: &Path) -> Option<(u64, Option<SystemTime>)> {
    std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()))
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste, EnableFocusChange)?;
    if terminal::supports_keyboard_enhancement().unwrap_or(false) {
        execute!(stdout, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
    }
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    term.clear()?;
    Ok(term)
}

fn restore_terminal() {
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        PopKeyboardEnhancementFlags,
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = terminal::disable_raw_mode();
}

pub fn run() -> i32 {
    match run_inner() {
        Ok(Some(msg)) => {
            eprintln!("{msg}");
            1
        }
        Ok(None) => 0,
        Err(err) => {
            eprintln!("cepheus: {err:#}");
            1
        }
    }
}

fn run_inner() -> Result<Option<String>> {
    let (cfg, cfg_error) = Config::load();
    let config_path = paths::config_path();
    let config_stamp = file_stamp(&config_path);
    let coordinator_stamp = util::file_stamp(&config_path.with_file_name("coordinator.json"));
    let coordinator_target = client::coordinator::selected_from(&config_path, &cfg).map_err(|e| e.to_string());
    let stream = client::connect_or_spawn()?;
    let (tx, rx) = mpsc::channel::<AppEv>();
    let (out_tx, control) = open_link(stream, tx.clone(), 0)?;

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
    let mut term = setup_terminal()?;
    let mut app = App::new(out_tx, cfg);
    app.dot.graphics = crate::kitty::supported();
    app.link_stream = Some(control);
    {
        let tx = tx.clone();
        let generation = app.link_generation.clone();
        input::spawn(move |ev| {
            tx.send(AppEv::Linked(generation.load(Ordering::SeqCst), Box::new(AppEv::Input(ev)))).is_ok()
        })?;
    }
    app.link = Some(Link {
        tx: tx.clone(),
        connect: Arc::new(client::connect),
        start: Arc::new(|| client::connect_or_spawn().map_err(|err| io::Error::other(format!("{err:#}")))),
    });
    app.use_order_file(paths::order_file());
    app.remote_dir = Some(remote::store::remote_dir(&paths::state_dir()));
    app.model_dirs = Some(model_list::Homes::here());
    app.follow_config(config_path);
    // Notice a declaration changed while the initial connection was opening.
    app.config_stamp = config_stamp;
    app.coordinator_stamp = coordinator_stamp;
    app.coordinator_target = coordinator_target;
    app.ask_dir = Some(paths::state_dir());
    app.command_history = command_history::CommandHistory::load(paths::command_history_file());
    app.use_attention_store(paths::state_dir().join("attention-desktop"));
    if app.cfg.lua {
        let places = crate::lua::Places {
            config_dir: paths::config_dir(),
            start_dir: std::env::current_dir().unwrap_or_default(),
        };
        let jobs = tx.clone();
        let sink: crate::lua::JobSink = Arc::new(move |done| {
            let _ = jobs.send(AppEv::Linked(0, Box::new(AppEv::Lua(done))));
        });
        app.load_lua(places, sink);
    }
    if let Some(err) = cfg_error {
        app.local_error(format!("config: {err}"));
    }
    for err in Keymap::from_config(&app.cfg.keys).1 {
        app.local_error(err);
    }
    let result = app.run(&mut term, rx);
    restore_terminal();
    result?;
    if let Some(reason) = app.quit_reason.take() {
        return Ok(Some(format!("cepheus: {reason}")));
    }
    Ok(app.disconnected.then(|| "cepheus: the service connection closed".to_string()))
}

impl App {
    fn new(out: Sender<ClientMsg>, cfg: Config) -> Self {
        App {
            out,
            keys: Keymap::from_config(&cfg.keys).0,
            config_file: None,
            config_stamp: None,
            coordinator_stamp: None,
            coordinator_target: Ok(None),
            theme: Theme::load(&cfg),
            theme_checked: Instant::now(),
            cfg,
            state: DeckState::default(),
            current: None,
            attention: attention::View::default(),
            jobs: job_sessions::View::default(),
            dot: dot_chat::View::default(),
            work_queue: work_queue::View::default(),
            fleet_serial: 0,
            owner_sends: HashMap::new(),
            frame: None,
            history: session_scroll::History::default(),
            selection: None,
            wheel_scroll: scroll::ScrollGesture::default(),
            last_frames: HashMap::new(),
            mode: Mode::Normal,
            command: Default::default(),
            dialog: None,
            dialog_viewport: DialogViewport::default(),
            sidebar: true,
            notice: None,
            messages: Vec::new(),
            started: Instant::now(),
            row_hits: Vec::new(),
            pane: Rect::default(),
            sent_view: (0, 0),
            sent_attach: None,
            cursor_shape: None,
            focused: true,
            quit: false,
            disconnected: false,
            dirty: true,
            have_state: false,
            closed: HashMap::new(),
            window_pending: None,
            clipboard: crate::clipboard::Reader::system(),
            service_version: 0,
            service_pid: 0,
            restart: None,
            quit_reason: None,
            resources: Sampler::default(),
            resources_at: None,
            hosts: Vec::new(),
            git: GitWatch::default(),
            local_errors: Vec::new(),
            errors_seen_at: 0,
            errors_cleared_at: 0,
            pending_clear: None,
            dev_hit: None,
            order: Vec::new(),
            order_file: None,
            creation_anchors: Vec::new(),
            order_stamp: None,
            remote_dir: None,
            link: None,
            link_stream: None,
            link_generation: Arc::new(AtomicU64::new(0)),
            changing_coordinator: false,
            reconnects: Vec::new(),
            stopping_service: false,
            model_dirs: None,
            new_models: Vec::new(),
            new_host: None,
            completion: None,
            command_history: Default::default(),
            scratch_back: None,
            asking: None,
            ask_queue: Default::default(),
            ask_thread: None,
            ask_history: Vec::new(),
            ask_dir: None,
            ask_generation: 0,
            ask_rendered: None,
            script_selection: None,
            ask_loaded: false,
            lua: None,
            lua_setup: None,
            lua_waiting: false,
            lua_sections: Vec::new(),
            lua_machines: Vec::new(),
            lua_badges: HashMap::new(),
            sidebar_order: None,
            sidebar_hidden: Vec::new(),
            lua_keys: Vec::new(),
            user_commands: Vec::new(),
            lua_seen: HashMap::new(),
            lua_view: None,
            lua_depth: 0,
            sent_panels: Vec::new(),
        }
    }

    /// The connection closed. The service closes it when it updates itself
    /// in place, so connect again unless the service was stopped.
    fn connection_lost(&mut self) {
        let now = Instant::now();
        self.reconnects.retain(|at| now.duration_since(*at) < Duration::from_secs(60));
        let link = self.link.as_ref().filter(|_| !self.stopping_service && self.reconnects.len() < RECONNECT_LIMIT);
        let Some(link) = link else {
            self.disconnected = true;
            self.quit = true;
            return;
        };
        if let Some(pid) = self.restart.as_ref().filter(|r| r.reopen.is_some()).map(|r| r.pid) {
            self.start_again(pid);
            return;
        }
        let tx = link.tx.clone();
        let connect = link.connect.clone();
        let generation = self.link_generation.load(Ordering::SeqCst);
        let started = thread::Builder::new().name("deck-reconnect".into()).spawn(move || {
            let deadline = Instant::now() + RECONNECT_WAIT;
            let ev = loop {
                match connect() {
                    Ok(stream) => break AppEv::Reconnected(stream),
                    // No socket, or nothing listening on it: the service is gone.
                    Err(err) if matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
                        break AppEv::ReconnectFailed;
                    }
                    Err(_) if Instant::now() > deadline => break AppEv::ReconnectFailed,
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            };
            let _ = tx.send(AppEv::Linked(generation, Box::new(ev)));
        });
        if started.is_err() {
            self.disconnected = true;
            self.quit = true;
            return;
        }
        self.reconnects.push(now);
        self.set_notice("The service connection closed; reconnecting…", false);
    }

    /// The older service stopped for a restart closed the connection: once
    /// its process `pid` has exited, start the service again and connect.
    fn start_again(&mut self, pid: u32) {
        let Some(link) = &self.link else { return };
        let (tx, start) = (link.tx.clone(), link.start.clone());
        let generation = self.link_generation.load(Ordering::SeqCst);
        let started = thread::Builder::new().name("deck-restart".into()).spawn(move || {
            let deadline = Instant::now() + OLD_SERVICE_EXIT_WAIT;
            // Its socket and lock are free only once it has exited.
            while pid != 0 && Path::new(&format!("/proc/{pid}")).exists() {
                if Instant::now() > deadline {
                    let why = format!(
                        "the old service (pid {pid}) did not stop within {}s; start cepheus again once it has",
                        OLD_SERVICE_EXIT_WAIT.as_secs()
                    );
                    let _ = tx.send(AppEv::Linked(generation, Box::new(AppEv::RestartFailed(why))));
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
            let ev = match start() {
                Ok(stream) => AppEv::Reconnected(stream),
                Err(err) => AppEv::RestartFailed(format!("the service did not start again: {err}")),
            };
            let _ = tx.send(AppEv::Linked(generation, Box::new(ev)));
        });
        if started.is_err() {
            self.disconnected = true;
            self.quit = true;
            return;
        }
        self.set_notice("The service stopped; starting it again…", false);
    }

    /// Carry on over a new connection: say hello, then attach and size the
    /// pane again on the next draw.
    fn reconnected(&mut self, stream: UnixStream) {
        let opened = match &self.link {
            Some(link) => open_link(stream, link.tx.clone(), self.link_generation.load(Ordering::SeqCst)),
            None => return,
        };
        match opened {
            Ok((out, control)) => {
                self.out = out;
                self.link_stream = Some(control);
                self.sent_attach = None;
                self.sent_view = (0, 0);
                self.sent_panels.clear();
                self.history.clear();
                self.reconnect_job_reads();
                if let Some(pending) = &mut self.pending_clear {
                    pending.sent = false;
                }
                self.set_notice("Reconnected to the service.", false);
            }
            Err(_) => {
                self.disconnected = true;
                self.quit = true;
            }
        }
    }

    /// Keep the session order in `path`, starting from the order saved there.
    /// A missing or unreadable file leaves the sessions in the order they
    /// were started.
    pub fn use_order_file(&mut self, path: PathBuf) {
        self.order_stamp = util::file_stamp(&path);
        self.order = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        self.order_file = Some(path);
        self.dirty = true;
    }

    /// Follow an order someone else saved: the phone, when its owner drags a
    /// session. A file that does not read leaves the order as it was.
    pub fn watch_order(&mut self) {
        if self.service_version >= 14 {
            return;
        }
        let Some(path) = &self.order_file else { return };
        let stamp = util::file_stamp(path);
        if stamp == self.order_stamp {
            return;
        }
        self.order_stamp = stamp;
        if let Some(order) = std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<Vec<u64>>(&b).ok()) {
            if order != self.order {
                self.order = order;
                self.dirty = true;
            }
        }
    }

    /// Super+↑ and Super+↓: swap the highlighted session with the one above or below it. Only visible live
    /// sessions move.
    fn move_session(&mut self, delta: i32) {
        let Some(Sel::Session(id)) = self.current else { return };
        let mut sessions: Vec<(u64, bool)> = self.ordered_sessions().iter().map(|s| (s.id, s.alive)).collect();
        let Some(pos) = sessions.iter().position(|(sid, _)| *sid == id) else { return };
        let Some(next) = pos.checked_add_signed(delta as isize).filter(|n| *n < sessions.len()) else { return };
        let from = self.session(sessions[pos].0).unwrap();
        let to = self.session(sessions[next].0).unwrap();
        if self.is_cloud(from) != self.is_cloud(to) || sessions[next].1 != sessions[pos].1 {
            return;
        }
        sessions.swap(pos, next);
        self.order = sessions.into_iter().map(|(sid, _)| sid).collect();
        self.save_order();
    }

    fn save_order(&mut self) {
        self.dirty = true;
        let live: std::collections::HashSet<_> = self.state.sessions.iter().filter(|s| s.alive).map(|s| s.id).collect();
        self.order.retain(|id| live.contains(id));
        if self.service_version >= 14 {
            self.send(ClientMsg::SetOrder { ids: self.order.clone() });
            return;
        }
        if !matches!(self.coordinator_target, Ok(None)) {
            self.order = self.state.session_order.clone();
            self.local_error("Shared session order needs an updated coordinator (protocol 14).");
            return;
        }
        if let Some(path) = self.order_file.clone() {
            let saved =
                serde_json::to_vec(&self.order).map_err(io::Error::other).and_then(|b| util::write_atomic(&path, &b));
            match saved {
                Ok(()) => self.order_stamp = util::file_stamp(&path),
                Err(err) => self.local_error(format!("Could not save the session order to {}: {err}", path.display())),
            }
        }
    }

    fn apply_creation_anchors(&mut self) {
        let pending = std::mem::take(&mut self.creation_anchors);
        let mut changed = false;
        for (id, after) in pending {
            if !self.state.sessions.iter().any(|s| s.id == id) {
                self.creation_anchors.push((id, after));
                continue;
            }
            let mut order: Vec<_> = self.ordered_sessions().iter().map(|s| s.id).filter(|sid| *sid != id).collect();
            if let Some(pos) = order.iter().position(|sid| *sid == after) {
                order.insert(pos + 1, id);
                self.order = order;
                changed = true;
            }
        }
        if changed {
            self.save_order();
        }
    }

    /// Every error to list in F2, oldest first.
    pub fn error_list(&self) -> Vec<LogEntry> {
        let mut all: Vec<LogEntry> = self
            .state
            .errors
            .iter()
            .chain(&self.local_errors)
            .filter(|e| e.at > self.errors_cleared_at)
            .cloned()
            .collect();
        all.sort_by_key(|e| e.at);
        // A service error also arrives as the requesting window's notice.
        all.dedup_by(|a, b| a.at == b.at && a.text == b.text);
        all
    }

    /// Errors that arrived since F2 was last open.
    pub fn unseen_errors(&self) -> usize {
        self.error_list().iter().filter(|e| e.at > self.errors_seen_at).count()
    }

    /// Show an error the deck itself noticed and keep it for F2.
    fn local_error(&mut self, text: impl Into<String>) {
        self.set_notice(text, true);
    }

    /// Follow `path` for changed hotkeys from now on.
    pub fn follow_config(&mut self, path: PathBuf) {
        self.config_stamp = file_stamp(&path);
        self.coordinator_stamp = util::file_stamp(&path.with_file_name("coordinator.json"));
        self.coordinator_target = client::coordinator::selected_from(&path, &self.cfg).map_err(|e| e.to_string());
        self.config_file = Some(path);
    }

    /// Take the hotkeys and plugin settings from the config file again once it changes, so a changed key or a
    /// plugin turned on or off needs no rebuild and no restart. A file that no longer reads keeps the keys and
    /// plugins the deck has.
    pub fn check_config(&mut self) {
        let Some(path) = self.config_file.clone() else { return };
        let coordinator_stamp = util::file_stamp(&path.with_file_name("coordinator.json"));
        let stamp = file_stamp(&path);
        if stamp == self.config_stamp && coordinator_stamp == self.coordinator_stamp {
            return;
        }
        self.config_stamp = stamp;
        self.coordinator_stamp = coordinator_stamp;
        let (cfg, err) = Config::load_from(&path);
        // A bad hotkey config does not change a native connection. A remote
        // declaration still requires a valid enrolled host and fails closed.
        let target = client::coordinator::selected_from(&path, &cfg).map_err(|e| e.to_string());
        if target != self.coordinator_target {
            self.coordinator_target = target;
            self.coordinator_changed();
        }
        if let Some(err) = err {
            self.local_error(format!("config: {err}; the keys and plugins stay as they were"));
            return;
        }
        let (keys, errors) = Keymap::from_config(&cfg.keys);
        let changed = keys != self.keys;
        self.keys = keys;
        self.cfg.keys = cfg.keys;
        for err in &errors {
            self.local_error(err.clone());
        }
        if changed && errors.is_empty() {
            self.set_notice(format!("Keys updated from {}.", path.display()), false);
        }
        if (&cfg.plugins, &cfg.plugin_source_dir, &cfg.plugin_settings)
            != (&self.cfg.plugins, &self.cfg.plugin_source_dir, &self.cfg.plugin_settings)
        {
            self.cfg.plugins = cfg.plugins;
            self.cfg.plugin_source_dir = cfg.plugin_source_dir;
            self.cfg.plugin_settings = cfg.plugin_settings;
            // Scripts not loaded yet load with these settings when they do.
            if self.lua.is_some() {
                self.reload_lua();
            }
        }
    }

    /// A different coordinator has a different ID namespace. Stop the old
    /// transport and forget every pending session action before reconnecting.
    fn coordinator_changed(&mut self) {
        if self.link.is_none() {
            return;
        }
        self.link_generation.fetch_add(1, Ordering::SeqCst);
        if let Some(stream) = self.link_stream.take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let (out, _) = mpsc::channel();
        self.out = out;
        self.changing_coordinator = true;
        self.pending_clear = None;
        self.state = DeckState::default();
        self.jobs = job_sessions::View::default();
        self.work_queue = work_queue::View::default();
        self.attention_namespace_changed();
        if let Some(root) = self.attention.store_root.clone() {
            self.use_attention_store(root);
        }
        self.current = None;
        self.frame = None;
        self.history.clear();
        self.selection = None;
        self.last_frames.clear();
        self.have_state = false;
        self.mode = Mode::Normal;
        self.command.clear();
        self.dialog = None;
        self.window_pending = None;
        self.closed.clear();
        self.order.clear();
        self.creation_anchors.clear();
        self.hosts.clear();
        self.row_hits.clear();
        self.completion = None;
        self.scratch_back = None;
        self.script_selection = None;
        self.lua_seen.clear();
        self.lua_view = None;
        self.lua_badges.clear();
        self.lua = None;
        self.lua_sections.clear();
        self.lua_machines.clear();
        self.lua_keys.clear();
        self.user_commands.clear();
        if let (Some((_, sink)), Some(link)) = (self.lua_setup.as_mut(), self.link.as_ref()) {
            let tx = link.tx.clone();
            let generation = self.link_generation.load(Ordering::SeqCst);
            *sink = Arc::new(move |done| {
                let _ = tx.send(AppEv::Linked(generation, Box::new(AppEv::Lua(done))));
            });
            self.lua_waiting = true;
        }
        self.service_version = 0;
        self.service_pid = 0;
        self.restart = None;
        self.sent_attach = None;
        self.sent_view = (0, 0);
        self.sent_panels.clear();
        self.reconnects.clear();
        self.connection_lost();
    }

    /// The old-service advice: how to restart it.
    fn restart_advice(&self) -> String {
        ":restart restarts it and reopens each session.".into()
    }

    /// The restart hotkey. A service of protocol 3 or later runs its program again in its own process, as it
    /// does to update itself, and every session carries on. An older one cannot: once confirmed, it is stopped,
    /// started again from the installed build, and each live session reopened in its conversation.
    fn restart_service(&mut self) {
        if self.restart.as_ref().is_some_and(|r| r.reopen.is_some() || r.at.elapsed() < RESTART_WAIT) {
            self.set_notice("The service is restarting already.", false);
            return;
        }
        if self.service_version >= 3 {
            self.restart = Some(Restart { at: Instant::now(), pid: self.service_pid, reopen: None, welcomed: false });
            self.send(ClientMsg::Restart);
            self.set_notice("Restarting the service…", false);
            return;
        }
        let live: Vec<&SessionInfo> = self.state.sessions.iter().filter(|s| s.alive).collect();
        let working = live.iter().filter(|s| s.status == Status::Working).count();
        let unknown = live.iter().filter(|s| s.conversation.is_none()).count();
        let mut text = format!(
            "Restart the service? It is an older build that cannot keep sessions running: its {} live session(s) \
             end and reopen in their conversations.",
            live.len()
        );
        if working > 0 {
            text.push_str(&format!(" {working} working stop mid-turn."));
        }
        if unknown > 0 {
            text.push_str(&format!(" {unknown} without a known conversation stay ended."));
        }
        self.dialog = Some(Dialog::Confirm { text, action: Action::Restart });
    }

    /// Reopen the sessions of an older service restarted from here, once
    /// the new one lists them.
    fn reopen_after_restart(&mut self) {
        let Some(Restart { reopen: Some(ids), welcomed: true, .. }) = &self.restart else { return };
        let ids: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|id| self.state.sessions.iter().any(|s| s.id == *id && !s.alive && s.conversation.is_some()))
            .collect();
        for id in &ids {
            self.send(ClientMsg::Reopen { id: *id });
        }
        self.restart = None;
        self.set_notice(format!("Restarted the service and reopened {} session(s).", ids.len()), false);
    }

    /// The config as the file says now: `remote` may have been turned on
    /// since the deck started.
    fn current_config(&self) -> Config {
        match &self.config_file {
            Some(path) => Config::load_from(path).0,
            None => self.cfg.clone(),
        }
    }

    fn open_pairing(&mut self) {
        let Some(dir) = self.remote_dir.clone() else {
            self.set_notice("This deck cannot pair a phone.", true);
            return;
        };
        let pairing = Pairing::open(&dir, &self.current_config(), util::now_secs());
        self.dialog = Some(Dialog::Pair(Box::new(pairing)));
    }

    fn open_errors(&mut self) {
        self.errors_seen_at = self.error_list().iter().map(|e| e.at).max().unwrap_or(self.errors_seen_at);
        self.dialog = Some(Dialog::Errors { scroll: 0 });
    }

    /// Whether the service is new enough for `what`; says how to update it if not.
    fn service_has(&mut self, version: u32, what: &str) -> bool {
        if self.service_version >= version {
            return true;
        }
        let advice = self.restart_advice();
        self.set_notice(format!("{what} needs the new service; the one running is an older build. {advice}"), true);
        false
    }

    /// The dev button and :dev: stop the server the deck runs, or start it.
    fn toggle_dev_server(&mut self) {
        if !self.service_has(2, "The dev server button") {
            return;
        }
        let dev = &self.state.dev_server;
        if dev.pid.is_some() {
            self.send(ClientMsg::DevServer { run: false });
        } else if dev.started_at.is_some() {
            self.set_notice("The dev server is stopping…", false);
        } else {
            self.send(ClientMsg::DevServer { run: true });
            self.set_notice(format!("Starting the dev server: {}", dev.command), false);
        }
    }

    /// Read CPU, memory and disk use every `RESOURCE_INTERVAL`.
    fn sample_resources(&mut self, now: Instant) {
        if self.resources_at.is_some_and(|at| now.duration_since(at) < RESOURCE_INTERVAL) {
            return;
        }
        self.resources_at = Some(now);
        let before = self.resources.now.clone();
        let dir = if self.state.start_dir.is_empty() {
            crate::paths::home()
        } else {
            std::path::PathBuf::from(&self.state.start_dir)
        };
        self.resources.sample(&dir);
        if self.resources.now != before {
            self.dirty = true;
        }
    }

    /// Keep the header's branch status current once the service has said
    /// where the deck starts sessions.
    fn watch_git(&mut self) {
        if !self.state.start_dir.is_empty() && self.git.poll(Path::new(&self.state.start_dir)) {
            self.dirty = true;
        }
    }

    fn send(&self, mut msg: ClientMsg) {
        if let ClientMsg::Create { request, .. } = &mut msg {
            // Echo the starting row with this request, including no selection.
            // Other hosts may answer concurrent creates in a different order.
            *request = Some(match self.current {
                Some(Sel::Session(id)) => id,
                _ => 0,
            });
        }
        let _ = self.out.send(msg);
    }

    /// Show a notice in the footer and keep it: an error with the deck's own
    /// errors for F2, anything else with its messages.
    pub fn set_notice(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        let (list, keep) = if error { (&mut self.local_errors, 30) } else { (&mut self.messages, 50) };
        LogEntry::record(list, util::now_secs(), &text, keep);
        self.show_notice(text, error);
    }

    /// Show a notice in the footer only.
    fn show_notice(&mut self, text: String, error: bool) {
        self.notice = Some((text, error, Instant::now()));
        self.dirty = true;
    }

    fn run(&mut self, term: &mut Terminal<CrosstermBackend<Stdout>>, rx: Receiver<AppEv>) -> Result<()> {
        loop {
            if self.dirty {
                self.draw(term)?;
                self.dirty = false;
            }
            // Dot shows no spinner (it is always on), so it needs no frames;
            // kitty animates its pet by itself.
            // An answer being written shows its spinner and seconds.
            let asking = self.asking.is_some() && matches!(self.dialog, Some(Dialog::Ask { .. }));
            let animate = self.state.sessions.iter().any(|s| s.status == Status::Working && !self.is_cloud(s))
                || self.notice.is_some()
                || asking;
            let mut timeout = if animate { Duration::from_millis(120) } else { RESOURCE_INTERVAL };
            if matches!(self.current, Some(Sel::Job(_))) {
                timeout = timeout.min(Duration::from_secs(1));
            }
            if self.dot_native() || self.dot.sending.is_some() {
                timeout = timeout.min(Duration::from_millis(400));
            }
            if let Some((at, _)) = self.window_pending {
                timeout = timeout.min((at + WINDOW_KEY_WAIT).saturating_duration_since(Instant::now()));
            }
            if let Some(Dialog::Picker { search_due: Some(due), .. }) = &self.dialog {
                timeout = timeout.min(due.saturating_duration_since(Instant::now()));
            }
            if let Some(due) = self.lua_next_due() {
                timeout = timeout.min(due.saturating_duration_since(Instant::now()));
            }
            match rx.recv_timeout(timeout) {
                Ok(ev) => self.handle(ev),
                Err(RecvTimeoutError::Timeout) => self.dirty = true,
                Err(RecvTimeoutError::Disconnected) => break,
            }
            while let Ok(ev) = rx.try_recv() {
                self.handle(ev);
            }
            self.expire_window_key(Instant::now());
            self.lua_tick(Instant::now());
            self.poll_job(Instant::now());
            self.poll_dot(Instant::now());
            self.poll_conversation_search(Instant::now());
            self.poll_work_queue(Instant::now());
            self.poll_attention(Instant::now());
            self.sample_resources(Instant::now());
            self.watch_git();
            self.watch_order();
            if let Some((_, _, at)) = &self.notice {
                if at.elapsed() > Duration::from_secs(5) {
                    self.notice = None;
                    self.dirty = true;
                }
            }
            // Follow theme switches (Omarchy rewrites the kitty colour include).
            if self.theme_checked.elapsed() > Duration::from_secs(2) {
                self.theme_checked = Instant::now();
                let theme = Theme::load(&self.cfg);
                if theme != self.theme {
                    self.theme = theme;
                    self.dirty = true;
                }
                self.check_config();
            }
            if self.quit {
                break;
            }
        }
        Ok(())
    }

    /// Redraw inside one synchronized update (DEC mode 2026), so kitty shows each finished frame and never the
    /// cursor travelling across the screen while cells are written. Every byte goes through the terminal's own
    /// writer, so nothing reaches the screen outside the update.
    fn draw<W: Write>(&mut self, term: &mut Terminal<CrosstermBackend<W>>) -> Result<()> {
        let _ = execute!(term.backend_mut(), BeginSynchronizedUpdate);
        // Dot's pet goes to kitty once; its cells are then ordinary text.
        let graphics = self.dot_graphics();
        if !graphics.is_empty() {
            let _ = term.backend_mut().write_all(&graphics);
        }
        let mut cursor = None;
        let drawn = term
            .draw(|f| {
                cursor = view::render(f, self);
            })
            .map(drop);
        let shape = cursor.map(|(_, _, shape)| shape);
        if drawn.is_ok() && shape.is_some() && shape != self.cursor_shape {
            let style = match shape {
                Some(crate::screen::CURSOR_UNDERLINE) => SetCursorStyle::SteadyUnderScore,
                Some(crate::screen::CURSOR_BEAM) => SetCursorStyle::SteadyBar,
                _ => SetCursorStyle::SteadyBlock,
            };
            let _ = execute!(term.backend_mut(), style);
            self.cursor_shape = shape;
        }
        let _ = execute!(term.backend_mut(), EndSynchronizedUpdate);
        drawn?;
        self.sync_view();
        Ok(())
    }

    pub fn pane_rect(&self) -> Rect {
        self.pane
    }

    /// Record the pane rectangle computed during rendering.
    pub fn set_pane(&mut self, pane: Rect) {
        if self.pane != pane {
            self.selection = None;
            self.history.clear();
        }
        self.pane = pane;
        self.jobs.width = pane.width;
        self.jobs.height = pane.height;
    }

    fn sync_view(&mut self) {
        let view = (self.pane.width.max(20), self.pane.height.max(5));
        let attach = match self.current {
            Some(Sel::Session(id)) => Some(id),
            _ => None,
        };
        if self.sent_attach != Some(attach) {
            self.sent_attach = Some(attach);
            self.sent_view = view;
            self.send(ClientMsg::Attach { id: attach, cols: view.0, rows: view.1 });
        } else if self.sent_view != view {
            self.sent_view = view;
            self.send(ClientMsg::Resize { cols: view.0, rows: view.1 });
        }
    }

    fn handle(&mut self, ev: AppEv) {
        self.dirty = true;
        match ev {
            AppEv::Linked(generation, ev) => {
                if generation == self.link_generation.load(Ordering::SeqCst) {
                    self.handle(*ev);
                }
            }
            AppEv::Input(_) if self.changing_coordinator => {}
            AppEv::Server(msg) => self.handle_server(msg),
            AppEv::Input(ev) => self.handle_input(ev),
            AppEv::Disconnected => {
                self.history.clear();
                self.connection_lost();
            }
            AppEv::Reconnected(stream) => self.reconnected(stream),
            AppEv::ReconnectFailed => {
                self.disconnected = true;
                self.quit = true;
            }
            AppEv::RestartFailed(why) => {
                self.quit_reason = Some(why);
                self.disconnected = true;
                self.quit = true;
            }
            AppEv::Error(text) => self.local_error(text),
            AppEv::Lua(done) => self.lua_job_done(done),
            AppEv::Answered(asked) => self.answered(asked),
            AppEv::AskProgress { text, activity } => self.ask_progress(text, activity),
        }
    }

    fn handle_server(&mut self, msg: ServerMsg) {
        match msg {
            msg @ ServerMsg::History { .. } => self.history_reply(msg),
            ServerMsg::PluginResult { request, data, error } => {
                if self.launch_result(request, &data, error.as_deref()) {
                    return;
                }
                if !self.dot_result(request, data.clone(), error.clone()) {
                    self.lua_service_result(request, data, error);
                }
            }
            ServerMsg::FleetResult { request, data, error } => {
                if self.attention_result(request, &data, error.as_deref())
                    || self.work_queue_result(request, &data, error.as_deref())
                    || self.job_result(request, &data, error.as_deref())
                {
                    return;
                }
                if request == 0 {
                    let failed = error.is_some();
                    self.set_notice(error.unwrap_or_else(|| data.to_string()), failed);
                } else {
                    self.lua_service_result(request, data, error);
                }
            }
            ServerMsg::Welcome { version, pid, .. } => {
                if self.service_pid != pid {
                    self.owner_sends.clear();
                }
                self.service_version = version;
                self.service_pid = pid;
                self.restart.take_if(|r| r.reopen.is_none() && r.at.elapsed() > RESTART_WAIT);
                match &mut self.restart {
                    Some(r) if r.reopen.is_some() => r.welcomed = true,
                    Some(_) => {
                        self.restart = None;
                        self.set_notice("The service restarted; every session kept running.", false);
                    }
                    None => {}
                }
                if version < PROTOCOL_VERSION {
                    // The deck is new but the service still runs the build
                    // it was started with: say exactly what to do.
                    let advice = self.restart_advice();
                    self.local_error(format!(
                        "The running service is an older build (protocol {version}, this deck {PROTOCOL_VERSION}): \
                         its fixes and Antigravity, the dev button and the service's errors need it restarted. \
                         {advice}"
                    ));
                }
            }
            ServerMsg::State(state) => self.take_state(StatePatch::default(), Some(state)),
            ServerMsg::StatePatch(patch) => {
                // Only after a whole state; a deck that has none waits for one.
                if self.have_state {
                    self.take_state(patch, None);
                }
            }
            ServerMsg::Frame { id, frame } => {
                if self.current == Some(Sel::Session(id)) {
                    self.history.observe(id, &frame);
                    self.frame = Some((id, frame));
                    if self
                        .selection
                        .as_ref()
                        .is_some_and(|s| !s.pressed && self.session_frame(id).is_none_or(|frame| !s.matches(frame)))
                    {
                        self.selection = None;
                    }
                }
            }
            // A session opened from here stays in Normal mode. Place a new
            // row below the highlighted one, keeping the saved order of others.
            ServerMsg::Created { id, request } => {
                // Resuming a closed session's conversation before the service
                // dropped it reuses that session: show it again.
                self.closed.remove(&id);
                let after = match request {
                    Some(0) => None,
                    Some(after) => Some(after),
                    // Reopen/resume and older services have no caller token.
                    None => match self.current {
                        Some(Sel::Session(after)) => Some(after),
                        _ => None,
                    },
                };
                if self.service_version >= 14 {
                    if let Some(after) = after.filter(|after| *after != id) {
                        if !self.creation_anchors.iter().any(|(pending, _)| *pending == id)
                            && self.creation_anchors.len() < crate::protocol::MAX_SESSION_ORDER
                        {
                            self.creation_anchors.push((id, after));
                        }
                        self.apply_creation_anchors();
                    }
                } else if !self.order.contains(&id) {
                    if let Some(after) = after.filter(|after| *after != id) {
                        let mut order: Vec<u64> =
                            self.ordered_sessions().iter().map(|s| s.id).filter(|sid| *sid != id).collect();
                        if let Some(pos) = order.iter().position(|sid| *sid == after) {
                            order.insert(pos + 1, id);
                            self.order = order;
                            self.save_order();
                        }
                    }
                }
                self.select(Sel::Session(id));
                self.mode = Mode::Normal;
            }
            ServerMsg::Conversations { query, request, mut items } => {
                if let Some(Dialog::Picker { items: slot, loading, sel, filter, search_request, .. }) = &mut self.dialog
                {
                    if request.is_some_and(|request| Some(request) != *search_request)
                        || query.as_ref().is_some_and(|query| query != filter)
                    {
                        return;
                    }
                    if query.is_none() {
                        for item in &mut items {
                            item.search_words.clear();
                        }
                    }
                    *slot = items;
                    *loading = false;
                    *sel = 0;
                }
            }
            ServerMsg::Clipboard { text } => {
                let seq = format!("\x1b]52;c;{}\x07", util::base64(text.as_bytes()));
                let mut out = io::stdout();
                let _ = out.write_all(seq.as_bytes());
                let _ = out.flush();
            }
            // The service keeps its errors itself; F2 lists them from its state.
            ServerMsg::Notice { text, error: true } => {
                if text == crate::protocol::STOP_REFUSED_NOTICE {
                    self.stopping_service = false;
                }
                if self.service_version >= 14 && text.starts_with("Could not save session order:") {
                    self.order = self.state.session_order.clone();
                }
                self.show_notice(text, true);
            }
            // The list shows the order it saved; automatic ordering would
            // otherwise fill the footer and kept messages with this reply.
            ServerMsg::Notice { text, error: false } if text == crate::protocol::ORDER_SAVED_NOTICE => {}
            ServerMsg::Notice { text, error: false } => self.set_notice(text, false),
            ServerMsg::DirectoryChanged { cwd, error, .. } => match error {
                Some(error) => self.set_notice(format!("Could not change directory: {error}"), true),
                None => self.set_notice(
                    format!("Directory: {}. Previous chats stay in :resume.", cwd.unwrap_or_default()),
                    false,
                ),
            },
            ServerMsg::Bell { .. } => {
                let mut out = io::stdout();
                let _ = out.write_all(b"\x07");
                let _ = out.flush();
            }
            // Scripts read the hosts in `deck.state()`: without a `state` event the unmerged-branches count
            // missed a host's branches until the next state.
            ServerMsg::Hosts { hosts } => {
                self.hosts = hosts;
                if self.have_state {
                    self.lua_state_changed(false);
                }
            }
            ServerMsg::Installed { host, ok, lines } => self.installed(host, ok, lines),
            // Hook replies carry no permission authority in a desktop client,
            // and only the owner-input plugin listens for owner input.
            ServerMsg::PermissionDecision { .. }
            | ServerMsg::OwnerInput { .. }
            | ServerMsg::OwnerStop { .. }
            | ServerMsg::ScreenText { .. }
            | ServerMsg::Alert(_)
            | ServerMsg::Answered { .. }
            | ServerMsg::SentNow { .. }
            | ServerMsg::ErrorsCleared
            | ServerMsg::BroadcastDone { .. } => {}
            ServerMsg::MessageSent { id, request, error, .. } => {
                if self.owner_sends.get(&request) == Some(&id) {
                    self.owner_sends.remove(&request);
                    if let Some(error) = error {
                        self.set_notice(error, true);
                    }
                } else {
                    self.dot_sent(id, request, error);
                }
            }
        }
    }

    /// Retained selections, including jobs opened explicitly in their own pane.
    pub fn rows(&self) -> Vec<Sel> {
        self.ordered_sessions()
            .iter()
            .map(|s| Sel::Session(s.id))
            .chain(self.ordered_jobs().iter().map(|j| Sel::Job(j.id)))
            .collect()
    }

    /// How the list heads a host's sessions.
    pub fn host_label(&self, host: Option<&str>) -> String {
        match host {
            None => self.state.native_host.as_ref().map_or_else(|| self.cfg.local_host_label(), |h| h.label.clone()),
            Some(name) => {
                self.hosts.iter().find(|h| h.name == name).map_or_else(|| name.to_string(), |h| h.label.clone())
            }
        }
    }

    pub fn is_cloud(&self, session: &SessionInfo) -> bool {
        self.state.providers.iter().any(|p| p.cloud && p.id == session.tool.key() && p.host == session.host)
    }

    /// All live sessions share one saved order. Unplaced sessions follow
    /// by creation time, then identity, regardless of host poll order.
    pub fn ordered_sessions(&self) -> Vec<&SessionInfo> {
        let mut sessions: Vec<&SessionInfo> = self.state.sessions.iter().filter(|s| s.alive).collect();
        let place = |id: u64| self.order.iter().position(|o| *o == id).unwrap_or(usize::MAX);
        sessions.sort_by_key(|s| (!self.is_cloud(s), place(s.id), s.created_at, s.id));
        sessions
    }

    /// This machine, followed by online agent hosts. Build-only hosts cannot run sessions.
    pub fn start_hosts(&self) -> Vec<Option<String>> {
        let mut out = vec![None];
        out.extend(self.hosts.iter().filter(|h| h.online && h.role.is_agents()).map(|h| Some(h.name.clone())));
        out
    }

    /// Where the New dialog starts a session on host `host`.
    pub fn start_dir_on(&self, host: Option<&str>) -> String {
        match host {
            None => self.state.start_dir.clone(),
            Some(name) => self.hosts.iter().find(|h| h.name == name).map(|h| h.start_dir.clone()).unwrap_or_default(),
        }
    }

    pub fn session(&self, id: u64) -> Option<&SessionInfo> {
        self.state.sessions.iter().find(|s| s.id == id)
    }

    /// The live sessions the list numbers, in its order: all but Dot, which goes by its own name and shows no
    /// number. The numbers shown, the keys 1-9 and every command that takes a session number count these.
    pub fn numbered_sessions(&self) -> Vec<&SessionInfo> {
        self.ordered_sessions().into_iter().filter(|s| s.tool != Tool::Dynamic("dot")).collect()
    }

    /// Session `id`'s place in the list, from 1, as the list numbers it and
    /// the keys 1-9 jump to it; none for Dot.
    pub fn position(&self, id: u64) -> Option<usize> {
        self.numbered_sessions().iter().position(|s| s.id == id).map(|p| p + 1)
    }

    #[cfg(test)]
    pub fn outside(&self, pid: u32) -> Option<&OutsideInfo> {
        self.state.outside.iter().find(|o| o.pid == pid)
    }

    /// A state from the service: whole, or the parts that changed since the
    /// last one (`patch`, applied in place so the unchanged parts are not
    /// copied).
    fn take_state(&mut self, mut patch: StatePatch, whole: Option<DeckState>) {
        let clears = whole.as_ref().map(|state| &state.error_clears).or(patch.error_clears.as_ref());
        let applied = clears.is_some_and(|clears| self.pending_clear.as_ref().is_some_and(|p| p.applied_in(clears)));
        if applied || self.service_version < 14 {
            self.pending_clear = None;
        }
        // Until the service applies the owner's clear, the errors it cleared
        // stay hidden whatever else changes its counters.
        if self.have_state
            && self.pending_clear.is_none()
            && clears.is_some_and(|clears| *clears != self.state.error_clears)
        {
            self.local_errors.clear();
            self.errors_cleared_at = 0;
            self.notice = None;
        }
        let attention_selection = self.attention_selection();
        let queue_selection = self.queue_selection();
        self.changing_coordinator = false;
        // Closed sessions are hidden from each list of sessions that comes.
        if let Some(sessions) = patch.sessions.as_mut() {
            self.hide_closed(sessions);
        }
        let providers_came = whole.is_some() || patch.providers.is_some();
        let mut state = std::mem::take(&mut self.state);
        match whole {
            Some(mut whole) => {
                self.hide_closed(&mut whole.sessions);
                state = whole;
            }
            None => state.apply(patch),
        }
        if providers_came {
            crate::providers::advertise(&state.providers);
        }
        if self.service_version >= 14 {
            self.order = state.session_order.clone();
        }
        self.history.observe_state(&state);
        self.state = state;
        self.restore_queue_selection(queue_selection);
        self.queue_state_changed();
        self.restore_attention_selection(attention_selection);
        self.attention_new_state();
        let pending_selection = self.current.filter(
            |sel| matches!(sel, Sel::Session(id) if self.creation_anchors.iter().any(|(pending, _)| pending == id)),
        );
        self.apply_creation_anchors();
        let live: std::collections::HashSet<u64> = self.state.sessions.iter().map(|s| s.id).collect();
        self.last_frames.retain(|id, _| live.contains(id));
        let first = !self.have_state;
        self.have_state = true;
        self.fix_selection(first);
        if let Some(sel @ Sel::Session(id)) = pending_selection {
            if self.creation_anchors.iter().any(|(pending, _)| *pending == id) {
                self.current = Some(sel);
            }
        }
        self.reopen_after_restart();
        if self.mode == Mode::Insert && self.current_session().is_some_and(|s| !s.alive) {
            self.mode = Mode::Normal;
            self.set_notice("That session ended; the keys are back with the deck.", false);
        }
        self.send_pending_clear();
        self.lua_state_changed(first);
    }

    /// Send the owner's clear over a connection that has not carried it, once
    /// the state shows the service lacks it. Not while the service holds an
    /// error that came after the clear: its clear empties the whole list, and
    /// that error would go unseen; the deck still hides the cleared ones.
    fn send_pending_clear(&mut self) {
        let newer = self.state.errors.iter().any(|e| e.at > self.errors_cleared_at);
        match &mut self.pending_clear {
            Some(pending) if !pending.sent && !newer => pending.sent = true,
            _ => return,
        }
        self.send(ClientMsg::ClearErrors);
    }

    /// Keep sessions closed with:close off the list, and ask the service to forget each one once it has ended.
    /// The service drops a closed session itself, but one started from an older build only ends the program,
    /// and the closed session stayed listed as "exit 129".
    fn hide_closed(&mut self, sessions: &mut Vec<SessionInfo>) {
        let mut forget = Vec::new();
        self.closed.retain(|id, forgotten| match sessions.iter().find(|s| s.id == *id) {
            None => false,
            Some(s) => {
                if !s.alive && !*forgotten {
                    *forgotten = true;
                    forget.push(*id);
                }
                true
            }
        });
        for id in forget {
            self.send(ClientMsg::Forget { id });
        }
        sessions.retain(|s| !self.closed.contains_key(&s.id));
    }

    fn fix_selection(&mut self, first: bool) {
        let rows = self.rows();
        let valid = self.current.is_some_and(|c| rows.contains(&c));
        if valid {
            return;
        }
        let pick = if first {
            let sessions = self.ordered_sessions();
            sessions
                .iter()
                .find(|s| s.status == Status::NeedsYou)
                .or_else(|| sessions.iter().find(|s| s.alive))
                .map(|s| Sel::Session(s.id))
        } else {
            None
        };
        let pick = pick.or_else(|| rows.first().copied());
        self.current = pick;
        self.show_last_frame();
        self.select_job_page();
        if self.mode == Mode::Insert {
            self.mode = Mode::Normal;
        }
    }

    /// Highlight a row. Typing never follows the highlight to another
    /// session: a new selection starts in Normal mode.
    fn select(&mut self, sel: Sel) {
        if self.current != Some(sel) {
            self.current = Some(sel);
            self.show_last_frame();
            self.select_job_page();
            if self.mode == Mode::Insert {
                self.mode = Mode::Normal;
            }
        }
        self.dirty = true;
    }

    /// Keep the screen shown so far for its session, and show the current
    /// session's last screen until the service sends a new one.
    fn show_last_frame(&mut self) {
        self.history.clear();
        self.selection = None;
        self.wheel_scroll.reset();
        if let Some((id, frame)) = self.frame.take() {
            self.last_frames.insert(id, frame);
        }
        if let Some(Sel::Session(id)) = self.current {
            self.frame = self.last_frames.remove(&id).map(|frame| (id, frame));
        }
    }

    fn move_selection(&mut self, delta: i32) {
        // Keyboard navigation stays within the explicitly selected pane.
        self.move_selection_in(delta, matches!(self.current, Some(Sel::Job(_))));
    }

    fn move_session_selection(&mut self, delta: i32) {
        // Jobs have no sidebar rows, even while the Jobs pane is open.
        self.move_selection_in(delta, false);
    }

    fn move_selection_in(&mut self, delta: i32, browsing_jobs: bool) {
        let rows: Vec<_> = self.rows().into_iter().filter(|row| matches!(row, Sel::Job(_)) == browsing_jobs).collect();
        if rows.is_empty() {
            return;
        }
        let pos = self.current.and_then(|c| rows.iter().position(|r| *r == c)).unwrap_or(0) as i32;
        let next = (pos + delta).rem_euclid(rows.len() as i32) as usize;
        self.select(rows[next]);
    }

    fn next_attention(&mut self) {
        let sessions = self.ordered_sessions();
        let start = match self.current {
            Some(Sel::Session(id)) => sessions.iter().position(|s| s.id == id).map(|p| p + 1).unwrap_or(0),
            _ => 0,
        };
        let n = sessions.len();
        let find = |want: Status| {
            (0..n).map(|i| sessions[(start + i) % n]).find(|s| App::wants_attention(s, want)).map(|s| s.id)
        };
        if let Some(id) = find(Status::NeedsYou).or_else(|| find(Status::Finished)) {
            self.select(Sel::Session(id));
        } else {
            self.set_notice("Nothing needs you right now.", false);
        }
    }

    pub fn current_session(&self) -> Option<&SessionInfo> {
        match self.current {
            Some(Sel::Session(id)) => self.session(id),
            _ => None,
        }
    }

    fn current_modes(&self) -> u32 {
        match (&self.frame, self.current) {
            (Some((fid, frame)), Some(Sel::Session(id))) if *fid == id => frame.modes,
            _ => 0,
        }
    }

    fn send_input(&mut self, data: String) {
        self.scroll_session(ScrollOp::Bottom);
        if let Some(s) = self.current_session().filter(|s| s.alive) {
            self.send(ClientMsg::Input { id: s.id, data });
        }
    }

    fn handle_input(&mut self, ev: Event) {
        if !matches!(&ev, Event::Mouse(_)) {
            self.wheel_scroll.reset();
        }
        if let Some(dialog) = &self.dialog {
            self.dialog_viewport.begin(dialog);
        } else {
            self.dialog_viewport = DialogViewport::default();
        }
        if matches!(&ev, Event::Paste(_) | Event::Resize(_, _))
            || matches!(&ev, Event::Key(k) if k.kind != KeyEventKind::Release)
        {
            self.selection = None;
        }
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.handle_key(k),
            Event::Key(_) => {}
            Event::Paste(text) => {
                if matches!(self.dialog, Some(Dialog::AttentionSend)) {
                    self.paste_idle_draft(&text);
                } else if self.dialog.is_some() && self.dialog_viewport.search.is_some() {
                    let mut dialog = self.dialog.take().unwrap();
                    self.dialog_viewport.search.as_mut().unwrap().push_str(&util::one_line(&text));
                    self.apply_dialog_search(&mut dialog);
                    self.dialog = Some(dialog);
                } else if let Some(dialog @ Dialog::Picker { .. }) = &mut self.dialog {
                    if let Dialog::Picker { filter, sel, .. } = dialog {
                        filter.push_str(&util::one_line(&text));
                        *sel = 0;
                    }
                    Self::picker_filter_changed(dialog);
                } else if let Some(Dialog::Broadcast { text: message, .. }) = &mut self.dialog {
                    message.push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                } else if let Some(Dialog::Ask { draft, .. }) = &mut self.dialog {
                    draft.insert(&text);
                } else if self.dialog.is_none() && self.mode == Mode::Command {
                    self.command.insert(&util::one_line(&text));
                    self.command_history_edited();
                } else if self.dialog.is_none() && self.dot_native() {
                    self.mode = Mode::Insert;
                    self.dot_type(&text);
                } else if self.dialog.is_none() && self.live_selected() {
                    // Pasting is typing: it goes to the session, as in Insert mode.
                    self.mode = Mode::Insert;
                    let modes = self.current_modes();
                    self.send_input(keys::encode_paste(&text, modes));
                }
            }
            Event::Mouse(m) => self.handle_mouse(m),
            // A resize clears the screen; place Dot's pet again.
            Event::Resize(_, _) => {
                self.history.clear();
                self.dot_resized();
            }
            Event::FocusGained | Event::FocusLost => {
                self.focused = matches!(ev, Event::FocusGained);
                self.send(ClientMsg::Focus { focused: self.focused });
            }
        }
    }

    fn handle_key(&mut self, k: KeyEvent) {
        if self.dialog.is_some() {
            self.window_pending = None;
            self.dialog_key(k);
            return;
        }
        if let Some((_, first)) = self.window_pending.take() {
            if self.window_key(&k, &first) {
                return;
            }
        }
        if self.history_key(k) {
            return;
        }
        // A key a script mapped comes before the deck's own, as a mapping
        // in Neovim's init.lua does.
        if self.lua_key(&k) {
            if self.mode == Mode::Command {
                self.mode = Mode::Normal;
            }
            return;
        }
        // A hotkey runs in the list and, most of them, while typing. The errors key is the program's while
        // typing: "f2 blocks seeing codex warnings, f2 should only trigger when a session isnt selected,
        // otherwise it goes through to the session". A plain character bound as a hotkey stays text while
        // typing and after `:`.
        let chord = Chord::of(&k);
        let typing = self.mode == Mode::Insert;
        let hotkey = self.keys.find(&k).filter(|hotkey| {
            !(typing && (hotkey.typed_through() || chord.types_text()))
                && !(self.mode == Mode::Command && (chord.types_text() || *hotkey == Hotkey::Window))
        });
        if let Some(hotkey) = hotkey {
            if hotkey == Hotkey::Window {
                self.window_pending = Some((Instant::now(), k));
                return;
            }
            if self.mode == Mode::Command {
                self.command.clear();
                self.mode = Mode::Normal;
            }
            self.run_hotkey(hotkey);
            return;
        }
        // An Alt letter that is no hotkey is the program's while typing
        // (Alt+b and Alt+d move back and delete a word).
        if !typing && chord.mods == KeyModifiers::ALT && matches!(chord.code, KeyCode::Char(_)) {
            if self.mode == Mode::Command {
                self.command.clear();
                self.mode = Mode::Normal;
            }
            let help = self.keys.label(Hotkey::Help).unwrap_or_else(|| ":help".into());
            self.set_notice(format!("{} is not a command. {help} lists them.", chord.label()), false);
            return;
        }
        match self.mode {
            Mode::Insert => self.insert_key(k),
            Mode::Command => self.command_key(k),
            Mode::Normal => self.normal_key(k),
        }
    }

    /// Normal mode, as in Neovim: the deck has the keys. The sidebar is the explorer and the pane shows the
    /// highlighted session live. Only Enter and Ctrl-w l start typing into a session; a lone arrow or letter
    /// never does.
    fn normal_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') if !ctrl => self.move_selection(-1),
            KeyCode::Char('j') if !ctrl => self.move_selection(1),
            KeyCode::Enter => self.open_current(),
            KeyCode::Char(':') => self.open_command_line(),
            KeyCode::Char('?') => self.dialog = Some(Dialog::Help),
            KeyCode::Tab => self.next_attention(),
            KeyCode::Char(d @ '1'..='9') if !ctrl => {
                let n = d as usize - '1' as usize;
                if let Some(id) = self.numbered_sessions().get(n).map(|s| s.id) {
                    self.select(Sel::Session(id));
                }
            }
            KeyCode::PageUp => self.scroll(ScrollOp::PageUp),
            KeyCode::PageDown => self.scroll(ScrollOp::PageDown),
            KeyCode::Char('u' | 'b') if ctrl => self.scroll(ScrollOp::PageUp),
            KeyCode::Char('d' | 'f') if ctrl => self.scroll(ScrollOp::PageDown),
            KeyCode::Char('y') if ctrl => self.scroll(ScrollOp::Lines(1)),
            KeyCode::Char('e') if ctrl => self.scroll(ScrollOp::Lines(-1)),
            KeyCode::Esc if matches!(self.current, Some(Sel::Job(_))) => self.scroll(ScrollOp::Top),
            _ => {}
        }
    }

    /// Insert mode: every key goes to the selected session, Esc included so it interrupts the agent, except
    /// history keys, Ctrl-w window keys and the Alt commands.
    fn insert_key(&mut self, k: KeyEvent) {
        let Some(Sel::Session(id)) = self.current else {
            self.mode = Mode::Normal;
            return;
        };
        if self.dot_native() {
            self.dot_key(k);
            return;
        }
        if k.code == KeyCode::Char('v')
            && k.modifiers == KeyModifiers::CONTROL
            && self.current_session().is_some_and(|s| s.host.is_some() || crate::client::is_remote_coordinator())
        {
            self.paste_clipboard(id);
            return;
        }
        if let Some(bytes) = keys::encode_key(&k, self.current_modes()) {
            if k.code != KeyCode::Esc {
                self.scroll_session(ScrollOp::Bottom);
            }
            self.send(ClientMsg::Input { id, data: bytes });
        }
    }

    /// Ctrl+V in a session on another host: the program there would paste
    /// that host's clipboard, so the deck pastes this
    /// machine's text itself, or sends its image to be kept there and its
    /// path pasted.
    fn paste_clipboard(&mut self, id: u64) {
        self.scroll_session(ScrollOp::Bottom);
        match self.clipboard.read() {
            Ok(crate::clipboard::Clip::Text(text)) => {
                let data = keys::encode_paste(&text, self.current_modes());
                self.send(ClientMsg::Input { id, data });
            }
            Ok(crate::clipboard::Clip::Image { name, bytes }) => {
                if self.service_has(8, "Pasting an image into a session on another host") {
                    self.send(ClientMsg::PasteFile { id, name, data: util::base64(&bytes) });
                }
            }
            Ok(crate::clipboard::Clip::Empty) => self.set_notice("The clipboard is empty.", false),
            Err(err) => self.set_notice(format!("Could not read this machine's clipboard: {err:#}"), true),
        }
    }

    /// The key after Ctrl-w: `w`, Ctrl-w, `h` or ← goes to the list, and `l` or → to the session. `w` only ever
    /// goes to the list, so Ctrl-w l is the one chord that types into a session. Any other key was not for the
    /// deck: while typing, the program gets the Ctrl-w and then that key. Returns whether the key was used.
    fn window_key(&mut self, k: &KeyEvent, first: &KeyEvent) -> bool {
        let plain = !k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SUPER);
        match k.code {
            _ if self.keys.find(k) == Some(Hotkey::Window) => self.focus_list(),
            KeyCode::Char('w') if plain => self.focus_list(),
            KeyCode::Char('h') | KeyCode::Left if plain => self.focus_list(),
            KeyCode::Char('l') | KeyCode::Right if plain => self.focus_session(),
            _ => {
                self.type_window_key(first);
                return false;
            }
        }
        true
    }

    /// The window key was not for the deck: while typing, the program gets
    /// it (Claude and Codex delete a word with Ctrl-w).
    fn type_window_key(&mut self, first: &KeyEvent) {
        if self.mode == Mode::Insert {
            if let Some(bytes) = keys::encode_key(first, self.current_modes()) {
                self.send_input(bytes);
            }
        }
    }

    /// Give a lone Ctrl-w to the program once the wait for its second key is
    /// over.
    pub fn expire_window_key(&mut self, now: Instant) {
        let Some((at, first)) = self.window_pending else { return };
        if now.saturating_duration_since(at) < WINDOW_KEY_WAIT {
            return;
        }
        self.window_pending = None;
        self.type_window_key(&first);
        self.dirty = true;
    }

    fn focus_list(&mut self) {
        self.mode = Mode::Normal;
    }

    /// Type into the highlighted session, or say why there is nothing to
    /// type into.
    fn focus_session(&mut self) {
        match self.current {
            Some(Sel::Session(id)) => match self.session(id).map(|s| s.alive) {
                Some(true) => self.enter_insert(),
                Some(false) => {
                    let who = self.session(id).map(|s| session_label(s.tool, &s.title)).unwrap_or_default();
                    self.set_notice(format!("{who} has ended; Enter reopens it."), false)
                }
                None => {}
            },
            Some(Sel::Job(_)) => self.set_notice("Jobs are read-only. :job chat / raw / stop / dismiss", false),
            #[cfg(test)]
            Some(Sel::Outside(_)) => self.set_notice("That session runs outside the deck; Enter moves it in.", false),
            None => self.set_notice("No session yet; :new starts one.", false),
        }
    }

    /// One hotkey's command.
    fn run_hotkey(&mut self, hotkey: Hotkey) {
        let key = self.keys.label(hotkey).unwrap_or_else(|| format!(":{}", hotkey.command().unwrap_or(hotkey.name())));
        match hotkey {
            Hotkey::New => self.open_new(),
            Hotkey::Broadcast => self.open_broadcast(),
            Hotkey::Resume => self.open_picker(),
            // At once, with no question: "i expect to hit close and it closes, not pop ups". The service drops
            // a closed session once its program exits.
            Hotkey::Close => match self.current_session().map(|s| (s.id, s.alive, session_label(s.tool, &s.title))) {
                Some((id, alive, who)) => {
                    let previous = self.rows().into_iter().take_while(|row| *row != Sel::Session(id)).last();
                    self.send(if alive { ClientMsg::Close { id } } else { ClientMsg::Forget { id } });
                    self.closed.insert(id, !alive);
                    // Only a service snapshot can confirm a closed ID is
                    // gone; earlier closes are already absent locally.
                    self.state.sessions.retain(|s| s.id != id);
                    if let Some(previous) = previous {
                        self.select(previous);
                    }
                    self.fix_selection(false);
                    self.mode = Mode::Normal;
                    let done = if alive { "Closed" } else { "Removed" };
                    self.set_notice(format!("{done} {who}. :resume resumes its conversation."), false);
                }
                None => self.set_notice(format!("{key} closes the highlighted session; pick one first."), true),
            },
            #[cfg(not(test))]
            Hotkey::MoveIn => self.set_notice(format!("{key} moves in a session from OUTSIDE; pick one first."), true),
            #[cfg(test)]
            Hotkey::MoveIn => match self.current {
                Some(Sel::Outside(pid)) => self.begin_move_in(pid),
                _ => self.set_notice(format!("{key} moves in a session from OUTSIDE; pick one first."), true),
            },
            #[cfg(not(test))]
            Hotkey::KittyWindow => {
                self.set_notice(format!("{key} goes to the kitty window of a session from OUTSIDE."), true)
            }
            #[cfg(test)]
            Hotkey::KittyWindow => match self.current.and_then(|sel| match sel {
                Sel::Outside(pid) => self.outside(pid).cloned(),
                _ => None,
            }) {
                Some(o) if o.host.is_some() => {
                    let host = self.host_label(o.host.as_deref());
                    self.set_notice(
                        format!("This session runs on {host}; its window cannot be focused from this computer."),
                        false,
                    );
                }
                Some(o) => match o.kitty_pid {
                    Some(kitty) if external::focus_window(kitty) => {}
                    Some(_) => self.set_notice("Could not focus that kitty window.", true),
                    None => self.set_notice("No desktop window was found for this session.", true),
                },
                None => self.set_notice(format!("{key} goes to the kitty window of a session from OUTSIDE."), true),
            },
            Hotkey::DevServer => self.toggle_dev_server(),
            Hotkey::Sidebar => self.sidebar = !self.sidebar,
            Hotkey::Quit => self.quit = true,
            Hotkey::Restart => self.restart_service(),
            Hotkey::Stop => {
                if self.can_stop_service() {
                    self.dialog =
                        Some(Dialog::Confirm { text: "Stop the empty service?".into(), action: Action::Shutdown });
                }
            }
            Hotkey::Help => self.dialog = Some(Dialog::Help),
            Hotkey::Errors => self.open_errors(),
            Hotkey::Pair => self.open_pairing(),
            Hotkey::MoveUp => self.move_session(-1),
            Hotkey::MoveDown => self.move_session(1),
            Hotkey::Window => self.focus_list(),
            Hotkey::Branches => self.open_branches(),
            Hotkey::Ask => self.open_ask(""),
            Hotkey::MemoryRerun => self.run_command("rerun"),
            Hotkey::Scratch => self.toggle_scratch(),
            Hotkey::Command => self.open_command_line(),
        }
    }

    fn command_key(&mut self, k: KeyEvent) {
        if !matches!(k.code, KeyCode::Tab | KeyCode::BackTab) {
            self.completion = None;
        }
        self.command_history_follow();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl or Alt with an arrow moves a word, as in a shell.
        let word = k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match k.code {
            KeyCode::Tab => self.complete_command(false),
            KeyCode::BackTab => self.complete_command(true),
            KeyCode::Up => self.command_history_older(),
            KeyCode::Char('p') if ctrl => self.command_history_older(),
            KeyCode::Down => self.command_history_newer(),
            KeyCode::Char('n') if ctrl => self.command_history_newer(),
            KeyCode::Left if word => self.command.word_left(),
            KeyCode::Right if word => self.command.word_right(),
            KeyCode::Left => self.command.left(),
            KeyCode::Right => self.command.right(),
            KeyCode::Home => self.command.home(),
            KeyCode::End => self.command.end(),
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Backspace if self.command.is_empty() => self.mode = Mode::Normal,
            KeyCode::Backspace => self.command.backspace(),
            KeyCode::Delete => self.command.delete(),
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                let command = self.command.take();
                self.command_history_record(&command);
                self.run_command(command.trim());
            }
            KeyCode::Char(c) if !ctrl => self.command.insert(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        self.command_history_edited();
    }

    /// `:rename <name>`: name the highlighted session's chat, shown over the
    /// title drawn from it until a new chat begins; `:rename` alone gives
    /// that title back. The name is free text: one line, `MAX_NAME` at most.
    fn rename(&mut self, name: &str) {
        let Some(id) = self.current_session().map(|s| s.id) else {
            self.set_notice(":rename names the highlighted session; pick one first.", true);
            return;
        };
        if !self.service_has(6, "Naming a session") {
            return;
        }
        let name = crate::service::clean_name(name);
        let name = (!name.is_empty()).then_some(name);
        let said = match &name {
            Some(name) => format!("Named it \u{201c}{name}\u{201d}; :rename alone gives its own title back."),
            None => "It takes its title from its chat again.".to_string(),
        };
        self.send(ClientMsg::Rename { id, name });
        self.set_notice(said, false);
    }

    /// Enter on the highlighted row: type into a running session, reopen an
    /// ended one, move an outside one in, or start the first one.
    fn open_current(&mut self) {
        match self.current {
            Some(Sel::Session(id)) => match self.session(id).map(|s| s.alive) {
                Some(true) => self.enter_insert(),
                Some(false) => self.send(ClientMsg::Reopen { id }),
                None => {}
            },
            #[cfg(test)]
            Some(Sel::Outside(pid)) => self.begin_move_in(pid),
            Some(Sel::Job(_)) => self.refresh_job(),
            None => self.open_new(),
        }
    }

    /// The New dialog, with the models each program offers read afresh. Its
    /// defaults are what a start that picks nothing runs with: the saved
    /// launch settings (`:launch`), else each program's own.
    fn open_new(&mut self) {
        self.new_models = self.model_dirs.as_ref().map(model_list::all).unwrap_or_default();
        if let Some(homes) = self.model_dirs.as_ref().filter(|_| self.cfg.plugin_enabled(launch::PLUGIN)) {
            match launch::load(&launch::path(&homes.state)) {
                Ok(settings) => launch::overlay(&mut self.new_models, &settings),
                Err(error) => self.set_notice(format!("{error:#}"), true),
            }
        }
        self.new_host = None;
        self.dialog = Some(Dialog::New { sel: 0, model: 0, effort: 0 });
    }

    fn enter_insert(&mut self) {
        if self.scrolled() {
            self.scroll(ScrollOp::Bottom);
        }
        self.mode = Mode::Insert;
    }

    fn live_selected(&self) -> bool {
        self.current_session().is_some_and(|s| s.alive)
    }

    fn scrolled(&self) -> bool {
        self.history_active()
    }

    fn scroll(&mut self, op: ScrollOp) {
        if matches!(self.current, Some(Sel::Job(_))) {
            self.scroll_job(op);
            return;
        }
        if self.dot_native() {
            self.dot_scroll(op);
            return;
        }
        self.scroll_session(op);
    }

    /// Start `tool`, with the New dialog's model choice `model` and effort
    /// choice `effort` (0: the program's own).
    fn create(&mut self, tool: Tool, model: usize, effort: usize) {
        if !self.service_has(tool.since_version(), tool.label()) {
            return;
        }
        let (model, effort) = match model_choices(&self.new_models, tool) {
            Some(choices) => (
                model.checked_sub(1).and_then(|i| choices.models.get(i)).map(|m| m.id.clone()),
                effort.checked_sub(1).and_then(|i| effort_choices(choices, model).get(i)).cloned(),
            ),
            None => (None, None),
        };
        let chosen = model.is_some() || effort.is_some();
        if chosen && !self.service_has(5, "Choosing a model") {
            return;
        }
        if tool == Tool::Codex && !self.service_has(19, "Native Codex creation") {
            return;
        }
        let with =
            if chosen { format!(" with {}", model_words(model.as_deref(), effort.as_deref())) } else { String::new() };
        let host = self.new_host.clone();
        let location = match host.as_deref() {
            Some(name) => format!(" on {} in {}", self.host_label(Some(name)), self.start_dir_on(Some(name))),
            None => format!(" in {}", self.start_dir_on(None)),
        };
        self.send(ClientMsg::Create {
            tool,
            model,
            effort,
            host,
            cwd: None,
            command: Vec::new(),
            name: None,
            request: None,
        });
        self.set_notice(format!("Starting {}{with}{location}…", tool.label()), false);
    }

    /// Compose a shared instruction for sessions currently working. The
    /// service rechecks eligibility before delivering the message.
    fn open_broadcast(&mut self) {
        if !self.service_has(15, "Confirmed broadcasts across all hosts") {
            return;
        }
        // A terminal is no agent to message.
        let targets: Vec<(u64, bool)> = self
            .ordered_sessions()
            .iter()
            .filter(|s| s.takes_broadcast())
            .map(|s| (s.id, s.takes_messages()))
            .collect();
        if targets.is_empty() {
            self.set_notice("No working agents to broadcast to.", true);
            return;
        }
        self.dialog = Some(Dialog::Broadcast { text: String::new(), targets, sel: 0 });
    }

    fn poll_conversation_search(&mut self, now: Instant) {
        let Some(Dialog::Picker { filter, search_due, search_request, .. }) = &mut self.dialog else { return };
        if search_due.is_some_and(|due| now >= due) {
            *search_due = None;
            let query = filter.clone();
            static NEXT_SEARCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let request = Some(NEXT_SEARCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            *search_request = request;
            self.send(ClientMsg::ListConversations { query, request });
        }
    }

    /// A changed picker filter drops the last search's prompt matches and
    /// asks the service again once typing pauses.
    fn picker_filter_changed(dialog: &mut Dialog) {
        let Dialog::Picker { items, filter, loading, search_due, .. } = dialog else { return };
        for item in items.iter_mut() {
            item.search_words.clear();
        }
        let valid = filter.trim().is_empty() || crate::remote::search::words(filter).is_ok();
        *loading = valid;
        *search_due = valid.then(|| Instant::now() + Duration::from_millis(180));
    }

    fn open_picker(&mut self) {
        self.dialog = Some(Dialog::Picker {
            items: Vec::new(),
            sel: 0,
            filter: String::new(),
            loading: true,
            search_due: None,
            search_request: None,
        });
        self.send(ClientMsg::ListConversations { request: None, query: String::new() });
    }

    #[cfg(test)]
    fn begin_move_in(&mut self, pid: u32) {
        let Some(o) = self.outside(pid) else { return };
        // The service resolves the process identity on its own host. Never
        // ask the owner to guess which saved conversation belongs to it.
        self.send(ClientMsg::MoveIn { tool: o.tool, pid, conversation: o.conversation.clone() });
    }

    /// The conversations every word of `filter` finds: in the title, as
    /// the start of the id, or as the program ("codex", "agy").
    pub fn picker_matches(items: &[Conversation], filter: &str) -> Vec<usize> {
        let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
        let program = |tool: Tool, word: &str| {
            let name = tool.label().to_lowercase();
            name.starts_with(word) || (tool == Tool::Antigravity && "agy".starts_with(word))
        };
        items
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                // opencode ids mix cases (`ses_ead42ac29ffei5Xcr4…`).
                let (title, id) = (c.title.to_lowercase(), c.id.to_lowercase());
                words.iter().all(|w| {
                    title.contains(w.as_str())
                        || c.search_words.contains(w)
                        || id.starts_with(w.as_str())
                        || program(c.tool, w)
                        || c.host.as_ref().is_some_and(|host| host.to_lowercase().contains(w.as_str()))
                })
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn dialog_key(&mut self, k: KeyEvent) {
        let Some(mut dialog) = self.dialog.take() else { return };
        self.dialog_viewport.begin(&dialog);
        if matches!(dialog, Dialog::AttentionSend) {
            self.dialog = self.idle_draft_key(k);
            return;
        }
        if self.search_dialog_key(&mut dialog, k) {
            self.dialog = Some(dialog);
            return;
        }
        if self.navigate_dialog(&mut dialog, k) {
            self.dialog = Some(dialog);
            return;
        }
        match dialog {
            Dialog::Attention { tab, sel } => self.dialog = self.attention_key(tab, sel, k),
            Dialog::AttentionSend => unreachable!(),
            Dialog::WorkQueue { tab, sel } => self.dialog = self.work_queue_key(tab, sel, k),
            Dialog::Help => match k.code {
                KeyCode::Esc | KeyCode::Char('q' | '?') | KeyCode::Enter => {}
                _ if self.keys.find(&k) == Some(Hotkey::Help) => {}
                _ => self.dialog = Some(Dialog::Help),
            },
            dialog @ (Dialog::Machines | Dialog::Usage) => {
                if !matches!(k.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
                    self.dialog = Some(dialog);
                }
            }
            dialog @ (Dialog::Branches { .. }
            | Dialog::Ask { .. }
            | Dialog::Scans { .. }
            | Dialog::Report { .. }
            | Dialog::Choose { .. }) => {
                let next = self.command_dialog_key(dialog, k);
                // A key in the dialog may have opened another (a script's
                // picker, the branches again with a hint): keep that one.
                if self.dialog.is_none() {
                    self.dialog = next;
                }
            }
            Dialog::Pair(mut pairing) => {
                match k.code {
                    _ if self.keys.find(&k) == Some(Hotkey::Pair) => return pairing.close(),
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return pairing.close(),
                    KeyCode::Up | KeyCode::Char('k') => pairing.select(-1),
                    KeyCode::Down | KeyCode::Char('j') => pairing.select(1),
                    KeyCode::Char('n') => pairing.renew(&self.current_config(), util::now_secs()),
                    KeyCode::Char('d') => match pairing.remove_selected() {
                        Ok(Some(d)) => self.set_notice(format!("Removed {}; it is disconnected.", d.name), false),
                        Ok(None) => {}
                        Err(err) => self.set_notice(format!("Could not remove the phone: {err}"), true),
                    },
                    _ => {}
                }
                self.dialog = Some(Dialog::Pair(pairing));
            }
            Dialog::Errors { mut scroll } => {
                let count = self.error_list().len();
                match k.code {
                    _ if self.keys.find(&k) == Some(Hotkey::Errors) => return,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return,
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => scroll = (scroll + 1).min(count.saturating_sub(1)),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    KeyCode::PageDown => scroll = (scroll + 10).min(count.saturating_sub(1)),
                    KeyCode::Char('c') => {
                        // Clear the list here and, from protocol 4, in the
                        // service, so a deck opened later does not list them
                        // again; an older service keeps its copy.
                        self.clear_errors();
                        scroll = 0;
                    }
                    _ => {}
                }
                self.dialog = Some(Dialog::Errors { scroll });
            }
            Dialog::New { mut sel, mut model, mut effort } => {
                let tools = new_tools(&self.cfg);
                if tools.is_empty() {
                    if k.code != KeyCode::Esc {
                        self.dialog = Some(Dialog::New { sel: 0, model: 0, effort: 0 });
                    }
                    return;
                }
                let n = tools.len();
                let keyed = |c: char| {
                    tools
                        .iter()
                        .enumerate()
                        .find(|(i, tool)| new_key(**tool) == c || (*i < 9 && c == char::from(b'1' + *i as u8)))
                };
                let choices = model_choices(&self.new_models, tools[sel.min(n - 1)]);
                // `m` and `e` step through the highlighted program's models
                // and efforts, Shift steps back; 0 is its own default.
                let step = |at: usize, count: usize, back: bool| match count {
                    0 => 0,
                    _ if back => (at + count) % (count + 1),
                    _ => (at + 1) % (count + 1),
                };
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                        (sel, model, effort) = ((sel + n - 1) % n, 0, 0);
                    }
                    KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => (sel, model, effort) = ((sel + 1) % n, 0, 0),
                    KeyCode::Char(c @ ('m' | 'M')) => {
                        model = step(model, choices.map_or(0, |m| m.models.len()), c == 'M');
                        let efforts = choices.map_or(0, |m| effort_choices(m, model).len());
                        effort = effort.min(efforts);
                    }
                    KeyCode::Char(c @ ('e' | 'E')) => {
                        effort = step(effort, choices.map_or(0, |m| effort_choices(m, model).len()), c == 'E');
                    }
                    // → and `h` step along the hosts it can start on, ← and Shift back.
                    KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'H') => {
                        let back = matches!(k.code, KeyCode::Left | KeyCode::Char('H'));
                        let hosts = self.start_hosts();
                        let at = hosts.iter().position(|h| *h == self.new_host).unwrap_or(0);
                        let next = if back { (at + hosts.len() - 1) % hosts.len() } else { (at + 1) % hosts.len() };
                        self.new_host = hosts[next].clone();
                    }
                    // A program's own key starts it with what is chosen for
                    // it when it is the one highlighted.
                    KeyCode::Char(c) if keyed(c).is_some() => {
                        if let Some((i, tool)) = keyed(c) {
                            let (model, effort) = if i == sel { (model, effort) } else { (0, 0) };
                            self.create(*tool, model, effort);
                        }
                        return;
                    }
                    KeyCode::Enter => return self.create(tools[sel.min(n - 1)], model, effort),
                    _ => {}
                }
                self.dialog = Some(Dialog::New { sel, model, effort });
            }
            // Enter does not stop or restart the service: it ends or restarts
            // every session, so only an explicit y confirms it.
            Dialog::Confirm { text, action } => match k.code {
                KeyCode::Enter => self.dialog = Some(Dialog::Confirm { text, action }),
                KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm(action),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.dialog = Some(Dialog::Confirm { text, action }),
            },
            Dialog::Picker { items, mut sel, filter, loading, search_due, search_request } => {
                let matches = Self::picker_matches(&items, &filter);
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Up => sel = sel.saturating_sub(1),
                    KeyCode::Down => sel = (sel + 1).min(matches.len().saturating_sub(1)),
                    KeyCode::PageUp => sel = sel.saturating_sub(10),
                    KeyCode::PageDown => sel = (sel + 10).min(matches.len().saturating_sub(1)),
                    KeyCode::Enter => {
                        if let Some(item) = matches.get(sel).and_then(|&i| items.get(i)) {
                            self.choose_conversation(item.clone());
                        }
                        return;
                    }
                    _ => {}
                }
                self.dialog = Some(Dialog::Picker { items, sel, filter, loading, search_due, search_request });
            }
            Dialog::Broadcast { mut text, mut targets, mut sel } => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter if k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                        text.push('\n')
                    }
                    KeyCode::Enter => {
                        let ids: Vec<u64> = targets.iter().filter(|(_, on)| *on).map(|(id, _)| *id).collect();
                        if text.trim().is_empty() {
                            self.set_notice("Type a message first.", true);
                        } else if ids.is_empty() {
                            self.set_notice("Tick at least one session (Tab).", true);
                        } else {
                            if !self
                                .service_has(crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL, "Native owner broadcasts")
                            {
                                return;
                            }
                            let all = targets.iter().all(|(_, on)| *on);
                            self.send(ClientMsg::Broadcast {
                                running_only: true,
                                ids: if all { vec![] } else { ids },
                                text,
                            });
                            self.show_notice(
                                if all {
                                    "Sending to working agents on all hosts…"
                                } else {
                                    "Sending to selected working agents…"
                                }
                                .into(),
                                false,
                            );
                            return;
                        }
                    }
                    KeyCode::Up => sel = sel.saturating_sub(1),
                    KeyCode::Down => sel = (sel + 1).min(targets.len().saturating_sub(1)),
                    KeyCode::Tab => {
                        if let Some(t) = targets.get_mut(sel) {
                            t.1 = !t.1;
                        }
                    }
                    KeyCode::Char('a') if ctrl => {
                        let all = targets.iter().all(|(_, on)| *on);
                        for t in &mut targets {
                            t.1 = !all;
                        }
                    }
                    KeyCode::Char('u') if ctrl => text.clear(),
                    KeyCode::Char('j') if ctrl => text.push('\n'),
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) if !ctrl => text.push(c),
                    _ => {}
                }
                self.dialog = Some(Dialog::Broadcast { text, targets, sel });
            }
        }
    }

    fn choose_conversation(&mut self, item: Conversation) {
        match item.open_in {
            Some(id) => self.select(Sel::Session(id)),
            None => {
                self.send(ClientMsg::Resume { tool: item.tool, conversation: item.id, host: item.host });
                self.set_notice(format!("Resuming \"{}\"…", util::truncate_width(&item.title, 40)), false);
            }
        }
    }

    fn can_stop_service(&mut self) -> bool {
        if self.state.sessions.iter().any(|session| session.alive) {
            self.dialog = None;
            self.stopping_service = false;
            self.show_notice(crate::protocol::STOP_REFUSED_NOTICE.into(), true);
            return false;
        }
        true
    }

    fn confirm(&mut self, action: Action) {
        match action {
            Action::Shutdown => {
                if !self.can_stop_service() {
                    return;
                }
                self.stopping_service = true;
                self.send(ClientMsg::Shutdown);
                self.set_notice("Stopping the service…", false);
            }
            Action::Restart => {
                let reopen = self.state.sessions.iter().filter(|s| s.alive && s.conversation.is_some()).map(|s| s.id);
                self.restart = Some(Restart {
                    at: Instant::now(),
                    pid: self.service_pid,
                    reopen: Some(reopen.collect()),
                    welcomed: false,
                });
                self.send(ClientMsg::Shutdown);
                self.set_notice("Stopping the service to restart it…", false);
            }
        }
    }

    fn handle_mouse(&mut self, m: MouseEvent) {
        if !matches!(m.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::Moved) {
            self.wheel_scroll.reset();
        }
        // A left press is held until release so a drag selects locally and
        // cannot move the hosted draft's cursor. A click still reaches it.
        if self.dialog.is_none()
            && self.selection.as_ref().is_some_and(|s| s.pressed)
            && matches!(m.kind, MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left))
        {
            let selection = self.selection.as_mut().unwrap();
            selection.update(self.pane, m);
            if m.kind == MouseEventKind::Up(MouseButton::Left) {
                selection.pressed = false;
                if selection.dragged {
                    let text = selection.text();
                    if !text.is_empty() {
                        self.handle_server(ServerMsg::Clipboard { text });
                        self.set_notice("Selection copied.", false);
                    }
                    if self.selection.as_ref().is_some_and(|s| {
                        self.current_session()
                            .and_then(|session| self.session_frame(session.id))
                            .is_none_or(|f| !s.matches(f))
                    }) {
                        self.selection = None;
                    }
                } else {
                    let press = self.selection.take().unwrap().press;
                    self.forward_mouse(press, 1);
                    self.forward_mouse(m, 1);
                }
            }
            self.dirty = true;
            return;
        }
        if !matches!(m.kind, MouseEventKind::Moved) {
            self.selection = None;
        }
        if self.dialog.is_some() {
            match m.kind {
                MouseEventKind::ScrollUp => self.wheel_dialog(true, m.modifiers),
                MouseEventKind::ScrollDown => self.wheel_dialog(false, m.modifiers),
                _ => self.wheel_scroll.reset(),
            }
            return;
        }
        if let (MouseEventKind::Down(MouseButton::Left), Some((y, x0, x1))) = (m.kind, self.dev_hit) {
            if m.row == y && (x0..x1).contains(&m.column) {
                self.toggle_dev_server();
                return;
            }
        }
        let pane = self.pane;
        let in_pane =
            m.column >= pane.x && m.column < pane.x + pane.width && m.row >= pane.y && m.row < pane.y + pane.height;
        if !in_pane {
            self.wheel_scroll.reset();
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let hit = self.row_hits.iter().rev().find(|(y, ..)| *y <= m.row).map(|(y, h, sel)| (*y, *h, *sel));
                    if let Some((y, height, sel)) = hit {
                        if m.row < y + height {
                            self.select(sel);
                            if self.mode == Mode::Insert {
                                self.mode = Mode::Normal;
                            }
                        }
                    }
                }
                MouseEventKind::ScrollUp => self.move_session_selection(-1),
                MouseEventKind::ScrollDown => self.move_session_selection(1),
                _ => {}
            }
            return;
        }
        if matches!(self.current, Some(Sel::Job(_))) {
            match m.kind {
                MouseEventKind::ScrollUp => self.scroll_job(ScrollOp::Lines(3)),
                MouseEventKind::ScrollDown => self.scroll_job(ScrollOp::Lines(-3)),
                _ => {}
            }
            return;
        }
        if self.dot_native() {
            match m.kind {
                MouseEventKind::ScrollUp => self.dot_scroll(ScrollOp::Lines(3)),
                MouseEventKind::ScrollDown => self.dot_scroll(ScrollOp::Lines(-3)),
                MouseEventKind::Down(_) => {
                    self.mode = Mode::Insert;
                    self.dirty = true;
                }
                _ => {}
            }
            return;
        }
        let Some(s) = self.current_session() else { return };
        if !s.alive && !s.has_screen {
            return;
        }
        let id = s.id;
        let alive = s.alive;
        // Clicking into a running session's screen starts typing there.
        if alive && matches!(m.kind, MouseEventKind::Down(_)) && self.mode != Mode::Insert {
            self.mode = Mode::Insert;
        }
        if m.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(frame) = self.session_frame(id) {
                self.selection = Some(TextSelection::new(frame.clone(), pane, m));
                return;
            }
        }
        let modes = self.current_modes();
        let up = match m.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => {
                if alive && modes & M_MOUSE_ANY != 0 {
                    self.forward_mouse(m, 1);
                }
                return;
            }
        };
        let multiplier = if m.modifiers.is_empty() {
            self.wheel_scroll.multiplier(id, up, Instant::now())
        } else {
            self.wheel_scroll.reset();
            1
        };
        // A program on the ordinary screen leaves its scrollback to the
        // terminal: the wheel reads the view's own copy, as a web page scrolls.
        if self.history_active() || modes & (M_MOUSE_ANY | M_ALT_SCREEN) == 0 {
            let rows = (scroll::WHEEL_ROWS * multiplier) as i32;
            self.scroll_session(ScrollOp::Lines(if up { rows } else { -rows }));
        } else if alive && modes & M_MOUSE_ANY != 0 {
            self.forward_mouse(m, multiplier);
        } else if alive && modes & M_ALT_SCREEN != 0 && modes & M_ALTERNATE_SCROLL != 0 {
            self.send(ClientMsg::Input { id, data: keys::wheel_as_arrows(up, 3 * multiplier, modes) });
        }
    }

    fn forward_mouse(&self, m: MouseEvent, repeats: usize) {
        let Some(s) = self.current_session().filter(|s| s.alive) else { return };
        let col = m.column.saturating_sub(self.pane.x).min(self.pane.width.saturating_sub(1));
        let row = m.row.saturating_sub(self.pane.y).min(self.pane.height.saturating_sub(1));
        if let Some(data) = keys::encode_mouse(m.kind, col, row, m.modifiers, self.current_modes()) {
            // Keep a swipe's repeated reports in one write, including over SSH.
            self.send(ClientMsg::Input { id: s.id, data: data.repeat(repeats) });
        }
    }
}

#[cfg(test)]
#[path = "tests/tui.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "tests/tui_stop.rs"]
mod stop_tests;

#[cfg(test)]
#[path = "tests/tui_clear_errors.rs"]
mod clear_error_tests;

#[cfg(test)]
#[path = "tests/menu_scroll.rs"]
mod menu_scroll_tests;

#[cfg(test)]
#[path = "tests/scroll.rs"]
mod scroll_tests;

#[cfg(test)]
#[path = "tests/directory_commands.rs"]
mod directory_command_tests;

#[cfg(test)]
#[path = "tests/agent_count.rs"]
mod agent_count_tests;

#[cfg(test)]
#[path = "tests/tui_order.rs"]
mod order_tests;

#[cfg(test)]
#[path = "tests/machines.rs"]
mod machines_tests;

#[cfg(test)]
#[path = "tests/session_scroll.rs"]
mod session_scroll_tests;
