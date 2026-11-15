//! The deck's Lua API, as Neovim has one. The deck reads `init.lua` from its config folder, then every
//! `plugins/*.lua`, then its bundled plugins; `require` finds modules in `lua/`. Scripts reach the deck through
//! the global `deck` table:
//!
//! - `deck.keymap(key, command_line | function, {desc})`, `deck.command(name,
//!   function(args), {desc, complete})`, `deck.cmd(line)`;
//! - `deck.opt` (options), `deck.state()` (read only), `deck.on(event, fn)`;
//! - `deck.ui.section(id, {title, lines, phone, pinned})`, `deck.ui.badge(fn)`,
//!   `deck.ui.pick({title, items, on_choose, on_close, filter, keys, …})`;
//! - `deck.new(tool, {host, cwd, name})`, `deck.term({host, cwd, name, cmd})`,
//!   `deck.send(id, text)`, `deck.select(id)`, `deck.notify(text, {error})`;
//! - `deck.run(argv, {cwd, timeout})`, `deck.job(argv, {cwd, timeout}, fn)`,
//!   `deck.every(seconds, fn)`, `deck.after(seconds, fn)`, `deck.json`;
//! - `deck.fs.list(dir)`, `deck.fs.stat(path)`, `deck.fs.read(path, max)`,
//!   `deck.fs.realpath(path)`, `deck.fs.watch(folders, fn, {settle})` and
//!   `deck.sha256(text)`, so a plugin follows files without polling them.
//!
//! Guardrails: every call into Lua has a limit on the processor time it uses (`CALL_LIMIT`, longer for
//! loading), the state has a memory limit, and a script that fails leaves the built-in deck working: its error
//! goes to F2 and `:reload` loads the scripts again. Programs run from a list of words, never a shell line,
//! with a time limit and capped output (`run.rs`).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, RegistryKey, Table, Value, VmState};
use serde::Serialize;

use crate::hotkeys::Chord;
use crate::protocol::{Panel, SessionInfo, Status, MAX_PANELS, MAX_PANEL_LINES};

#[path = "lua_files.rs"]
mod files;
#[path = "plugin_sources.rs"]
mod sources;

/// How much processor time one call into Lua may use, and loading the
/// scripts. It is the calling thread's own time (`thread_cpu`), so a machine
/// busy with other programs never stops an ordinary callback, while a
/// runaway loop is still stopped.
pub const CALL_LIMIT: Duration = Duration::from_millis(250);
pub const LOAD_LIMIT: Duration = Duration::from_secs(2);
/// The most memory the scripts may hold.
pub const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// `deck.run` blocks the screen, so it gets little time; `deck.job` more.
pub const RUN_LIMIT: Duration = Duration::from_secs(2);
pub const JOB_LIMIT: Duration = Duration::from_secs(300);
pub const JOB_DEFAULT: Duration = Duration::from_secs(30);
/// At most this many jobs at once, timers, and keymaps or commands.
pub const MAX_JOBS: usize = 8;
pub const MAX_TIMERS: usize = 64;
pub const MAX_BINDINGS: usize = 256;
/// The shortest repeat for `deck.every`.
pub const MIN_EVERY: Duration = Duration::from_secs(1);
/// The longest line a section shows.
pub const MAX_LINE: usize = 400;

/// The processor time the calling thread has used. Lua runs on the thread
/// that calls into it, so this charges a script for its own work only.
pub(crate) fn thread_cpu() -> Duration {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime only writes the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

include!(concat!(env!("OUT_DIR"), "/bundled_plugins.rs"));
/// The example config the README and `cepheus init-lua` show.
pub const EXAMPLE_INIT: &str = include_str!("../lua/init.example.lua");

/// The theme styles a script may name for its text; anything else is
/// plain. Scripts never pick raw colours: the deck follows the terminal's
/// theme.
pub const STYLES: &[&str] = &[
    "accent",
    "dim",
    "faint",
    "bold",
    "ok",
    "warn",
    "bad",
    "working",
    "needs",
    "finished",
    "task",
    "review",
    "claude",
    "codex",
    "antigravity",
    "grok",
    "opencode",
    // Bold in the accent colour, as the repository name on the title row.
    "strong",
];

/// What the standard library loses inside the deck. The deck owns the
/// terminal: `print` and `io.write` would draw over its screen, `io.read`
/// would wait on its keys, and `os.exit` would leave the terminal raw. A
/// shell line (`os.execute`, `io.popen`) blocks where the time limit cannot
/// stop it, so programs run through `deck.run` and `deck.job`. `print`
/// shows its text on the notice line instead.
const GUARDS: &str = r#"
local notify = deck.notify
print = function(...)
  local parts = table.pack(...)
  for i = 1, parts.n do parts[i] = tostring(parts[i]) end
  notify(table.concat(parts, " ", 1, parts.n))
end
local function gone(name, instead)
  return function() error(name .. " is not available in the deck: " .. instead, 2) end
end
os.exit = gone("os.exit", "the deck owns the terminal (:q quits)")
os.execute = gone("os.execute", "use deck.run or deck.job with a list of words")
io.popen = gone("io.popen", "use deck.run or deck.job with a list of words")
io.read = gone("io.read", "the deck owns the keyboard")
io.write = gone("io.write", "use deck.notify or print")
io.input, io.output, io.stdin, io.stdout, io.stderr = nil, nil, nil, nil, nil
local lines = io.lines
io.lines = function(file, ...)
  if file == nil then error("io.lines needs a file name: the deck owns the keyboard", 2) end
  return lines(file, ...)
end
"#;

/// A piece of text in a theme style ("" is plain).
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub text: String,
    pub style: String,
}

pub type Styled = Vec<Segment>;

pub fn styled_text(parts: &[Segment]) -> String {
    parts.iter().map(|part| part.text.as_str()).collect()
}

/// What a picker row is: one to choose, text to read, or a heading that
/// starts a group of rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ItemKind {
    #[default]
    Choice,
    Text,
    Heading,
}

/// One row of a script's picker.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PickItem {
    pub text: Styled,
    /// Drawn against the row's right edge.
    pub right: Styled,
    pub kind: ItemKind,
    /// A table row's cells, lined up under its group's columns.
    pub cells: Vec<Styled>,
    /// A heading's columns for the rows under it.
    pub columns: Vec<PickColumn>,
}

impl PickItem {
    pub fn choice(text: Styled) -> PickItem {
        PickItem { text, ..PickItem::default() }
    }

    /// Its text as the script's callback and the phone get it: a table
    /// row's cells two spaces apart.
    pub fn plain(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let text = styled_text(&self.text);
        if !text.trim().is_empty() || self.cells.is_empty() {
            parts.push(text);
        }
        parts.extend(self.cells.iter().map(|cell| styled_text(cell).trim().to_string()).filter(|c| !c.is_empty()));
        let right = styled_text(&self.right);
        if !right.trim().is_empty() {
            parts.push(right.trim().to_string());
        }
        parts.join("  ")
    }
}

/// A table column of a picker: its title and whether its cells line up on
/// the right, as figures do.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PickColumn {
    pub title: String,
    pub right: bool,
}

/// A figure over its label at the top of a picker.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PickStat {
    pub value: Styled,
    pub label: String,
}

/// One of a picker's pages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PickTab {
    pub label: String,
    pub count: String,
}

/// A key a picker answers besides Enter, and its hint.
#[derive(Clone, Debug, PartialEq)]
pub struct PickKey {
    pub key: String,
    pub label: String,
}

/// The keys a picker may name besides single characters.
pub const PICK_KEY_NAMES: &[&str] = &["backspace", "left", "right", "tab", "delete"];

/// A script's picker (`deck.ui.pick`).
#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    pub title: String,
    /// Status drawn at the top right of the window.
    pub subtitle: String,
    pub items: Vec<PickItem>,
    /// Legacy panel option, retained for script compatibility. Search now
    /// always starts with `/`, leaving ordinary letters free for `keys`.
    pub filter: bool,
    /// The item first selected.
    pub selected: usize,
    pub keys: Vec<PickKey>,
    /// Columns for table rows outside a heading that names its own.
    pub columns: Vec<PickColumn>,
    /// Figures shown over the rows.
    pub stats: Vec<PickStat>,
    /// The picker's pages and the open one; ← → and Tab move between them.
    pub tabs: Vec<PickTab>,
    pub tab: usize,
}

impl Pick {
    pub fn new(title: impl Into<String>, items: Vec<PickItem>) -> Pick {
        Pick {
            title: title.into(),
            subtitle: String::new(),
            items,
            filter: true,
            selected: 0,
            keys: Vec::new(),
            columns: Vec::new(),
            stats: Vec::new(),
            tabs: Vec::new(),
            tab: 0,
        }
    }
}

/// A section a script draws in the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub id: String,
    pub title: String,
    pub lines: Vec<Styled>,
    /// Shown on the phone's stats screen too.
    pub phone: bool,
    /// Shown in the desktop sidebar; phone panels remain independent.
    pub sidebar: bool,
    /// Kept at the top with the header instead of scrolling with the list.
    pub pinned: bool,
    /// Its first line on the repository title row instead of its own rows.
    pub header: Option<HeaderPlace>,
}

/// A machine a script adds to the deck's CPU/RAM/SSD table (`deck.ui.machine`), beside the hosts the deck
/// drives itself: a server it reads some other way, say.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Machine {
    pub id: String,
    pub label: String,
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    pub disk_used: Option<u64>,
    pub disk_available: Option<u64>,
    /// Why there are no readings, drawn in their place ("unreachable").
    pub status: Option<String>,
    /// A row in the sidebar's table too; `:machines` lists every machine.
    pub sidebar: bool,
}

/// At most this many script machines, and this long a label or status.
const MAX_MACHINES: usize = 32;
const MAX_MACHINE_LABEL: usize = 40;
const MAX_MACHINE_STATUS: usize = 80;

/// Where a header section's first line sits on the repository title row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderPlace {
    /// Right-aligned against the divider (`header = true`).
    Right,
    /// Right after the title text, one space apart (`header = "inline"`).
    Inline,
}

impl Section {
    pub fn panel(&self) -> Panel {
        let lines = self.lines.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>()).collect();
        Panel { id: self.id.clone(), title: self.title.clone(), lines }
    }
}

