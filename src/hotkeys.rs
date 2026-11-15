//! The deck's hotkeys: the chords that run a deck command from the list and, most of them, while typing into a
//! session. Actions use `:` commands; the config file's `"keys"` table rebinds them and a running deck follows
//! changes to the file. The list keys (↑ ↓ j k, Enter, Tab, 1-9, PgUp, `:`) and the keys inside dialogs stay as
//! they are.

use std::collections::{BTreeMap, HashMap};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

use crate::config::Config;
use crate::paths;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hotkey {
    Close,
    New,
    Broadcast,
    Resume,
    MoveIn,
    KittyWindow,
    DevServer,
    Sidebar,
    Quit,
    Restart,
    Stop,
    Help,
    Errors,
    Pair,
    MoveUp,
    MoveDown,
    Window,
    Branches,
    MemoryRerun,
    Scratch,
    Command,
}

/// Every hotkey in the order the help sheet lists them: its name in the
/// config file, its default keys and what it does. `<name>` in a description
/// is that hotkey's key.
///
/// Actions have no default Alt shortcuts: they are `:` commands. Explicit
/// config bindings remain available; navigation and function keys keep theirs.
pub const HOTKEYS: &[(Hotkey, &str, &[&str], &str)] = &[
    (Hotkey::Close, "close", &[], "close selected session; :resume can reopen"),
    (Hotkey::New, "new", &[], "new session (Claude Code, Codex, Antigravity, Grok or opencode)"),
    (Hotkey::Broadcast, "broadcast", &[], "message selected live agents"),
    (Hotkey::Resume, "resume", &[], "resume an earlier conversation"),
    (Hotkey::MoveIn, "move_in", &[], "move an outside session in"),
    (Hotkey::KittyWindow, "kitty_window", &[], "focus its local terminal window"),
    (Hotkey::DevServer, "dev_server", &[], "start or stop the dev server"),
    (Hotkey::Sidebar, "sidebar", &[], "show or hide the sidebar"),
    (Hotkey::Quit, "quit", &[], "quit view; keep sessions running"),
    (Hotkey::Restart, "restart", &[], "restart service; keep sessions running"),
    (Hotkey::Stop, "stop", &[], "stop service only when no sessions are live"),
    (Hotkey::Help, "help", &[], "keys and commands"),
    (Hotkey::Errors, "errors", &["f2"], "errors"),
    (Hotkey::Branches, "branches", &["f3"], "branches and agents on every host"),
    (Hotkey::MemoryRerun, "memory_rerun", &["f6"], "rerun selected session verification once memory is free"),
    (Hotkey::Scratch, "scratch", &[], "scratch terminal; again goes back"),
    (Hotkey::Command, "command", &[], "enter a deck command"),
    (Hotkey::Pair, "pair", &[], "pair the phone app"),
    (Hotkey::MoveUp, "move_up", &["super+up"], "move the session up the list"),
    (Hotkey::MoveDown, "move_down", &["super+down"], "move the session down the list"),
    (Hotkey::Window, "window", &["ctrl+w"], "then w or h goes to the list and l to the session"),
];

impl Hotkey {
    /// The same action through the command line (empty: enter `:` itself).
    pub fn command(self) -> Option<&'static str> {
        Some(match self {
            Self::Close => "close",
            Self::New => "new",
            Self::Broadcast => "broadcast",
            Self::Resume => "resume",
            Self::MoveIn => "movein",
            Self::KittyWindow => "window",
            Self::DevServer => "dev",
            Self::Sidebar => "sidebar",
            Self::Quit => "q",
            Self::Restart => "restart",
            Self::Stop => "stop",
            Self::Help => "help",
            Self::Errors => "errors",
            Self::Pair => "pair",
            Self::Branches => "branches",
            Self::MemoryRerun => "rerun",
            Self::Scratch => "scratch",
            Self::Command => "",
            Self::MoveUp | Self::MoveDown | Self::Window => return None,
        })
    }

    fn entry(self) -> &'static (Hotkey, &'static str, &'static [&'static str], &'static str) {
        HOTKEYS.iter().find(|e| e.0 == self).expect("every hotkey is in HOTKEYS")
    }

    pub fn name(self) -> &'static str {
        self.entry().1
    }

    pub fn about(self) -> &'static str {
        self.entry().3
    }

    pub fn named(name: &str) -> Option<Hotkey> {
        HOTKEYS.iter().find(|e| e.1 == name).map(|e| e.0)
    }

    /// Keys for an outside row, F2 and pairing: while typing into a session they are the program's. Claude Code
    /// switches models with Alt+p.
    pub fn typed_through(self) -> bool {
        matches!(self, Hotkey::MoveIn | Hotkey::KittyWindow | Hotkey::Errors | Hotkey::Pair)
    }
}

