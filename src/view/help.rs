//! `:help`: every key and command, grouped, each key in one column beside what it does. The windows list no
//! keys along their edges ("we dont need help option menu rows on the bottom of any of them"), so the keys they
//! take are here, under "In windows".

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::overlays::{INSET, READING};
use super::panel::{line_width, note, search_rows, section, show, text_width, Panel, GAP};
use super::{home_path, hotkey_rows, plain, HELP};
use crate::actions::COMMANDS;
use crate::hotkeys::{Hotkey, HOTKEYS};
use crate::theme::Theme;
use crate::tui::{App, DialogViewport};

/// A key wider than this has a row of its own, its text beneath.
const KEY_COLUMN: usize = 18;
/// Text narrower than this beside the keys goes beneath them instead.
const NARROW_TEXT: usize = 24;

/// The keys every window takes.
const WINDOW_KEYS: &[(&str, &str)] = &[
    ("↑ ↓  j k", "move the selection, or scroll the text"),
    ("PgUp PgDn", "a page at a time; Home and End go to the first and last row"),
    ("Enter", "open or run the selected row"),
    ("/", "search the window; Esc clears the search"),
    ("← →  Tab", "the window's other pages, where it has them"),
    ("Esc  q", "close the window"),
    ("y  n", "do it, or cancel, where a window asks first"),
];

/// Keys only one window takes: the window, then each key and what it does.
const WINDOW_OWN_KEYS: &[(&str, &[(&str, &str)])] = &[
    ("New session", &[("← →", "host"), ("m", "model"), ("e", "effort")]),
    ("Errors", &[("c", "clears them")]),
    ("Branches", &[("Ctrl-n", "Claude"), ("Ctrl-x", "Codex"), ("Ctrl-t", "a terminal, on the branch")]),
    (
        "Message sessions",
        &[
            ("Enter", "sends"),
            ("Shift-Enter", "a new line"),
            ("Tab", "ticks a session"),
            ("Ctrl-a", "ticks or clears all"),
        ],
    ),
    ("Idle message", &[("Enter", "submits it once"), ("Esc", "closes, keeping the draft")]),
    ("Scans", &[("s", "starts a scan")]),
    ("Pairing", &[("n", "a new code"), ("d", "forgets the selected phone")]),
    ("Queue", &[("a", "adds"), ("c", "cancels the selected item"), ("r", "refreshes")]),
    ("Attention", &[("Enter", "fetches the private text"), ("c", "cancels a queued message"), ("r", "refreshes")]),
];

/// One line of the sheet.
enum Entry {
    Section(String),
    /// A key, what it does, and a dim line under it (a command's
    /// arguments).
    Row {
        key: String,
        does: Vec<(String, Style)>,
        more: String,
    },
    Note(String),
}

impl Entry {
    fn row(key: impl Into<String>, does: &str) -> Entry {
        Entry::Row { key: key.into(), does: vec![(does.to_string(), plain())], more: String::new() }
    }

    fn text(&self) -> String {
        match self {
            Entry::Section(name) | Entry::Note(name) => name.clone(),
            Entry::Row { key, does, more } => {
                let does: Vec<&str> = does.iter().map(|(s, _)| s.as_str()).collect();
                format!("{key} {} {more}", does.join(" "))
            }
        }
    }
}

/// A help line written "KEY — what it does", or a sentence.
fn described(line: &str) -> Entry {
    match line.split_once(" — ") {
        Some((key, does)) => Entry::row(key.trim(), does.trim()),
        None if line.starts_with(':') => Entry::row(line.trim(), ""),
        None => Entry::Note(line.to_string()),
    }
}