/// What a script asked the deck to do; the deck does it once the call
/// into Lua has returned.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Command(String),
    Reorder(Vec<u64>),
    Fleet { action: String, args: serde_json::Value, callback: u64 },
    Notify { text: String, error: bool },
    Pick { pick: Box<Pick>, callback: u64 },
    Create { tool: String, host: Option<String>, cwd: Option<String>, name: Option<String>, command: Vec<String> },
    Send { id: u64, text: String },
    Select { id: u64 },
    Resume { tool: String, conversation: String, host: Option<String> },
    ServiceCall { plugin: String, command: String, args: serde_json::Value, callback: Option<u64> },
    Annotate { plugin: String, id: u64, data: serde_json::Value },
    Title { plugin: String, id: u64, text: Option<String> },
    Process { plugin: String, name: String, spec: Option<serde_json::Value> },
}

/// What a keymap does.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyAction {
    Command(String),
    Lua(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Keymap {
    pub chord: Chord,
    pub key: String,
    pub desc: String,
    pub action: KeyAction,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UserCommand {
    pub name: String,
    pub desc: String,
    pub complete: Vec<String>,
    callback: u64,
}

/// A finished `deck.job`.
/// A service command contributed by an installed plugin.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ServiceCommand {
    pub plugin: String,
    pub name: String,
    pub description: String,
    pub args: Vec<String>,
    #[serde(skip)]
    callback: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JobDone {
    pub id: u64,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Where finished jobs go: the deck's event channel.
pub type JobSink = Arc<dyn Fn(JobDone) + Send + Sync>;

#[derive(Clone)]
struct SectionDef {
    id: String,
    title: String,
    lines: Option<u64>,
    fixed: Vec<Styled>,
    phone: bool,
    sidebar: bool,
    pinned: bool,
    header: Option<HeaderPlace>,
}

struct Timer {
    due: Instant,
    every: Option<Duration>,
}

/// Where `deck.state()` reads the deck's state: each top-level field is made
/// JSON only when a script asks for it.
pub trait StateSource {
    /// One top-level field as the whole state holds it; none when the state
    /// leaves it out.
    fn field(&self, name: &str) -> Option<serde_json::Value>;
    /// The whole state.
    fn whole(&self) -> serde_json::Map<String, serde_json::Value>;
}

impl StateSource for Arc<crate::protocol::DeckState> {
    fn field(&self, name: &str) -> Option<serde_json::Value> {
        pick(&**self, name)
    }
    fn whole(&self) -> serde_json::Map<String, serde_json::Value> {
        match serde_json::to_value(&**self) {
            Ok(serde_json::Value::Object(fields)) => fields,
            _ => serde_json::Map::new(),
        }
    }
}

/// The state scripts read, with the fields already asked for kept as JSON
/// until a later state changes them.
#[derive(Default)]
struct ScriptState {
    source: Option<Rc<dyn StateSource>>,
    fields: serde_json::Map<String, serde_json::Value>,
    /// `fields` holds every field of the state.
    whole: bool,
}

impl ScriptState {
    fn get(&mut self, names: Option<Vec<String>>) -> serde_json::Value {
        let Some(names) = names else {
            if !self.whole {
                if let Some(source) = &self.source {
                    self.fields = source.whole();
                }
                self.whole = true;
            }
            return serde_json::Value::Object(self.fields.clone());
        };
        let mut out = serde_json::Map::new();
        for name in names {
            if !self.whole && !self.fields.contains_key(&name) {
                if let Some(value) = self.source.as_ref().and_then(|source| source.field(&name)) {
                    self.fields.insert(name.clone(), value);
                }
            }
            if let Some(value) = self.fields.get(&name) {
                out.insert(name, value.clone());
            }
        }
        serde_json::Value::Object(out)
    }

    fn field(&mut self, name: &str) -> serde_json::Value {
        self.get(Some(vec![name.to_string()])).as_object_mut().and_then(|m| m.remove(name)).unwrap_or_default()
    }
}

/// One top-level field of `value` as JSON, exactly as serializing all of
/// `value` gives it (none when that leaves the field out), without
/// serializing any other field.
pub fn pick<T: Serialize>(value: &T, name: &str) -> Option<serde_json::Value> {
    value.serialize(Picker { name }).ok().flatten()
}

struct Picker<'a> {
    name: &'a str,
}

struct Picked<'a> {
    name: &'a str,
    found: Option<serde_json::Value>,
}

impl serde::ser::SerializeStruct for Picked<'_> {
    type Ok = Option<serde_json::Value>;
    type Error = serde_json::Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error> {
        if key == self.name {
            self.found = Some(serde_json::to_value(value)?);
        }
        Ok(())
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(self.found)
    }
}

macro_rules! not_a_struct {
    ($($method:ident($($arg:ty),*);)*) => {
        $(fn $method(self, $(_: $arg),*) -> Result<Self::Ok, Self::Error> {
            Err(serde::ser::Error::custom("not a struct"))
        })*
    };
}

type NotAStruct = serde::ser::Impossible<Option<serde_json::Value>, serde_json::Error>;