/// A hotkey as the config file writes it: one key or a list of keys.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum KeySpec {
    One(String),
    Many(Vec<String>),
}

impl KeySpec {
    fn keys(&self) -> Vec<&str> {
        match self {
            KeySpec::One(key) => vec![key.as_str()],
            KeySpec::Many(keys) => keys.iter().map(String::as_str).collect(),
        }
    }
}

const MODIFIERS: KeyModifiers =
    KeyModifiers::CONTROL.union(KeyModifiers::ALT).union(KeyModifiers::SHIFT).union(KeyModifiers::SUPER);

/// One key with its modifiers, Shift folded into the character it types:
/// kitty reports Alt+Shift+q as `q` with Shift and Alt+? as `/` with Shift,
/// other terminals send `Q` and `?`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

/// The character Shift types on a US keyboard.
fn shifted(c: char) -> char {
    const PAIRS: &str = "1!2@3#4$5%6^7&8*9(0)-_=+[{]}\\|;:'\",<.>/?`~";
    if c.is_ascii_lowercase() {
        return c.to_ascii_uppercase();
    }
    let chars: Vec<char> = PAIRS.chars().collect();
    chars.chunks(2).find(|pair| pair[0] == c).map_or(c, |pair| pair[1])
}

impl Chord {
    pub fn of(k: &KeyEvent) -> Chord {
        let mut mods = k.modifiers & MODIFIERS;
        let code = match k.code {
            KeyCode::Char(c) => {
                let c = if mods.contains(KeyModifiers::SHIFT) { shifted(c) } else { c };
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::Char(c)
            }
            KeyCode::Tab | KeyCode::BackTab if mods.contains(KeyModifiers::SHIFT) || k.code == KeyCode::BackTab => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            code => code,
        };
        Chord { code, mods }
    }