fn entries(app: &App, t: &Theme) -> Vec<Entry> {
    let keys = &app.keys;
    let mut out = vec![Entry::Section("Sessions".into())];
    for (key, does) in HELP {
        let key = keys.fill(key);
        if !key.is_empty() {
            out.push(Entry::row(key, &keys.fill(does)));
        }
    }
    out.push(Entry::Section("Hotkeys".into()));
    out.extend(hotkey_rows(app).into_iter().map(|(key, does)| Entry::row(key, &does)));
    // While typing, these reach the program; the rest work in both modes.
    let mut through: Vec<String> =
        HOTKEYS.iter().filter(|e| e.0.typed_through()).filter_map(|e| keys.label(e.0)).collect();
    through.extend(keys.label(Hotkey::Window).map(|k| format!("{k} then any other key")));
    if let Some((last, rest)) = through.split_last() {
        let list = if rest.is_empty() { last.clone() } else { format!("{} and {last}", rest.join(", ")) };
        out.push(Entry::Note(format!("While typing, {list} go to the program.")));
    }
    out.push(Entry::Note(format!(
        "Change them under \"keys\" in {}; cepheus keys lists them.",
        home_path(&crate::paths::config_path().display().to_string(), std::env::var("HOME").ok().as_deref())
    )));
    out.push(Entry::Section("In windows".into()));
    out.extend(WINDOW_KEYS.iter().map(|(key, does)| Entry::row(*key, does)));
    for (window, own) in WINDOW_OWN_KEYS {
        let mut does = Vec::new();
        for (i, (key, what)) in own.iter().enumerate() {
            let sep = if i + 1 < own.len() { " ·" } else { "" };
            does.push((key.to_string(), t.bold().fg(t.accent)));
            does.push((format!("{what}{sep}"), plain()));
        }
        out.push(Entry::Row { key: window.to_string(), does, more: String::new() });
    }
    out.push(Entry::Section("Jobs".into()));
    out.extend(crate::tui::job_sessions::help_lines().iter().map(|line| described(line)));
    out.push(Entry::Section("Attention".into()));
    out.extend(crate::tui::attention_commands::help_lines().iter().map(|line| described(line)));
    out.push(Entry::Section("Commands".into()));
    for spec in COMMANDS {
        let args = spec.usage.strip_prefix(spec.name).unwrap_or(spec.usage).trim();
        let mut more = args.to_string();
        if !spec.aliases.is_empty() {
            let also = format!("also :{}", spec.aliases.join(" :"));
            more = if more.is_empty() { also } else { format!("{more}  ·  {also}") };
        }
        out.push(Entry::Row { key: format!(":{}", spec.name), does: vec![(spec.about.to_string(), plain())], more });
    }
    if !app.user_commands.is_empty() {
        out.push(Entry::Section("From scripts".into()));
        out.extend(app.user_commands.iter().map(|c| Entry::row(format!(":{}", c.name), &c.desc)));
    }
    if !app.lua_keys.is_empty() {
        out.push(Entry::Section("Keys from scripts".into()));
        for k in &app.lua_keys {
            let action = match &k.action {
                crate::lua::KeyAction::Command(line) => format!(":{line}"),
                crate::lua::KeyAction::Lua(_) => "a function".to_string(),
            };
            let (does, more) = if k.desc.is_empty() { (action, String::new()) } else { (k.desc.clone(), action) };
            out.push(Entry::Row { key: k.key.clone(), does: vec![(does, plain())], more });
        }
    }
    out
}

/// Styled words wrapped into lines `width` cells wide.
fn wrap_spans(parts: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for (text, style) in parts {
        for word in text.split_whitespace() {
            let word = crate::util::truncate_width(word, width);
            let w = word.width();
            if used > 0 && used + 1 + w > width {
                lines.push(Vec::new());
                used = 0;
            }
            let line = lines.last_mut().expect("one line at least");
            if used > 0 {
                line.push(Span::raw(" "));
                used += 1;
            }
            line.push(Span::styled(word, *style));
            used += w;
        }
    }
    lines
}