impl<'a> serde::Serializer for Picker<'a> {
    type Ok = Option<serde_json::Value>;
    type Error = serde_json::Error;
    type SerializeSeq = NotAStruct;
    type SerializeTuple = NotAStruct;
    type SerializeTupleStruct = NotAStruct;
    type SerializeTupleVariant = NotAStruct;
    type SerializeMap = NotAStruct;
    type SerializeStruct = Picked<'a>;
    type SerializeStructVariant = NotAStruct;
    not_a_struct! {
        serialize_bool(bool); serialize_i8(i8); serialize_i16(i16); serialize_i32(i32); serialize_i64(i64);
        serialize_u8(u8); serialize_u16(u16); serialize_u32(u32); serialize_u64(u64);
        serialize_f32(f32); serialize_f64(f64); serialize_char(char); serialize_str(&str);
        serialize_bytes(&[u8]); serialize_none(); serialize_unit(); serialize_unit_struct(&'static str);
        serialize_unit_variant(&'static str, u32, &'static str);
    }
    fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<Self::Ok, Self::Error> {
        Err(serde::ser::Error::custom("not a struct"))
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(self, _: &'static str, _: &T) -> Result<Self::Ok, Self::Error> {
        Err(serde::ser::Error::custom("not a struct"))
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<Self::Ok, Self::Error> {
        Err(serde::ser::Error::custom("not a struct"))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Err(serde::ser::Error::custom("not a struct"))
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(serde::ser::Error::custom("not a struct"))
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(serde::ser::Error::custom("not a struct"))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(serde::ser::Error::custom("not a struct"))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Err(serde::ser::Error::custom("not a struct"))
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(Picked { name: self.name, found: None })
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(serde::ser::Error::custom("not a struct"))
    }
}

/// What the `deck` functions share with the engine.
#[derive(Default)]
struct Shared {
    requests: Vec<Request>,
    keymaps: Vec<Keymap>,
    commands: Vec<UserCommand>,
    service_commands: Vec<ServiceCommand>,
    sections: Vec<SectionDef>,
    machines: Vec<Machine>,
    badges: Vec<u64>,
    handlers: Vec<(String, u64)>,
    timers: BTreeMap<u64, Timer>,
    watches: BTreeMap<u64, files::Watch>,
    jobs: HashMap<u64, u64>,
    callbacks: HashMap<u64, RegistryKey>,
    /// A picker's `on_close`, by the picker's callback.
    closers: HashMap<u64, u64>,
    /// A picker's `on_tab`, by the picker's callback.
    tabbers: HashMap<u64, u64>,
    next_id: u64,
    state: ScriptState,
}

impl Shared {
    fn keep(&mut self, lua: &Lua, f: Function) -> mlua::Result<u64> {
        self.next_id += 1;
        let key = lua.create_registry_value(f)?;
        self.callbacks.insert(self.next_id, key);
        Ok(self.next_id)
    }
}

type SharedRef = Rc<RefCell<Shared>>;

/// Where the deck finds the scripts.
#[derive(Clone, Debug)]
pub struct Places {
    /// `init.lua`, `lua/` and `plugins/`.
    pub config_dir: PathBuf,
    /// Where the deck starts sessions: the bundled plugins look there.
    pub start_dir: PathBuf,
}

pub struct Engine {
    lua: Lua,
    shared: SharedRef,
    /// The calling thread's processor time (`thread_cpu`) the running
    /// script must end by. Apart from `shared`, so the instruction hook reads
    /// it while a `deck` function holds that borrowed (OPS-S12801: a table's
    /// `__index` ran there with no limit).
    deadline: Rc<Cell<Option<Duration>>>,
    places: Places,
    source_dir: Option<PathBuf>,
    sink: JobSink,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    errors: Vec<String>,
    /// The scripts loaded, for `:reload` and the help sheet.
    pub loaded: Vec<String>,
}

fn err(text: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(text.into())
}

/// The deck's data as a Lua value, a JSON `null` as `nil`. mlua makes it a
/// userdata by default, which is truthy: `s.worktree and s.worktree.task`
/// then failed for a terminal, which has no worktree, on every draw.
fn to_lua(lua: &Lua, value: &impl serde::Serialize) -> mlua::Result<Value> {
    let options = mlua::serde::SerializeOptions::new().serialize_none_to_null(false).serialize_unit_to_null(false);
    lua.to_value_with(value, options)
}

/// A segment's text on one line: a line break or tab becomes a space and
/// other control characters go, so a script cannot move the cursor or
/// colour the screen. Its spaces stay: segments are drawn side by side, and
/// `{ " up", "ok" }, { " 3" }` would otherwise run together as "up3".
fn segment_text(s: &str) -> String {
    let flat: String =
        s.chars().filter_map(|c| if c.is_whitespace() { Some(' ') } else { (!c.is_control()).then_some(c) }).collect();
    crate::util::truncate_width(&flat, MAX_LINE)
}

/// One line of section text: a string, or a list of `{text, style}` pairs.
fn styled(value: Value) -> mlua::Result<Styled> {
    let clip = |s: String| segment_text(&s);
    match value {
        Value::String(s) => Ok(vec![Segment { text: clip(s.to_string_lossy()), style: String::new() }]),
        Value::Integer(n) => Ok(vec![Segment { text: n.to_string(), style: String::new() }]),
        Value::Number(n) => Ok(vec![Segment { text: n.to_string(), style: String::new() }]),
        Value::Table(t) => {
            // A meter: `{meter = 0..100, width = cells, style = name}`.
            if let Some(fill) = t.get::<Option<f64>>("meter")? {
                return Ok(meter(fill, t.get::<Option<usize>>("width")?, t.get::<Option<String>>("style")?));
            }
            // A single `{text, style}` pair, or a list of them. A failed read
            // is the table's `__index` failing, such as at the time limit.
            if let Value::String(text) = t.get::<Value>(1)? {
                let style: Option<String> = t.get(2)?;
                return Ok(vec![Segment { text: clip(text.to_string_lossy()), style: style_name(style) }]);
            }
            let mut out = Vec::new();
            for part in t.sequence_values::<Value>() {
                out.extend(styled(part?)?);
            }
            Ok(out)
        }
        Value::Nil => Ok(Vec::new()),
        other => Err(err(format!("a line is text or {{text, style}} pairs, not {}", other.type_name()))),
    }
}

/// A bar `width` cells wide (10 unless given), `fill` percent of it lit in
/// `style`, or in the colour of how full it is.
fn meter(fill: f64, width: Option<usize>, style: Option<String>) -> Styled {
    let fill = if fill.is_nan() { 0.0 } else { fill.clamp(0.0, 100.0) };
    let cells = width.unwrap_or(10).clamp(1, 40);
    let lit = ((fill / 100.0) * cells as f64).round() as usize;
    let level = if fill >= 85.0 {
        "bad"
    } else if fill >= 60.0 {
        "warn"
    } else {
        "ok"
    };
    let style = Some(style_name(style)).filter(|s| !s.is_empty()).unwrap_or_else(|| level.to_string());
    [(lit, style), (cells - lit, "faint".to_string())]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, style)| Segment { text: "━".repeat(n), style })
        .collect()
}

/// At most this many table columns, figures and pages.
const MAX_COLUMNS: usize = 12;
const MAX_STATS: usize = 8;
const MAX_TABS: usize = 8;

/// Table columns: each a title, or `{title, align = "right"}`.
fn columns_of(value: Value) -> mlua::Result<Vec<PickColumn>> {
    let Value::Table(list) = value else {
        return match value {
            Value::Nil => Ok(Vec::new()),
            other => Err(err(format!("columns is a list, not {}", other.type_name()))),
        };
    };
    let mut out = Vec::new();
    for entry in list.sequence_values::<Value>().take(MAX_COLUMNS) {
        let column = match entry? {
            Value::String(title) => PickColumn { title: segment_text(&title.to_string_lossy()), right: false },
            Value::Table(c) => {
                let title = match c.get::<Option<String>>(1)? {
                    Some(title) => title,
                    None => c.get::<Option<String>>("title")?.unwrap_or_default(),
                };
                let right = match c.get::<Option<String>>("align")?.as_deref() {
                    None | Some("left") => false,
                    Some("right") => true,
                    Some(other) => return Err(err(format!("a column aligns left or right, not {other:?}"))),
                };
                PickColumn { title: segment_text(&title), right }
            }
            other => return Err(err(format!("a column is a title or {{title, align}}, not {}", other.type_name()))),
        };
        out.push(column);
    }
    Ok(out)
}

/// One picker row: a line as `styled` takes it, which as a table may also
/// say `heading = true`, `inert = true` (shown, never selected),
/// `right = <line>` (drawn against the row's right edge), `cells = {<line>,
/// ...}` (a table row) and, on a heading, `columns` for the rows under it.
fn pick_item(value: Value) -> mlua::Result<PickItem> {
    let Value::Table(t) = &value else {
        return Ok(PickItem::choice(styled(value)?));
    };
    let kind = if t.get::<Option<bool>>("heading")?.unwrap_or(false) {
        ItemKind::Heading
    } else if t.get::<Option<bool>>("inert")?.unwrap_or(false) {
        ItemKind::Text
    } else {
        ItemKind::Choice
    };
    let right = styled(t.get::<Value>("right")?)?;
    let cells = match t.get::<Value>("cells")? {
        Value::Table(list) => list
            .sequence_values::<Value>()
            .take(MAX_COLUMNS)
            .map(|cell| cell.and_then(styled))
            .collect::<mlua::Result<_>>()?,
        Value::Nil => Vec::new(),
        other => return Err(err(format!("cells is a list, not {}", other.type_name()))),
    };
    let columns = columns_of(t.get::<Value>("columns")?)?;
    Ok(PickItem { text: styled(value)?, right, kind, cells, columns })
}

/// A picker's figures: `{value = <line>, label = text}` each.
fn stats_of(value: Value) -> mlua::Result<Vec<PickStat>> {
    let Value::Table(list) = value else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for entry in list.sequence_values::<Table>().take(MAX_STATS) {
        let entry = entry?;
        let label = entry.get::<Option<String>>("label")?.map(|s| segment_text(&s)).unwrap_or_default();
        out.push(PickStat { value: styled(entry.get::<Value>("value")?)?, label });
    }
    Ok(out)
}

/// A picker's pages: each a label, or `{label, count}`.
fn tabs_of(value: Value) -> mlua::Result<Vec<PickTab>> {
    let Value::Table(list) = value else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for entry in list.sequence_values::<Value>().take(MAX_TABS) {
        let tab = match entry? {
            Value::String(label) => PickTab { label: segment_text(&label.to_string_lossy()), count: String::new() },
            Value::Table(tab) => {
                let label = match tab.get::<Option<String>>(1)? {
                    Some(label) => label,
                    None => tab.get::<Option<String>>("label")?.unwrap_or_default(),
                };
                let count = match tab.get::<Value>("count")? {
                    Value::Nil => match tab.get::<Value>(2)? {
                        Value::Nil => String::new(),
                        other => count_text(other)?,
                    },
                    other => count_text(other)?,
                };
                PickTab { label: segment_text(&label), count }
            }
            other => return Err(err(format!("a tab is a label or {{label, count}}, not {}", other.type_name()))),
        };
        out.push(tab);
    }
    Ok(out)
}

fn count_text(value: Value) -> mlua::Result<String> {
    match value {
        Value::Integer(n) => Ok(n.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => Ok(segment_text(&s.to_string_lossy())),
        other => Err(err(format!("a tab's count is a number or text, not {}", other.type_name()))),
    }
}

fn style_name(style: Option<String>) -> String {
    style.filter(|s| STYLES.contains(&s.as_str())).unwrap_or_default()
}

fn lines_of(value: Value) -> mlua::Result<Vec<Styled>> {
    match value {
        Value::Table(t) => {
            let mut out = Vec::new();
            for line in t.sequence_values::<Value>().take(MAX_PANEL_LINES) {
                out.push(styled(line?)?);
            }
            Ok(out)
        }
        Value::Nil => Ok(Vec::new()),
        other => Ok(vec![styled(other)?]),
    }
}

/// A `deck.ui.machine` spec, refused unless every reading is one a sidebar
/// row can draw: percents from 0 to 100, disk bytes from 0.
fn machine_of(id: String, spec: &Table) -> mlua::Result<Machine> {
    let text = |key: &str, max: usize| -> mlua::Result<Option<String>> {
        match spec.get::<Option<String>>(key)? {
            Some(value) if value.chars().count() > max => Err(err(format!("machine {key} is longer than {max}"))),
            value => Ok(value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())),
        }
    };
    let percent = |key: &str| -> mlua::Result<Option<f64>> {
        match spec.get::<Option<f64>>(key)? {
            Some(value) if !(0.0..=100.0).contains(&value) => {
                Err(err(format!("machine {key} must be a percent from 0 to 100")))
            }
            value => Ok(value),
        }
    };
    let bytes = |key: &str| -> mlua::Result<Option<u64>> {
        match spec.get::<Option<f64>>(key)? {
            Some(value) if !value.is_finite() || value < 0.0 || value >= u64::MAX as f64 => {
                Err(err(format!("machine {key} must be a byte count from 0")))
            }
            value => Ok(value.map(|v| v as u64)),
        }
    };
    let label = text("label", MAX_MACHINE_LABEL)?.ok_or_else(|| err("a machine needs a label"))?;
    Ok(Machine {
        label,
        cpu: percent("cpu")?,
        memory: percent("memory")?,
        disk_used: bytes("disk_used")?,
        disk_available: bytes("disk_available")?,
        status: text("status", MAX_MACHINE_STATUS)?,
        sidebar: spec.get::<Option<bool>>("sidebar")?.unwrap_or(true),
        id,
    })
}

fn argv_of(value: Value) -> mlua::Result<Vec<String>> {
    let argv: Vec<String> = match value {
        Value::Table(t) => t.sequence_values::<String>().collect::<mlua::Result<_>>()?,
        _ => {
            return Err(err(
                "a program is a list of words, as {\"git\", \"status\"}: the deck never runs a shell line",
            ));
        }
    };
    if argv.is_empty() || argv[0].is_empty() {
        return Err(err("the list of words is empty"));
    }
    Ok(argv)
}

fn opt_string(t: &Option<Table>, key: &str) -> mlua::Result<Option<String>> {
    match t {
        Some(t) => t.get::<Option<String>>(key),
        None => Ok(None),
    }
}

fn limit_of(t: &Option<Table>, default: Duration, max: Duration) -> mlua::Result<Duration> {
    let secs: Option<f64> = match t {
        Some(t) => t.get("timeout")?,
        None => None,
    };
    Ok(secs.map_or(default, |s| Duration::from_secs_f64(s.clamp(0.1, max.as_secs_f64()))))
}

/// A Lua value as text for `:lua =expr`.
pub fn show(lua: &Lua, value: &Value) -> String {
    match value {
        Value::Nil => "nil".into(),
        Value::String(s) => s.to_string_lossy(),
        Value::Table(_) => match lua.from_value::<serde_json::Value>(value.clone()) {
            Ok(json) => serde_json::to_string(&json).unwrap_or_else(|_| "table".into()),
            Err(_) => "table".into(),
        },
        other => other.to_string().unwrap_or_else(|_| other.type_name().to_string()),
    }
}

fn valid_id(name: &str) -> mlua::Result<()> {
    if name.is_empty() || name.len() > 32 || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(err("plugin and command names must be 1–32 letters, digits, - or _"));
    }
    Ok(())
}