    /// "alt+x", "alt+shift+q" or "alt+Q", "super+up", "f2", "ctrl+w".
    pub fn parse(spec: &str) -> Result<Chord, String> {
        let text = spec.trim();
        let (mods_part, key) = if text == "+" {
            ("", "+")
        } else if let Some(mods) = text.strip_suffix("++") {
            (mods, "+")
        } else {
            text.rsplit_once('+').unwrap_or(("", text))
        };
        let mut mods = KeyModifiers::NONE;
        for m in mods_part.split('+').filter(|m| !m.is_empty()) {
            mods |= match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "option" | "meta" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "super" | "win" | "cmd" | "logo" => KeyModifiers::SUPER,
                _ => return Err(format!("{spec:?}: {m:?} is not ctrl, alt, shift or super")),
            };
        }
        let mut chars = key.chars();
        let code = match (chars.next(), chars.next()) {
            (Some(c), None) => KeyCode::Char(c),
            _ => match key.to_ascii_lowercase().as_str() {
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                "enter" | "return" => KeyCode::Enter,
                "esc" | "escape" => KeyCode::Esc,
                "tab" => KeyCode::Tab,
                "backtab" => KeyCode::BackTab,
                "backspace" => KeyCode::Backspace,
                "delete" | "del" => KeyCode::Delete,
                "insert" | "ins" => KeyCode::Insert,
                "home" => KeyCode::Home,
                "end" => KeyCode::End,
                "pageup" | "pgup" => KeyCode::PageUp,
                "pagedown" | "pgdn" => KeyCode::PageDown,
                "space" => KeyCode::Char(' '),
                name => match name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                    Some(n @ 1..=24) => KeyCode::F(n),
                    _ => return Err(format!("{spec:?}: {key:?} is not a key")),
                },
            },
        };
        Ok(Chord::of(&KeyEvent::new(code, mods)))
    }

    /// A character typed with no Ctrl, Alt or Super: it stays the
    /// program's while typing and the command line's after `:`.
    pub fn types_text(&self) -> bool {
        matches!(self.code, KeyCode::Char(_))
            && !self.mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    }

    fn mod_names(&self, names: [&'static str; 3]) -> Vec<&'static str> {
        [KeyModifiers::CONTROL, KeyModifiers::ALT, KeyModifiers::SUPER]
            .iter()
            .zip(names)
            .filter(|(m, _)| self.mods.contains(**m))
            .map(|(_, name)| name)
            .collect()
    }

    /// As the config file writes it: "alt+Q", "super+up", "f2".
    pub fn spec(&self) -> String {
        let key = match self.code {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::F(n) => format!("f{n}"),
            KeyCode::BackTab => "shift+tab".to_string(),
            KeyCode::PageUp => "pageup".to_string(),
            KeyCode::PageDown => "pagedown".to_string(),
            code => format!("{code:?}").to_ascii_lowercase(),
        };
        let mut parts = self.mod_names(["ctrl", "alt", "super"]);
        parts.push(&key);
        parts.join("+")
    }

    /// As the deck shows it: "Alt+x", "Super+↑", "F2", and "Ctrl-w" in
    /// Neovim's spelling.
    pub fn label(&self) -> String {
        let key = match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Up => "↑".into(),
            KeyCode::Down => "↓".into(),
            KeyCode::Left => "←".into(),
            KeyCode::Right => "→".into(),
            KeyCode::PageUp => "PgUp".into(),
            KeyCode::PageDown => "PgDn".into(),
            KeyCode::Delete => "Del".into(),
            KeyCode::Insert => "Ins".into(),
            KeyCode::BackTab => "Shift+Tab".into(),
            KeyCode::F(n) => format!("F{n}"),
            code => format!("{code:?}"),
        };
        if self.mods == KeyModifiers::CONTROL && matches!(self.code, KeyCode::Char(_)) {
            return format!("Ctrl-{key}");
        }
        let mut parts = self.mod_names(["Ctrl", "Alt", "Super"]);
        parts.push(&key);
        parts.join("+")
    }
}

/// Each hotkey's keys, the config file's over the defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct Keymap {
    keys: Vec<(Hotkey, Vec<Chord>)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::from_config(&BTreeMap::new()).0
    }
}

impl Keymap {
    /// The keymap `config` asks for, and what it got wrong. A hotkey whose
    /// keys do not parse keeps its default; an empty list leaves it without
    /// a key. A key the config gives one hotkey is taken from the hotkey it
    /// was the default of; given to two, the first in `HOTKEYS` keeps it.
    pub fn from_config(config: &BTreeMap<String, KeySpec>) -> (Keymap, Vec<String>) {
        let mut errors = Vec::new();
        let mut chosen: HashMap<Hotkey, Vec<Chord>> = HashMap::new();
        for (name, spec) in config {
            let Some(hotkey) = Hotkey::named(name) else {
                errors.push(format!("keys: there is no hotkey {name:?}; `cepheus keys` lists them"));
                continue;
            };
            let parsed: Result<Vec<Chord>, String> = spec.keys().into_iter().map(Chord::parse).collect();
            match parsed {
                Ok(mut chords) => {
                    chords.dedup();
                    chosen.insert(hotkey, chords);
                }
                Err(err) => errors.push(format!("keys.{name}: {err}; it keeps its default")),
            }
        }
        let mut owner: HashMap<Chord, Hotkey> = HashMap::new();
        let mut keys: Vec<(Hotkey, Vec<Chord>, bool)> = HOTKEYS
            .iter()
            .map(|(hotkey, _, defaults, _)| match chosen.remove(hotkey) {
                Some(chords) => (*hotkey, chords, true),
                None => {
                    (*hotkey, defaults.iter().map(|d| Chord::parse(d).expect("default keys parse")).collect(), false)
                }
            })
            .collect();
        for (hotkey, chords, _) in keys.iter_mut().filter(|k| k.2) {
            chords.retain(|chord| match owner.get(chord) {
                Some(first) => {
                    errors.push(format!(
                        "keys: {} is set for both {} and {}; {} keeps it",
                        chord.spec(),
                        first.name(),
                        hotkey.name(),
                        first.name()
                    ));
                    false
                }
                None => {
                    owner.insert(*chord, *hotkey);
                    true
                }
            });
        }
        for (_, chords, _) in keys.iter_mut().filter(|k| !k.2) {
            chords.retain(|chord| !owner.contains_key(chord));
        }
        (Keymap { keys: keys.into_iter().map(|(hotkey, chords, _)| (hotkey, chords)).collect() }, errors)
    }