fn rows(entry: &Entry, key_column: usize, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let lead = || Span::raw(" ".repeat(INSET));
    match entry {
        Entry::Section(name) => vec![section(vec![Span::raw(name.clone())], Vec::new(), width, t)],
        Entry::Note(text) => note(text, width, t),
        Entry::Row { key, does, more } => {
            let room = text_width(width);
            // A narrow window has no room to read beside the keys: each key
            // then has its own row, its text indented beneath.
            let stacked = room < key_column + GAP + NARROW_TEXT;
            let hang = if stacked { 2 } else { key_column + GAP };
            let does_room = room.saturating_sub(hang).max(1);
            let mut text = wrap_spans(does, does_room);
            if !more.is_empty() {
                text.extend(wrap_spans(&[(more.clone(), t.dim())], does_room));
            }
            text.retain(|line| !line.is_empty());
            let indent = || Span::raw(" ".repeat(INSET + hang));
            let mut out = Vec::new();
            if stacked || key.width() > key_column {
                // A long command form has its own row.
                for part in wrap_spans(&[(key.clone(), t.bold().fg(t.accent))], room) {
                    out.push(Line::from([vec![lead()], part].concat()));
                }
                out.extend(text.into_iter().map(|line| Line::from([vec![indent()], line].concat())));
                return out;
            }
            let mut text = text.into_iter();
            let mut first = vec![lead(), Span::styled(key.clone(), t.bold().fg(t.accent))];
            first.push(Span::raw(" ".repeat(hang - key.width())));
            first.extend(text.next().unwrap_or_default());
            out.push(Line::from(first));
            out.extend(text.map(|line| Line::from([vec![indent()], line].concat())));
            out
        }
    }
}

/// The help sheet.
pub(super) fn help_view(f: &mut Frame, area: Rect, app: &App, t: &Theme, viewport: &mut DialogViewport) {
    let all = entries(app, t);
    let key_column = all
        .iter()
        .filter_map(|e| match e {
            Entry::Row { key, .. } if key.width() <= KEY_COLUMN => Some(key.width()),
            _ => None,
        })
        .max()
        .unwrap_or(8);
    let natural = all
        .iter()
        .map(|e| match e {
            Entry::Row { key, does, more } => {
                let does: usize = does.iter().map(|(s, _)| s.width() + 1).sum();
                key_column + GAP + does.max(more.width()).max(key.width().saturating_sub(key_column + GAP))
            }
            _ => 0,
        })
        .max()
        .unwrap_or(0)
        .min(READING);
    let commands = COMMANDS.len() + app.user_commands.len();
    let mut panel = Panel::new("Help");
    panel.summary = vec![Span::raw(format!("{commands} commands"))];
    let width = line_width(area, natural.max(panel.chrome_width()));
    // Search keeps the rows that hold every word, under their headings.
    let words: Vec<String> = viewport.search.iter().flat_map(|q| q.split_whitespace().map(str::to_lowercase)).collect();
    let mut kept: Vec<&Entry> = Vec::new();
    let mut heading: Option<&Entry> = None;
    let (mut found, mut total) = (0, 0);
    for entry in &all {
        if let Entry::Section(_) = entry {
            heading = Some(entry);
            continue;
        }
        total += 1;
        let text = entry.text().to_lowercase();
        if !words.iter().all(|w| text.contains(w)) {
            continue;
        }
        found += 1;
        if let Some(h) = heading.take() {
            kept.push(h);
        }
        kept.push(entry);
    }
    let mut body: Vec<Line<'static>> = Vec::new();
    for entry in kept {
        if matches!(entry, Entry::Section(_)) && !body.is_empty() {
            body.push(Line::raw(""));
        }
        body.extend(rows(entry, key_column, width, t));
    }
    if let Some(query) = &viewport.search {
        panel.top = search_rows(query, &format!("{found} of {total}"), width, t);
        if body.is_empty() {
            body.extend(note("Nothing matches. Backspace widens the search.", width, t));
        }
    }
    panel.body = body;
    show(f, area, width, panel, viewport, t);
}