fn service_context(lua: &Lua) -> mlua::Result<()> {
    let deck: Table = lua.globals().get("deck")?;
    if deck.get::<String>("context")? != "service" {
        return Err(err("this operation belongs to a service plugin"));
    }
    Ok(())
}

fn bounded_json(lua: &Lua, value: Value) -> mlua::Result<serde_json::Value> {
    let value: serde_json::Value = lua.from_value(value)?;
    if serde_json::to_vec(&value).map_err(mlua::Error::external)?.len() > 16 * 1024 {
        return Err(err("plugin payload exceeds 16 KiB"));
    }
    Ok(value)
}

impl Engine {
    /// Select code separately from personal configuration and persistent data.
    pub fn use_source_dir(&mut self, directory: Option<PathBuf>) {
        self.source_dir = directory.map(|path| path.canonicalize().unwrap_or(path));
        let root = self.source_dir.as_ref().unwrap_or(&self.places.config_dir);
        let result = (|| -> mlua::Result<()> {
            let deck: Table = self.lua.globals().get("deck")?;
            deck.set("plugin_dir", root.to_string_lossy().into_owned())?;
            deck.set("plugin_source_dir", self.source_dir.as_ref().map(|path| path.to_string_lossy().into_owned()))?;
            let package: Table = self.lua.globals().get("package")?;
            package.set("path", format!("{0}/lua/?.lua;{0}/lua/?/init.lua", root.to_string_lossy().replace(';', "")))
        })();
        if let Err(error) = result {
            self.errors.push(format!("Lua plugin source: {error}"));
        }
    }