    /// The hotkey `k` is bound to.
    pub fn find(&self, k: &KeyEvent) -> Option<Hotkey> {
        let chord = Chord::of(k);
        self.keys.iter().find(|(_, chords)| chords.contains(&chord)).map(|(hotkey, _)| *hotkey)
    }

    pub fn chords(&self, hotkey: Hotkey) -> &[Chord] {
        self.keys.iter().find(|(h, _)| *h == hotkey).map_or(&[], |(_, chords)| chords.as_slice())
    }

    /// The first key of `hotkey` as the deck shows it, if it has one.
    pub fn label(&self, hotkey: Hotkey) -> Option<String> {
        self.chords(hotkey).first().map(Chord::label)
    }

    /// `text` with each `<name>` replaced by that hotkey's key. A part
    /// between " · " that names a hotkey without a key is left out.
    pub fn fill(&self, text: &str) -> String {
        text.split(" · ").filter_map(|part| self.fill_part(part)).collect::<Vec<_>>().join(" · ")
    }

    fn fill_part(&self, part: &str) -> Option<String> {
        let mut out = String::new();
        let mut rest = part;
        while let Some(open) = rest.find('<') {
            let Some(len) = rest[open + 1..].find('>') else { break };
            match Hotkey::named(&rest[open + 1..open + 1 + len]) {
                Some(hotkey) => {
                    out.push_str(&rest[..open]);
                    out.push_str(&self.label(hotkey)?);
                    rest = &rest[open + len + 2..];
                }
                None => {
                    out.push_str(&rest[..=open]);
                    rest = &rest[open + 1..];
                }
            }
        }
        out.push_str(rest);
        Some(out)
    }
}

/// `cepheus keys`: every hotkey, its keys and where to change them.
pub fn print() -> i32 {
    let (cfg, err) = Config::load();
    if let Some(err) = err {
        eprintln!("cepheus: config: {err}");
    }
    let (keymap, errors) = Keymap::from_config(&cfg.keys);
    for err in &errors {
        eprintln!("cepheus: {err}");
    }
    print!("{}", listing(&keymap, &paths::config_path().display().to_string()));
    i32::from(!errors.is_empty())
}

pub fn listing(keymap: &Keymap, config_file: &str) -> String {
    let mut out =
        format!("Hotkeys. Change them under \"keys\" in {config_file};\na running deck picks the change up.\n\n");
    for (hotkey, name, _, _) in HOTKEYS {
        let keys: Vec<String> = keymap.chords(*hotkey).iter().map(Chord::spec).collect();
        let keys = if keys.is_empty() { "(none)".to_string() } else { keys.join(", ") };
        out.push_str(&format!("  {name:<13} {keys:<16} {}\n", keymap.fill(hotkey.about())));
    }
    out.push_str(
        "\nFor example: {\"keys\": {\"errors\": \"f3\", \"move_up\": [\"super+up\", \"alt+k\"], \"stop\": []}}\n\
         A key is ctrl, alt, shift or super with a character, up, down, left, right, enter,\n\
         esc, tab, backspace, delete, insert, home, end, pageup, pagedown, space or f1-f24.\n\
         An empty list leaves a hotkey without a key. Move in, kitty window and errors\n\
         go to the program while you type into a session, as does a plain character.\n",
    );
    out
}

#[cfg(test)]
#[path = "tests/hotkeys.rs"]
mod tests;