    fn plugin_files(&mut self, service: bool) -> Vec<(String, PathBuf)> {
        if let Some(root) = &self.source_dir {
            return match sources::files(root, service) {
                Ok(files) => files,
                Err(error) => {
                    self.errors.push(format!("Lua plugin source: {error}"));
                    Vec::new()
                }
            };
        }
        let directory = self.places.config_dir.join(if service { "service" } else { "plugins" });
        let mut files: Vec<_> = std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "lua"))
            .map(|path| (path.file_stem().unwrap().to_string_lossy().into_owned(), path))
            .collect();
        files.sort();
        files.truncate(64);
        files
    }

    fn load_bundled(&mut self, id: &str, context: &str, source: &str) {
        match &self.source_dir {
            Some(root) => match sources::bundled(root, id, context) {
                Ok(file) => self.load_file(&file),
                Err(error) => self.errors.push(format!("Lua plugin source: {error}")),
            },
            None => self.load_chunk(id, source),
        }
    }

    pub fn new_service(places: Places, sink: JobSink) -> mlua::Result<Engine> {
        let engine = Self::new(places, sink)?;
        let deck: Table = engine.lua.globals().get("deck")?;
        deck.set("context", "service")?;
        Ok(engine)
    }

    pub fn cancel_jobs(&self) {
        self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn has_jobs(&self) -> bool {
        !self.shared.borrow().jobs.is_empty()
    }

    pub fn service_commands(&self) -> Vec<ServiceCommand> {
        self.shared.borrow().service_commands.clone()
    }

    pub fn service_command(
        &mut self,
        plugin: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let command = self
            .shared
            .borrow()
            .service_commands
            .iter()
            .find(|c| c.plugin == plugin && c.name == name)
            .cloned()
            .ok_or_else(|| "No such enabled plugin command".to_string())?;
        if serde_json::to_vec(args).map_err(|e| e.to_string())?.len() > 16 * 1024 {
            return Err("Plugin arguments exceed 16 KiB".into());
        }
        let args = to_lua(&self.lua, args).map_err(|e| e.to_string())?;
        let value = self
            .call(&format!("{}.{}", plugin, name), command.callback, args)
            .ok_or_else(|| "Plugin command failed; see the service errors".to_string())?;
        bounded_json(&self.lua, value).map_err(|e| e.to_string())
    }

    pub fn service_result(&mut self, callback: u64, data: &serde_json::Value, error: Option<&str>) {
        if let Ok(result) = to_lua(&self.lua, &serde_json::json!({"ok": error.is_none(), "data": data, "error": error}))
        {
            self.call("service reply", callback, result);
        }
        self.forget(callback);
    }

    /// Service scripts run once per service, independently of attached clients.
    #[cfg(test)]
    pub fn load_service(&mut self) {
        self.load_service_with(&BTreeMap::new());
    }

    pub fn load_service_with(&mut self, settings: &BTreeMap<String, bool>) {
        let init = self.places.config_dir.join("service.lua");
        if init.is_file() {
            self.load_file(&init);
        }
        let files = self.plugin_files(true);
        for (id, file) in &files {
            if settings.get(id) != Some(&false) {
                self.load_file(file);
            }
        }
        for &(id, context, source) in BUNDLED_SCRIPTS {
            if context == "service" && settings.get(id) != Some(&false) && !files.iter().any(|(name, _)| name == id) {
                self.load_bundled(id, context, source);
            }
        }
        self.emit("ready", Value::Nil);
    }

    /// A fresh engine: the `deck` API in place, no script loaded yet.
    pub fn new(places: Places, sink: JobSink) -> mlua::Result<Engine> {
        let lua = Lua::new();
        lua.set_memory_limit(MEMORY_LIMIT)?;
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let shared: SharedRef = Rc::default();
        shared.borrow_mut().next_id = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) << 32;
        let deadline: Rc<Cell<Option<Duration>>> = Rc::default();
        {
            let deadline = deadline.clone();
            lua.set_global_hook(HookTriggers::new().every_nth_instruction(10_000), move |_, _| match deadline.get() {
                Some(deadline) if thread_cpu() > deadline => {
                    Err(err("it ran too long and was stopped (scripts get a quarter of a second of processor time)"))
                }
                _ => Ok(VmState::Continue),
            })?;
        }
        let engine = Engine {
            lua,
            shared,
            deadline,
            places,
            source_dir: None,
            sink,
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            errors: Vec::new(),
            loaded: Vec::new(),
        };
        engine.install_api()?;
        Ok(engine)
    }

    fn install_api(&self) -> mlua::Result<()> {
        let lua = &self.lua;
        let deck = lua.create_table()?;
        let ui = lua.create_table()?;
        let json = lua.create_table()?;
        deck.set("opt", lua.create_table()?)?;
        deck.set("version", env!("CARGO_PKG_VERSION"))?;
        deck.set("exe", std::env::current_exe().map_err(mlua::Error::external)?.to_string_lossy().into_owned())?;
        deck.set("context", "ui")?;
        deck.set("config_dir", self.places.config_dir.to_string_lossy().into_owned())?;
        deck.set("plugin_dir", self.places.config_dir.to_string_lossy().into_owned())?;
        deck.set("start_dir", self.places.start_dir.to_string_lossy().into_owned())?;
        let styles = lua.create_sequence_from(STYLES.iter().copied())?;
        ui.set("styles", styles)?;

        let shared = self.shared.clone();
        deck.set(
            "keymap",
            lua.create_function(move |lua, (key, action, opts): (String, Value, Option<Table>)| {
                let chord = Chord::parse(&key).map_err(err)?;
                // Read before borrowing: the table's `__index` is Lua too.
                let desc = opt_string(&opts, "desc")?.unwrap_or_default();
                let mut s = shared.borrow_mut();
                if s.keymaps.len() >= MAX_BINDINGS {
                    return Err(err("too many keymaps"));
                }
                let action = match action {
                    Value::String(line) => KeyAction::Command(line.to_string_lossy()),
                    Value::Function(f) => KeyAction::Lua(s.keep(lua, f)?),
                    _ => return Err(err("a keymap runs a command line or a function")),
                };
                s.keymaps.retain(|k| k.chord != chord);
                s.keymaps.push(Keymap { chord, key, desc, action });
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        deck.set(
            "command",
            lua.create_function(move |lua, (name, f, opts): (String, Function, Option<Table>)| {
                let valid = !name.is_empty()
                    && name.len() <= 32
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                if !valid {
                    return Err(err(format!("{name:?} is not a command name (letters, digits, - and _)")));
                }
                if crate::actions::is_builtin(&name) {
                    return Err(err(format!(":{name} is one of the deck's own commands")));
                }
                let complete: Vec<String> = match &opts {
                    Some(t) => t.get::<Option<Vec<String>>>("complete")?.unwrap_or_default(),
                    None => Vec::new(),
                };
                let desc = opt_string(&opts, "desc")?.unwrap_or_default();
                let mut s = shared.borrow_mut();
                if s.commands.len() >= MAX_BINDINGS {
                    return Err(err("too many commands"));
                }
                let callback = s.keep(lua, f)?;
                s.commands.retain(|c| c.name != name);
                s.commands.push(UserCommand { name, desc, complete, callback });
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        deck.set(
            "reorder",
            lua.create_function(move |_, ids: Vec<u64>| {
                if ids.len() > 4096 || ids.iter().collect::<std::collections::HashSet<_>>().len() != ids.len() {
                    return Err(err("session order must have at most 4096 unique session ids"));
                }
                shared.borrow_mut().requests.push(Request::Reorder(ids));
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        deck.set(
            "cmd",
            lua.create_function(move |_, line: String| {
                shared.borrow_mut().requests.push(Request::Command(line));
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        deck.set(
            "notify",
            lua.create_function(move |_, (text, opts): (String, Option<Table>)| {
                let error = match &opts {
                    Some(t) => t.get::<Option<bool>>("error")?.unwrap_or(false),
                    None => false,
                };
                shared.borrow_mut().requests.push(Request::Notify { text: crate::util::one_line(&text), error });
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        deck.set(
            "state",
            lua.create_function(move |lua, fields: Option<Vec<String>>| {
                // Copy out before Lua allocation, which may run a finalizer.
                // Callbacks ask for only their inputs; only those become JSON.
                let state = shared.borrow_mut().state.get(fields);
                to_lua(lua, &state)
            })?,
        )?;

        let shared = self.shared.clone();
        deck.set(
            "on",
            lua.create_function(move |lua, (event, f): (String, Function)| {
                let mut s = shared.borrow_mut();
                if s.handlers.len() >= MAX_BINDINGS {
                    return Err(err("too many event handlers"));
                }
                let id = s.keep(lua, f)?;
                s.handlers.push((event, id));
                Ok(())
            })?,
        )?;

        for (name, repeat) in [("every", true), ("after", false)] {
            let shared = self.shared.clone();
            deck.set(
                name,
                lua.create_function(move |lua, (secs, f): (f64, Function)| {
                    let mut s = shared.borrow_mut();
                    if s.timers.len() >= MAX_TIMERS {
                        return Err(err("too many timers"));
                    }
                    let wait = Duration::from_secs_f64(secs.clamp(0.0, 86_400.0));
                    let wait = if repeat { wait.max(MIN_EVERY) } else { wait };
                    let id = s.keep(lua, f)?;
                    s.timers.insert(id, Timer { due: Instant::now() + wait, every: repeat.then_some(wait) });
                    Ok(id)
                })?,
            )?;
        }
        let shared = self.shared.clone();
        deck.set(
            "cancel",
            lua.create_function(move |_, id: u64| {
                let mut s = shared.borrow_mut();
                s.timers.remove(&id);
                s.watches.remove(&id);
                s.callbacks.remove(&id);
                Ok(())
            })?,
        )?;

        let start_dir = self.places.start_dir.clone();
        deck.set(
            "run",
            lua.create_function(move |lua, (argv, opts): (Value, Option<Table>)| {
                let argv = argv_of(argv)?;
                let limit = limit_of(&opts, RUN_LIMIT, RUN_LIMIT)?;
                let cwd = opt_string(&opts, "cwd")?.map(PathBuf::from).unwrap_or_else(|| start_dir.clone());
                let ran =
                    crate::run::run(&argv, Some(&cwd), &[], limit).map_err(|e| err(format!("{}: {e}", argv[0])))?;
                let out = lua.create_table()?;
                out.set("code", ran.code)?;
                out.set("ok", ran.ok())?;
                out.set("stdout", ran.stdout)?;
                out.set("stderr", ran.stderr)?;
                out.set("timed_out", ran.timed_out)?;
                Ok(out)
            })?,
        )?;

        let shared = self.shared.clone();
        let sink = self.sink.clone();
        let cancel = self.cancel.clone();
        let start_dir = self.places.start_dir.clone();
        deck.set(
            "job",
            lua.create_function(move |lua, (argv, opts, f): (Value, Option<Table>, Function)| {
                if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(err("service is stopping"));
                }
                let argv = argv_of(argv)?;
                let limit = limit_of(&opts, JOB_DEFAULT, JOB_LIMIT)?;
                let cwd = opt_string(&opts, "cwd")?.map(PathBuf::from).unwrap_or_else(|| start_dir.clone());
                let id = {
                    let mut s = shared.borrow_mut();
                    if s.jobs.len() >= MAX_JOBS {
                        return Err(err(format!("{MAX_JOBS} jobs are running already")));
                    }
                    let id = s.keep(lua, f)?;
                    s.jobs.insert(id, id);
                    id
                };
                let sink = sink.clone();
                let cancel = cancel.clone();
                let started = std::thread::Builder::new().name("deck-lua-job".into()).spawn(move || {
                    let done = match crate::run::with_cancellation(cancel, || {
                        crate::run::run(&argv, Some(&cwd), &[], limit)
                    }) {
                        Ok(ran) => JobDone {
                            id,
                            code: ran.code,
                            stdout: ran.stdout,
                            stderr: ran.stderr,
                            timed_out: ran.timed_out,
                        },
                        Err(e) => JobDone {
                            id,
                            code: None,
                            stdout: String::new(),
                            stderr: format!("{}: {e}", argv[0]),
                            timed_out: false,
                        },
                    };
                    sink(done);
                });
                if let Err(e) = started {
                    let mut s = shared.borrow_mut();
                    s.jobs.remove(&id);
                    s.callbacks.remove(&id);
                    return Err(err(format!("could not start the job: {e}")));
                }
                Ok(id)
            })?,
        )?;

        let service = lua.create_table()?;
        let shared = self.shared.clone();
        service.set(
            "command",
            lua.create_function(move |lua, (plugin, name, f, opts): (String, String, Function, Option<Table>)| {
                service_context(lua)?;
                valid_id(&plugin)?;
                valid_id(&name)?;
                let description = opt_string(&opts, "desc")?.unwrap_or_default();
                let args = opts
                    .as_ref()
                    .map(|t| t.get::<Option<Vec<String>>>("args"))
                    .transpose()?
                    .flatten()
                    .unwrap_or_default();
                if args.len() > 16 || args.iter().any(|name| valid_id(name).is_err()) {
                    return Err(err("service arguments must be at most 16 ordinary names"));
                }
                let mut shared = shared.borrow_mut();
                if shared.service_commands.len() >= MAX_BINDINGS {
                    return Err(err("too many service commands"));
                }
                if shared.service_commands.iter().any(|c| c.plugin == plugin && c.name == name) {
                    return Err(err("a service command is already registered with that name"));
                }
                let callback = shared.keep(lua, f)?;
                shared.service_commands.push(ServiceCommand { plugin, name, description, args, callback });
                Ok(())
            })?,
        )?;
        let shared = self.shared.clone();
        service.set(
            "call",
            lua.create_function(
                move |lua, (plugin, command, args, callback): (String, String, Value, Option<Function>)| {
                    valid_id(&plugin)?;
                    valid_id(&command)?;
                    let args = bounded_json(lua, args)?;
                    let mut shared = shared.borrow_mut();
                    let callback = Some(match callback {
                        Some(f) => shared.keep(lua, f)?,
                        None => {
                            shared.next_id += 1;
                            shared.next_id
                        }
                    });
                    shared.requests.push(Request::ServiceCall { plugin, command, args, callback });
                    Ok(())
                },
            )?,
        )?;
        let shared = self.shared.clone();
        service.set(
            "annotate",
            lua.create_function(move |lua, (plugin, id, data): (String, u64, Value)| {
                service_context(lua)?;
                valid_id(&plugin)?;
                let data = bounded_json(lua, data)?;
                if !data.is_object() {
                    return Err(err("session metadata must be an object"));
                }
                shared.borrow_mut().requests.push(Request::Annotate { plugin, id, data });
                Ok(())
            })?,
        )?;
        let shared = self.shared.clone();
        service.set(
            "title",
            lua.create_function(move |lua, (plugin, id, text): (String, u64, Option<String>)| {
                service_context(lua)?;
                valid_id(&plugin)?;
                let text = text.map(|text| crate::util::one_line(&text).chars().take(200).collect());
                shared.borrow_mut().requests.push(Request::Title { plugin, id, text });
                Ok(())
            })?,
        )?;
        let shared = self.shared.clone();
        service.set(
            "process",
            lua.create_function(move |lua, (plugin, name, spec): (String, String, Value)| {
                service_context(lua)?;
                valid_id(&plugin)?;
                valid_id(&name)?;
                let spec = if matches!(spec, Value::Nil) { None } else { Some(bounded_json(lua, spec)?) };
                shared.borrow_mut().requests.push(Request::Process { plugin, name, spec });
                Ok(())
            })?,
        )?;
        deck.set("service", service)?;
        let slots = lua.create_table()?;
        for (name, action) in [
            ("hold", "slot-hold"),
            ("release", "slot-release"),
            ("admission", "admission"),
            ("enqueue", "enqueue"),
            ("cancel", "cancel"),
            ("rerun", "rerun"),
            ("record_rerun", "rerun-record"),
        ] {
            let shared = self.shared.clone();
            slots.set(
                name,
                lua.create_function(move |lua, (args, callback): (Value, Option<Function>)| {
                    let args = bounded_json(lua, args)?;
                    let mut s = shared.borrow_mut();
                    let callback = if let Some(f) = callback {
                        s.keep(lua, f)?
                    } else {
                        s.next_id += 1;
                        s.next_id
                    };
                    s.requests.push(Request::Fleet { action: action.into(), args, callback });
                    Ok(())
                })?,
            )?;
        }
        let shared = self.shared.clone();
        slots.set(
            "status",
            lua.create_function(move |lua, ()| {
                let scheduler = shared.borrow_mut().state.field("scheduler");
                to_lua(lua, &scheduler)
            })?,
        )?;
        deck.set("slots", slots.clone())?;
        deck.set("scheduler", slots)?;
        let jobs = lua.create_table()?;
        for operation in ["start", "status", "log", "stop", "dismiss"] {
            let shared = self.shared.clone();
            let action = format!("job-{operation}");
            jobs.set(
                operation,
                lua.create_function(move |lua, (args, callback): (Value, Option<Function>)| {
                    let args = bounded_json(lua, args)?;
                    let mut s = shared.borrow_mut();
                    let callback = if let Some(f) = callback {
                        s.keep(lua, f)?
                    } else {
                        s.next_id += 1;
                        s.next_id
                    };
                    s.requests.push(Request::Fleet { action: action.clone(), args, callback });
                    Ok(())
                })?,
            )?;
        }
        deck.set("job_session", jobs)?;
        for (table_name, prefix, operations) in [
            (
                "attention",
                "attention",
                vec![
                    "status",
                    "list",
                    "send",
                    "message",
                    "cancel",
                    "decide",
                    "supervisor-status",
                    "supervisor-set",
                    "supervisor-rules",
                    "supervisor-off",
                ],
            ),
            ("work_queue", "queue", vec!["add", "list", "cancel", "reorder", "enable", "batch"]),
            ("task_sources", "source", vec!["register", "reconcile", "publish", "fail", "list", "refresh", "queue"]),
            (
                "chores",
                "chore",
                vec!["register", "list", "probe", "enable", "run", "claim", "complete", "update", "remove"],
            ),
        ] {
            let table = lua.create_table()?;
            for operation in operations {
                let shared = self.shared.clone();
                let action = format!("{prefix}-{operation}");
                let service_only = (prefix == "source"
                    && matches!(operation, "register" | "reconcile" | "publish" | "fail"))
                    || prefix == "chore" && matches!(operation, "claim" | "complete");
                let large = prefix == "source" && operation == "publish" || prefix == "queue" && operation == "batch";
                table.set(
                    operation,
                    lua.create_function(move |lua, (args, callback): (Value, Option<Function>)| {
                        if service_only {
                            service_context(lua)?;
                        }
                        let args: serde_json::Value = if large {
                            let args: serde_json::Value = lua.from_value(args)?;
                            if serde_json::to_vec(&args).map_err(mlua::Error::external)?.len() > 262144 {
                                return Err(err("work payload exceeds256KiB"));
                            }
                            args
                        } else {
                            bounded_json(lua, args)?
                        };
                        let mut s = shared.borrow_mut();
                        let callback = if let Some(f) = callback {
                            s.keep(lua, f)?
                        } else {
                            s.next_id += 1;
                            s.next_id
                        };
                        s.requests.push(Request::Fleet { action: action.clone(), args, callback });
                        Ok(())
                    })?,
                )?;
            }
            deck.set(table_name, table)?;
        }

        let shared = self.shared.clone();
        deck.set(
            "new",
            lua.create_function(move |_, (tool, opts): (String, Option<Table>)| {
                let request = Request::Create {
                    tool,
                    host: opt_string(&opts, "host")?,
                    cwd: opt_string(&opts, "cwd")?,
                    name: opt_string(&opts, "name")?,
                    command: Vec::new(),
                };
                shared.borrow_mut().requests.push(request);
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        deck.set(
            "resume",
            lua.create_function(move |_, (tool, conversation, opts): (String, String, Option<Table>)| {
                if !crate::protocol::Tool::parse(&tool).is_some_and(|tool| tool.is_agent()) {
                    return Err(err("resume needs an agent program"));
                }
                if !crate::service::valid_conversation_id(&conversation) {
                    return Err(err("invalid conversation id"));
                }
                // Read before borrowing: the table's `__index` is Lua too.
                let host = opt_string(&opts, "host")?;
                shared.borrow_mut().requests.push(Request::Resume { tool, conversation, host });
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        deck.set(
            "term",
            lua.create_function(move |_, opts: Option<Table>| {
                let command = match &opts {
                    Some(t) => match t.get::<Value>("cmd")? {
                        Value::Nil => Vec::new(),
                        other => argv_of(other)?,
                    },
                    None => Vec::new(),
                };
                let request = Request::Create {
                    tool: "shell".into(),
                    host: opt_string(&opts, "host")?,
                    cwd: opt_string(&opts, "cwd")?,
                    name: opt_string(&opts, "name")?,
                    command,
                };
                shared.borrow_mut().requests.push(request);
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        deck.set(
            "send",
            lua.create_function(move |_, (id, text): (u64, String)| {
                shared.borrow_mut().requests.push(Request::Send { id, text });
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        deck.set(
            "select",
            lua.create_function(move |_, id: u64| {
                shared.borrow_mut().requests.push(Request::Select { id });
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        ui.set(
            "section",
            lua.create_function(move |lua, (id, spec): (String, Table)| {
                let title: Option<String> = spec.get("title")?;
                let phone: Option<bool> = spec.get("phone")?;
                let sidebar: Option<bool> = spec.get("sidebar")?;
                let pinned: Option<bool> = spec.get("pinned")?;
                let header = match spec.get::<Value>("header")? {
                    Value::Nil | Value::Boolean(false) => None,
                    Value::Boolean(true) => Some(HeaderPlace::Right),
                    Value::String(place) if place.to_string_lossy() == "inline" => Some(HeaderPlace::Inline),
                    _ => return Err(err("header must be true or \"inline\"")),
                };
                // Read and convert before borrowing: the tables' `__index` is Lua too.
                let (lines, fixed) = match spec.get::<Value>("lines")? {
                    Value::Function(f) => (Some(f), Vec::new()),
                    other => (None, lines_of(other)?),
                };
                let mut s = shared.borrow_mut();
                let lines = lines.map(|f| s.keep(lua, f)).transpose()?;
                if !s.sections.iter().any(|d| d.id == id) && s.sections.len() >= MAX_PANELS {
                    return Err(err(format!("at most {MAX_PANELS} sections")));
                }
                let def = SectionDef {
                    title: title.unwrap_or_else(|| id.to_uppercase()),
                    id: id.clone(),
                    lines,
                    fixed,
                    phone: phone.unwrap_or(true),
                    sidebar: sidebar.unwrap_or(true),
                    pinned: pinned.unwrap_or(false),
                    header,
                };
                match s.sections.iter_mut().find(|d| d.id == id) {
                    Some(old) => *old = def,
                    None => s.sections.push(def),
                }
                Ok(())
            })?,
        )?;
        let shared = self.shared.clone();
        ui.set(
            "machine",
            lua.create_function(move |_, (id, spec): (String, Table)| {
                // Read and convert before borrowing: the table's `__index` is Lua too.
                let machine = machine_of(id, &spec)?;
                let mut s = shared.borrow_mut();
                match s.machines.iter().position(|m| m.id == machine.id) {
                    Some(i) => s.machines[i] = machine,
                    None if s.machines.len() >= MAX_MACHINES => {
                        return Err(err(format!("at most {MAX_MACHINES} machines")));
                    }
                    None => s.machines.push(machine),
                }
                Ok(())
            })?,
        )?;
        let shared = self.shared.clone();
        ui.set(
            "remove",
            lua.create_function(move |_, id: String| {
                let mut s = shared.borrow_mut();
                s.sections.retain(|d| d.id != id);
                s.machines.retain(|m| m.id != id);
                Ok(())
            })?,
        )?;
        let shared = self.shared.clone();
        ui.set(
            "badge",
            lua.create_function(move |lua, f: Function| {
                let mut s = shared.borrow_mut();
                if s.badges.len() >= MAX_BINDINGS {
                    return Err(err("too many badges"));
                }
                let id = s.keep(lua, f)?;
                s.badges.push(id);
                Ok(())
            })?,
        )?;
        let shared = self.shared.clone();
        ui.set(
            "pick",
            lua.create_function(move |lua, spec: Table| {
                let title: String = spec.get::<Option<String>>("title")?.unwrap_or_else(|| "Pick".into());
                let items: Vec<PickItem> = spec
                    .get::<Table>("items")?
                    .sequence_values::<Value>()
                    .take(2000)
                    .map(|v| v.and_then(pick_item))
                    .collect::<mlua::Result<_>>()?;
                let mut pick = Pick::new(segment_text(&title), items);
                pick.subtitle = spec.get::<Option<String>>("subtitle")?.map(|s| segment_text(&s)).unwrap_or_default();
                pick.filter = spec.get::<Option<bool>>("filter")?.unwrap_or(true);
                pick.selected = spec.get::<Option<usize>>("selected")?.unwrap_or(1).saturating_sub(1);
                if let Some(keys) = spec.get::<Option<Table>>("keys")? {
                    for entry in keys.sequence_values::<Table>().take(8) {
                        let entry = entry?;
                        let key: String = entry.get("key")?;
                        let named = PICK_KEY_NAMES.contains(&key.as_str());
                        let mut chars = key.chars();
                        let single = matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_whitespace() && !c.is_control());
                        if !named && !single {
                            return Err(err(format!(
                                "a picker key is one character or one of {}, not {key:?}",
                                PICK_KEY_NAMES.join(", ")
                            )));
                        }
                        let label = entry.get::<Option<String>>("label")?.map(|s| segment_text(&s)).unwrap_or_default();
                        pick.keys.push(PickKey { key, label });
                    }
                }
                pick.columns = columns_of(spec.get::<Value>("columns")?)?;
                pick.stats = stats_of(spec.get::<Value>("stats")?)?;
                pick.tabs = tabs_of(spec.get::<Value>("tabs")?)?;
                pick.tab =
                    spec.get::<Option<usize>>("tab")?.unwrap_or(1).saturating_sub(1).min(pick.tabs.len().saturating_sub(1));
                let f: Function = spec.get("on_choose")?;
                let close: Option<Function> = spec.get("on_close")?;
                let on_tab: Option<Function> = spec.get("on_tab")?;
                if !pick.tabs.is_empty() && on_tab.is_none() {
                    return Err(err("a picker with tabs needs on_tab to draw each page"));
                }
                let mut s = shared.borrow_mut();
                let callback = s.keep(lua, f)?;
                if let Some(close) = close {
                    let closer = s.keep(lua, close)?;
                    s.closers.insert(callback, closer);
                }
                if let Some(on_tab) = on_tab {
                    let tabber = s.keep(lua, on_tab)?;
                    s.tabbers.insert(callback, tabber);
                }
                s.requests.push(Request::Pick { pick: Box::new(pick), callback });
                Ok(())
            })?,
        )?;

        json.set(
            "encode",
            lua.create_function(|lua, v: Value| {
                let json: serde_json::Value = lua.from_value(v)?;
                serde_json::to_string(&json).map_err(|e| err(e.to_string()))
            })?,
        )?;
        json.set(
            "decode",
            lua.create_function(|lua, text: String| {
                if text.len() > 4 * 1024 * 1024 {
                    return Err(err("the JSON is larger than 4 MB"));
                }
                let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| err(e.to_string()))?;
                to_lua(lua, &json)
            })?,
        )?;

        // A table that becomes a JSON list even when it is empty.
        json.set(
            "array",
            lua.create_function(|lua, t: Option<Table>| {
                let t = match t {
                    Some(t) => t,
                    None => lua.create_table()?,
                };
                t.set_metatable(Some(lua.array_metatable()))?;
                Ok(t)
            })?,
        )?;
        let fs = files::table(&lua)?;
        let shared = self.shared.clone();
        fs.set(
            "watch",
            lua.create_function(move |lua, (paths, f, opts): (Vec<String>, Function, Option<Table>)| {
                // Read before borrowing: the table's `__index` is Lua too.
                let settle = opts.as_ref().map(|t| t.get::<Option<f64>>("settle")).transpose()?.flatten();
                let mut s = shared.borrow_mut();
                if s.watches.len() >= files::MAX_WATCHES {
                    return Err(err("too many watches"));
                }
                if paths.len() > files::MAX_WATCHED {
                    return Err(err("a watch follows at most 64 folders"));
                }
                let settle = settle.filter(|s| s.is_finite()).unwrap_or(1.0).clamp(0.0, 60.0);
                let (watch, watched) =
                    files::Watch::new(&paths, Duration::from_secs_f64(settle)).map_err(|e| err(e.to_string()))?;
                let id = s.keep(lua, f)?;
                s.watches.insert(id, watch);
                Ok((id, watched))
            })?,
        )?;
        deck.set("fs", fs)?;
        deck.set("sha256", lua.create_function(files::sha256)?)?;
        deck.set("ui", ui)?;
        deck.set("json", json)?;
        lua.globals().set("deck", deck.clone())?;
        // `require("deck")` gives the same table, as `require("vim")` would.
        let package: Table = lua.globals().get("package")?;
        let loaded: Table = package.get("loaded")?;
        loaded.set("deck", deck)?;
        let dir = &self.places.config_dir;
        let path = format!("{0}/lua/?.lua;{0}/lua/?/init.lua", dir.to_string_lossy().replace(';', ""));
        package.set("path", path)?;
        package.set("cpath", "")?;
        lua.load(GUARDS).set_name("=guards").exec()
    }

    /// Run `f` under the processor-time limit and collect its error for F2.
    fn guarded<T>(&mut self, what: &str, limit: Duration, f: impl FnOnce(&Lua) -> mlua::Result<T>) -> Option<T> {
        self.deadline.set(Some(thread_cpu() + limit));
        let result = f(&self.lua);
        self.deadline.set(None);
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                let text = e.to_string();
                let first = text.lines().next().unwrap_or("").to_string();
                self.errors.push(format!("Lua {what}: {first}"));
                None
            }
        }
    }

    fn callback(&self, id: u64) -> Option<Function> {
        let s = self.shared.borrow();
        s.callbacks.get(&id).and_then(|key| self.lua.registry_value::<Function>(key).ok())
    }

    fn call(&mut self, what: &str, id: u64, args: impl mlua::IntoLuaMulti) -> Option<Value> {
        self.call_into(what, id, args, Ok)
    }

    /// `call`, with what the function returned turned into `T` under the
    /// same limit: reading a returned table can run its `__index`.
    fn call_into<T>(
        &mut self,
        what: &str,
        id: u64,
        args: impl mlua::IntoLuaMulti,
        convert: impl FnOnce(Value) -> mlua::Result<T>,
    ) -> Option<T> {
        let f = self.callback(id)?;
        let result = self.guarded(what, CALL_LIMIT, |_| f.call::<Value>(args).and_then(convert));
        if result.is_none() {
            // Hook timeouts contain no Lua traceback. Name the registered
            // script in the existing error so the slow handler can be found.
            let info = f.info();
            if let (Some(source), Some(line), Some(error)) =
                (info.source.as_deref().and_then(|s| s.strip_prefix('@')), info.line_defined, self.errors.last_mut())
            {
                let name = Path::new(source).file_name().unwrap_or_default().to_string_lossy();
                let name: String = crate::util::one_line(&name).chars().take(96).collect();
                if !name.is_empty() {
                    error.push_str(&format!(" [{name}:{line}]"));
                }
            }
        }
        result
    }

    /// Load `init.lua`, the plugins and, where they apply, the bundled
    /// plugins. A script that fails stops there; the deck carries on.
    #[cfg(test)]
    pub fn load(&mut self) {
        self.load_with(&BTreeMap::new());
    }

    pub fn load_with(&mut self, settings: &BTreeMap<String, bool>) {
        let init = self.places.config_dir.join("init.lua");
        if init.is_file() {
            self.load_file(&init);
        }
        let files = self.plugin_files(false);
        for (id, file) in &files {
            if settings.get(id) != Some(&false) {
                self.load_file(file);
            }
        }
        for &(id, context, source) in BUNDLED_SCRIPTS {
            if context == "ui"
                && settings.get(id) != Some(&false)
                && !matches!(self.option(id), Some(serde_json::Value::Bool(false)))
                && !files.iter().any(|(name, _)| name == id)
            {
                self.load_bundled(id, context, source);
            }
        }
        self.emit("ready", Value::Nil);
    }

    fn load_file(&mut self, path: &Path) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match std::fs::metadata(path) {
            Ok(m) if m.len() > 1024 * 1024 => {
                self.errors.push(format!("Lua {name}: larger than 1 MB, not loaded"));
                return;
            }
            Err(e) => {
                self.errors.push(format!("Lua {name}: {e}"));
                return;
            }
            _ => {}
        }
        match std::fs::read_to_string(path) {
            Ok(code) => self.load_chunk(&name, &code),
            Err(e) => self.errors.push(format!("Lua {name}: {e}")),
        }
    }

    fn load_chunk(&mut self, name: &str, code: &str) {
        let chunk_name = format!("@{name}");
        // A package may decline activation in this repository without
        // adding itself to the loaded list or exposing any UI.
        if let Some(value) = self.guarded(name, LOAD_LIMIT, |lua| lua.load(code).set_name(chunk_name).eval::<Value>()) {
            if !matches!(value, Value::Boolean(false)) {
                self.loaded.push(name.to_string());
            }
        }
    }

    /// `:lua code`: run it; `=expr` gives its value back as text.
    pub fn eval(&mut self, code: &str) -> Option<String> {
        let (code, show_value) = match code.strip_prefix('=') {
            Some(expr) => (format!("return {expr}"), true),
            None => (code.to_string(), false),
        };
        let value = self.guarded(":lua", CALL_LIMIT, |lua| lua.load(&code).set_name("=:lua").eval::<Value>())?;
        show_value.then(|| show(&self.lua, &value))
    }

    /// Errors since last asked, for F2.
    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    /// What the scripts asked for since last asked.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.shared.borrow_mut().requests)
    }

    pub fn keymaps(&self) -> Vec<Keymap> {
        self.shared.borrow().keymaps.clone()
    }

    /// The machines scripts added to the resource table, in the order added.
    pub fn machines(&self) -> Vec<Machine> {
        self.shared.borrow().machines.clone()
    }

    pub fn commands(&self) -> Vec<UserCommand> {
        self.shared.borrow().commands.clone()
    }

    /// An option from `deck.opt`, as JSON. Read raw, as `options` lists
    /// them: this runs outside any time limit, so no `__index` may run.
    pub fn option(&self, name: &str) -> Option<serde_json::Value> {
        let deck: Table = self.lua.globals().raw_get("deck").ok()?;
        let opt: Table = deck.raw_get("opt").ok()?;
        let value: Value = opt.raw_get(name).ok()?;
        match value {
            Value::Nil => None,
            v => self.lua.from_value(v).ok(),
        }
    }

    /// Every option set, by name.
    pub fn options(&self) -> BTreeMap<String, serde_json::Value> {
        let mut out = BTreeMap::new();
        let Ok(deck) = self.lua.globals().get::<Table>("deck") else { return out };
        let Ok(opt) = deck.get::<Table>("opt") else { return out };
        for (k, v) in opt.pairs::<String, Value>().flatten() {
            if let Ok(json) = self.lua.from_value::<serde_json::Value>(v) {
                out.insert(k, json);
            }
        }
        out
    }

    /// `:set name=value`: the value read as JSON, else as text.
    pub fn set_option(&mut self, name: &str, value: &str) -> Result<(), String> {
        let json: serde_json::Value =
            serde_json::from_str(value).unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
        let set = || -> mlua::Result<()> {
            let deck: Table = self.lua.globals().get("deck")?;
            let opt: Table = deck.get("opt")?;
            opt.set(name, to_lua(&self.lua, &json)?)
        };
        set().map_err(|e| e.to_string())
    }

    /// The deck's state as `deck.state()` gives it, whole.
    #[cfg(test)]
    pub fn set_state(&mut self, state: serde_json::Value) {
        let fields = match state {
            serde_json::Value::Object(fields) => fields,
            _ => serde_json::Map::new(),
        };
        self.shared.borrow_mut().state = ScriptState { source: None, fields, whole: true };
    }

    /// The deck's state, read from `source` as scripts ask for it. With
    /// `changed`, the fields not named there are the same as in the last
    /// state, and what scripts already read of them is kept.
    pub fn set_state_source(&mut self, source: Rc<dyn StateSource>, changed: Option<&[&str]>) {
        let mut shared = self.shared.borrow_mut();
        let state = &mut shared.state;
        match changed {
            Some(changed) => {
                for name in changed {
                    state.fields.remove(*name);
                }
                if !changed.is_empty() {
                    state.whole = false;
                }
            }
            None => {
                state.fields.clear();
                state.whole = false;
            }
        }
        state.source = Some(source);
    }

    /// Run a keymap's function.
    pub fn press(&mut self, callback: u64) {
        self.call("keymap", callback, ());
    }

    /// Run a script's command with its words.
    pub fn run_command(&mut self, name: &str, args: &[String]) -> bool {
        let Some(c) = self.shared.borrow().commands.iter().find(|c| c.name == name).cloned() else { return false };
        let lua = &self.lua;
        let table = match lua.create_sequence_from(args.iter().cloned()) {
            Ok(t) => t,
            Err(_) => return true,
        };
        self.call(&format!(":{name}"), c.callback, table);
        true
    }

    /// The owner chose `index` of a script's picker, with Enter or, given
    /// `key`, one of its keys (no item when nothing can be selected).
    pub fn choose(&mut self, callback: u64, chosen: Option<(usize, &str)>, key: Option<&str>) {
        let (item, index) = chosen.map_or((None, None), |(i, item)| (Some(item.to_string()), Some(i + 1)));
        self.call("picker", callback, (item, index, key.map(str::to_string)));
        self.discard(callback);
    }

    /// The owner moved a picker to page `index` (from 0): tell its
    /// `on_tab`, which draws that page. The picker stays open.
    pub fn tab(&mut self, callback: u64, index: usize, label: &str) {
        let tabber = self.shared.borrow().tabbers.get(&callback).copied();
        if let Some(tabber) = tabber {
            self.call("picker tab", tabber, (label.to_string(), index + 1));
        }
    }

    /// A picker closed without a choice: tell its `on_close`, then drop
    /// its functions.
    pub fn forget(&mut self, callback: u64) {
        let closer = self.shared.borrow_mut().closers.remove(&callback);
        if let Some(closer) = closer {
            self.call("picker close", closer, ());
            self.shared.borrow_mut().callbacks.remove(&closer);
        }
        let mut shared = self.shared.borrow_mut();
        if let Some(tabber) = shared.tabbers.remove(&callback) {
            shared.callbacks.remove(&tabber);
        }
        shared.callbacks.remove(&callback);
    }

    /// Another picker replaced this one, or it was chosen: drop its
    /// functions without calling them.
    pub fn discard(&mut self, callback: u64) {
        let mut shared = self.shared.borrow_mut();
        if let Some(closer) = shared.closers.remove(&callback) {
            shared.callbacks.remove(&closer);
        }
        if let Some(tabber) = shared.tabbers.remove(&callback) {
            shared.callbacks.remove(&tabber);
        }
        shared.callbacks.remove(&callback);
    }

    /// Tell every handler of `event`.
    pub fn emit(&mut self, event: &str, payload: Value) {
        let ids: Vec<u64> =
            self.shared.borrow().handlers.iter().filter(|(e, _)| e == event).map(|(_, id)| *id).collect();
        for id in ids {
            self.call(&format!("on {event}"), id, payload.clone());
        }
    }

    /// Tell every handler of `event`, with a payload given as JSON.
    pub fn emit_json(&mut self, event: &str, payload: &serde_json::Value) {
        if !self.shared.borrow().handlers.iter().any(|(e, _)| e == event) {
            return;
        }
        if let Ok(value) = to_lua(&self.lua, payload) {
            self.emit(event, value);
        }
    }

    /// When the next timer, or a watched change, is due.
    pub fn next_due(&self) -> Option<Instant> {
        let s = self.shared.borrow();
        s.timers.values().map(|t| t.due).chain(s.watches.values().filter_map(files::Watch::next_due)).min()
    }

    /// Run the timers that are due and the watches whose changes settled;
    /// whether any ran.
    pub fn tick(&mut self, now: Instant) -> bool {
        let settled: Vec<u64> =
            self.shared.borrow_mut().watches.iter_mut().filter_map(|(id, w)| w.due(now).then_some(*id)).collect();
        for id in &settled {
            self.call("watch", *id, ());
        }
        self.run_timers(now) || !settled.is_empty()
    }

    fn run_timers(&mut self, now: Instant) -> bool {
        let due: Vec<u64> =
            self.shared.borrow().timers.iter().filter(|(_, t)| t.due <= now).map(|(id, _)| *id).collect();
        for id in &due {
            let callback = {
                let mut s = self.shared.borrow_mut();
                let Some(timer) = s.timers.get_mut(id) else { continue };
                match timer.every {
                    Some(every) => timer.due = now + every,
                    None => {
                        s.timers.remove(id);
                    }
                }
                *id
            };
            let repeat = self.shared.borrow().timers.contains_key(id);
            self.call("timer", callback, ());
            if !repeat {
                self.shared.borrow_mut().callbacks.remove(id);
            }
        }
        !due.is_empty()
    }

    /// A job finished: run its function with what it printed.
    pub fn job_done(&mut self, done: JobDone) {
        let Some(callback) = self.shared.borrow_mut().jobs.remove(&done.id) else { return };
        let result = (|| -> mlua::Result<Table> {
            let t = self.lua.create_table()?;
            t.set("code", done.code)?;
            t.set("ok", done.code == Some(0))?;
            t.set("stdout", done.stdout.as_str())?;
            t.set("stderr", done.stderr.as_str())?;
            t.set("timed_out", done.timed_out)?;
            Ok(t)
        })();
        if let Ok(t) = result {
            self.call("job", callback, t);
        }
        self.shared.borrow_mut().callbacks.remove(&callback);
    }

    /// The sections, their lines read now.
    pub fn sections(&mut self) -> Vec<Section> {
        // A copy: a section's function may add or remove sections.
        let defs: Vec<SectionDef> = self.shared.borrow().sections.clone();
        let mut out = Vec::new();
        for SectionDef { id, title, lines, fixed, phone, sidebar, pinned, header } in defs {
            let lines = match lines {
                Some(callback) => self.call_into(&format!("section {id}"), callback, (), lines_of).unwrap_or_default(),
                None => fixed,
            };
            out.push(Section { id, title, lines, phone, sidebar, pinned, header });
        }
        out
    }

    /// Whether any script shows session badges.
    pub fn has_badges(&self) -> bool {
        !self.shared.borrow().badges.is_empty()
    }

    /// Each session's badges, from every badge function, by session id.
    /// `sessions` are the sessions as `deck.state()` lists them.
    pub fn badges(&mut self, sessions: &[serde_json::Value]) -> HashMap<u64, Styled> {
        let ids: Vec<u64> = self.shared.borrow().badges.clone();
        let mut out: HashMap<u64, Styled> = HashMap::new();
        if ids.is_empty() {
            return out;
        }
        for session in sessions {
            let Some(id) = session.get("id").and_then(serde_json::Value::as_u64) else { continue };
            let Ok(value) = to_lua(&self.lua, session) else { continue };
            for badge in &ids {
                match self.call_into("badge", *badge, value.clone(), styled) {
                    Some(parts) if !parts.is_empty() => out.entry(id).or_default().extend(parts),
                    _ => {}
                }
            }
        }
        out
    }
}

/// `cepheus lua check` loads the scripts as the deck would and says
/// what failed; `cepheus lua example` prints an example init.lua.
pub fn run_cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("example") => {
            print!("{EXAMPLE_INIT}");
            0
        }
        None | Some("check") | Some("check-service") => {
            let start_dir = std::env::current_dir().unwrap_or_default();
            let places = Places { config_dir: crate::paths::config_dir(), start_dir };
            let config = crate::config::Config::load().0;
            let service_only = args.first().is_some_and(|s| s == "check-service");
            let mut engine = match if service_only {
                Engine::new_service(places, Arc::new(|_| {}))
            } else {
                Engine::new(places, Arc::new(|_| {}))
            } {
                Ok(engine) => engine,
                Err(e) => {
                    eprintln!("cepheus lua: Lua did not start: {e}");
                    return 1;
                }
            };
            engine.use_source_dir(config.plugin_source_dir.clone());
            if service_only {
                engine.load_service_with(&config.plugins);
            } else {
                engine.load_with(&config.plugins);
            }
            let errors = engine.take_errors();
            let loaded = if engine.loaded.is_empty() { "no script".to_string() } else { engine.loaded.join(", ") };
            println!("Loaded {loaded}.");
            let s = engine.shared.borrow();
            println!(
                "{} keymaps, {} commands, {} sections, {} badges, {} event handlers.",
                s.keymaps.len(),
                s.commands.len(),
                s.sections.len(),
                s.badges.len(),
                s.handlers.len()
            );
            for e in &errors {
                eprintln!("{e}");
            }
            i32::from(!errors.is_empty())
        }
        Some(other) => {
            eprintln!("cepheus lua: unknown command {other:?} (check, check-service, example)");
            2
        }
    }
}

// Provide Lua's `math.cosh` and `math.sinh` without the glibc versions introduced in 2.44, preserving
// compatibility with hosts using older glibc. Vendored Lua's 5.3 compatibility math calls these functions.
// Rust's `exp` and `exp_m1` retain their existing glibc imports.

/// Past this `exp(|x|)` overflows, though `cosh` and `sinh` do not yet.
const EXP_FINITE: f64 = 709.0;

/// The hyperbolic cosine, for the C library's callers (`math.cosh`).
#[no_mangle]
pub extern "C" fn cosh(x: f64) -> f64 {
    let a = x.abs();
    if a < EXP_FINITE {
        let e = a.exp();
        0.5 * e + 0.5 / e
    } else {
        // Halved first, so the square overflows only where cosh does.
        let h = (0.5 * a).exp();
        0.5 * h * h
    }
}

/// The hyperbolic sine, for the C library's callers (`math.sinh`).
#[no_mangle]
pub extern "C" fn sinh(x: f64) -> f64 {
    let a = x.abs();
    // Below 2⁻²⁸, x³/6 is under half a unit in the last place of x.
    if a < 1.0 / 268_435_456.0 {
        return x;
    }
    let v = if a < 1.0 {
        // From exp(a) − 1, so a small argument loses no digits.
        let m = a.exp_m1();
        0.5 * (m + m / (m + 1.0))
    } else if a < EXP_FINITE {
        let e = a.exp();
        0.5 * e - 0.5 / e
    } else {
        let h = (0.5 * a).exp();
        0.5 * h * h
    };
    v.copysign(x)
}

#[cfg(test)]
#[path = "tests/lua.rs"]
mod tests;

/// What the scripts are told about a change of state.
pub struct Change {
    pub event: &'static str,
    pub payload: serde_json::Value,
}

/// How the scripts last saw a session, for `changes`.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    status: Status,
    alive: bool,
    conversation: Option<String>,
}

/// How the scripts see `sessions` now, to compare the next state with.
pub fn seen(sessions: &[SessionInfo]) -> HashMap<u64, Seen> {
    sessions
        .iter()
        .map(|s| (s.id, Seen { status: s.status, alive: s.alive, conversation: s.conversation.clone() }))
        .collect()
}

/// The events between two states: sessions that started, ended, changed
/// status or were cleared, by how the scripts saw them before.
pub fn changes(before: &HashMap<u64, Seen>, sessions: &[SessionInfo]) -> Vec<Change> {
    let mut out = Vec::new();
    for session in sessions {
        let (id, status, alive) = (session.id, session.status, session.alive);
        let s = || serde_json::to_value(session).unwrap_or_default();
        match before.get(&id) {
            None => out.push(Change { event: "session_new", payload: s() }),
            Some(old) => {
                if old.alive && !alive {
                    out.push(Change { event: "session_end", payload: s() });
                }
                if old.status != status {
                    out.push(Change {
                        event: "status",
                        payload: serde_json::json!({ "session": s(), "from": old.status, "to": status }),
                    });
                    match status {
                        Status::Finished => out.push(Change { event: "finished", payload: s() }),
                        Status::NeedsYou => out.push(Change { event: "needs_you", payload: s() }),
                        _ => {}
                    }
                }
                // `/clear` or `/new`: a live session left its conversation for
                // an empty one. Codex names none until its first turn ends;
                // Claude names the new one at once, with nothing in it yet.
                if old.alive
                    && alive
                    && session.last_active == 0
                    && old.conversation.is_some()
                    && old.conversation != session.conversation
                {
                    out.push(Change { event: "cleared", payload: s() });
                }
            }
        }
    }
    out
}
