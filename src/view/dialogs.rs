//! The windows the deck's own keys open: a new session, the conversations
//! to resume, one message for many sessions, phone pairing and the question
//! asked before the service stops, each on the shared window (`panel`).

use std::collections::HashSet;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::overlays::{buttons, row, text_rows, width_of, INSET, READING};
use super::panel::{
    body_room, head_rows, input_box, line_width, note, search_rows, section, show, text_width, Cell, Column, Panel,
    Tab, Table,
};
use super::{fit, home_path, keep_tail, plain, spinner, status_glyph};
use crate::pairing::{self, Pairing};
use crate::protocol::{Conversation, Tool};
use crate::theme::Theme;
use crate::tui::{effort_choices, model_choices, new_group, new_key, new_tools, Action, App, DialogViewport};
use crate::util::{self, short_duration};

/// "1 conversation", "3 conversations".
fn counted(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

fn program_label(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "Claude Code",
        Tool::Antigravity => "Antigravity (agy)",
        other => other.label(),
    }
}

/// The model and effort the highlighted program starts with, and the key
/// that changes each: two rows whether it offers them or not, so the
/// window keeps its size.
fn model_rows(app: &App, tool: Tool, model: usize, effort: usize, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let line = |name: &str, value: Span<'static>, key: &str| {
        let key = if key.is_empty() { Vec::new() } else { vec![Span::styled(key.to_string(), t.bold().fg(t.accent))] };
        row(vec![Span::styled(format!("{name:<8}"), t.dim()), value], key, width, false, t)
    };
    let Some(choices) = model_choices(&app.new_models, tool) else {
        return vec![line("Model", Span::styled("its own default", t.dim()), ""), Line::raw("")];
    };
    let default = |known: Option<&String>| {
        Span::styled(known.map_or("default".to_string(), |d| format!("default ({d})")), t.dim())
    };
    let model_value = match model.checked_sub(1).and_then(|i| choices.models.get(i)) {
        Some(m) => Span::styled(m.label.clone(), t.bold()),
        None => default(choices.default_model.as_ref()),
    };
    let efforts = effort_choices(choices, model);
    let effort_value = match effort.checked_sub(1).and_then(|i| efforts.get(i)) {
        Some(e) => Span::styled(e.clone(), t.bold()),
        None if efforts.is_empty() => Span::styled("none to choose", t.dim()),
        None => default(choices.default_effort.as_ref()),
    };
    vec![line("Model", model_value, "m"), line("Effort", effort_value, "e")]
}

/// The session chooser: each program by its key, grouped; the host it
/// starts on as pages when there are several, and the model and effort.
#[allow(clippy::too_many_arguments)]
pub(super) fn new_view(
    f: &mut Frame,
    area: Rect,
    sel: usize,
    model: usize,
    effort: usize,
    app: &App,
    t: &Theme,
    viewport: &mut DialogViewport,
) {
    let tools = new_tools(&app.cfg);
    let mut panel = Panel::new("New session");
    let host = app.new_host.as_deref();
    // A host's start directory is its own; `~` would be this machine's home.
    let dir = app.start_dir_on(host);
    let dir = if host.is_none() { home_path(&dir, std::env::var("HOME").ok().as_deref()) } else { dir };
    panel.summary = vec![Span::raw(format!("in {}", keep_tail(&dir, 52)))];
    // Every host it can start on, ← → moving along them.
    let hosts = app.start_hosts();
    if hosts.len() > 1 {
        panel.tabs = hosts.iter().map(|h| Tab::new(app.host_label(h.as_deref()), "")).collect();
        panel.tab = hosts.iter().position(|h| h.as_deref() == host).unwrap_or(0);
    }
    if tools.is_empty() {
        let why = "No agent providers are enabled. Enable a provider plugin to start a session.";
        let width = line_width(area, panel.chrome_width().max(why.width()));
        panel.body = note(why, width, t);
        show(f, area, width, panel, viewport, t);
        return;
    }
    let restart = "needs the restarted service";
    let natural = tools
        .iter()
        .map(|&tool| {
            let note = if tool.since_version() > app.service_version { restart.width() + 2 } else { 0 };
            3 + program_label(tool).width() + note
        })
        .max()
        .unwrap_or(0)
        .max(36)
        .max(panel.chrome_width());
    let width = line_width(area, natural);
    let mut body = Vec::new();
    let mut selected_line = 0;
    for (i, &tool) in tools.iter().enumerate() {
        // The main programs, then Antigravity, then the rest under "Other".
        match new_group(tool) {
            Some("") => body.push(Line::raw("")),
            Some(heading) => {
                body.push(Line::raw(""));
                body.push(section(vec![Span::raw(heading.to_string())], Vec::new(), width, t));
            }
            None => {}
        }
        let selected = sel == i;
        if selected {
            selected_line = body.len();
        }
        let color = t.tool(tool);
        let key = new_key(tool);
        let left = vec![
            Span::styled(if key == '\0' { "   ".to_string() } else { format!("{key}  ") }, t.bold().fg(t.accent)),
            Span::styled(program_label(tool), if selected { t.bold().fg(color) } else { plain().fg(color) }),
        ];
        let right =
            if tool.since_version() > app.service_version { vec![Span::styled(restart, t.dim())] } else { Vec::new() };
        body.push(row(left, right, width, selected, t));
    }
    let tool = tools[sel.min(tools.len() - 1)];
    panel.bottom = vec![Line::raw("")];
    panel.bottom.extend(model_rows(app, tool, model, effort, width, t));
    panel.body = body;
    panel.follow = Some(selected_line);
    show(f, area, width, panel, viewport, t);
}

/// Before the service stops or restarts: only y does it, since it ends or
/// restarts every session and Enter is pressed without a thought.
pub(super) fn confirm_view(
    f: &mut Frame,
    area: Rect,
    text: &str,
    action: &Action,
    t: &Theme,
    viewport: &mut DialogViewport,
) {
    let (title, yes) = match action {
        Action::Shutdown => ("Stop the service", "Stop it"),
        Action::Restart => ("Restart the service", "Restart it"),
    };
    let mut panel = Panel::new(title);
    let natural = text.lines().map(UnicodeWidthStr::width).max().unwrap_or(0).min(64);
    let width = line_width(area, natural.max(panel.chrome_width()));
    for line in text.lines() {
        panel.body.extend(text_rows(line, plain(), width));
    }
    panel.bottom = vec![Line::raw(""), buttons(&[("n", "Keep it running"), ("y", yes)], t)];
    show(f, area, width, panel, viewport, t);
}

/// A conversation's cells: its program, how long ago it changed, its host
/// when there are several, and its title.
fn conversation_cells(c: &Conversation, hosts: bool, selected: bool, now: u64, app: &App, t: &Theme) -> Vec<Cell> {
    let mut cells = vec![
        vec![Span::styled(c.tool.label().to_string(), plain().fg(t.tool(c.tool)))],
        vec![Span::styled(short_duration(now.saturating_sub(c.updated_at)), t.dim())],
    ];
    if hosts {
        cells.push(vec![Span::styled(app.host_label(c.host.as_deref()), t.dim())]);
    }
    let mut title = vec![Span::styled(c.title.clone(), if selected { t.bold() } else { plain() })];
    if c.open_in.is_some() {
        title.push(Span::styled("  open", plain().fg(t.ok)));
    }
    cells.push(title);
    cells
}

/// The conversations to resume, newest first; `/` searches them.
#[allow(clippy::too_many_arguments)]
pub(super) fn resume_view(
    f: &mut Frame,
    area: Rect,
    items: &[Conversation],
    sel: usize,
    filter: &str,
    loading: bool,
    app: &App,
    t: &Theme,
    viewport: &mut DialogViewport,
) {
    let now = util::now_secs();
    let matches = App::picker_matches(items, filter);
    let mut panel = Panel::new("Resume a conversation");
    panel.summary = if loading {
        vec![Span::raw(format!("{} loading", spinner(app)))]
    } else {
        vec![Span::raw(counted(items.len(), "conversation"))]
    };
    let hosts = items.iter().map(|c| app.host_label(c.host.as_deref())).collect::<HashSet<_>>().len() > 1;
    let mut columns = vec![Column::left("Program"), Column::right("Age")];
    if hosts {
        columns.push(Column::left("Host"));
    }
    columns.push(Column::left("Conversation"));
    // Sized from every conversation, so search does not resize the window.
    let all: Vec<Vec<Cell>> = items.iter().map(|c| conversation_cells(c, hosts, false, now, app, t)).collect();
    let table = Table::measure(columns, all.iter().map(Vec::as_slice));
    let width = line_width(area, table.width().min(READING).max(panel.chrome_width()).max(56));
    let table = table.fit(width);
    if viewport.search.is_some() {
        panel.top.extend(search_rows(filter, &format!("{} of {}", matches.len(), items.len()), width, t));
    }
    if !matches.is_empty() {
        panel.top.push(table.header(width, t));
    }
    panel.body = matches
        .iter()
        .enumerate()
        .map(|(pos, &i)| table.row(conversation_cells(&items[i], hosts, pos == sel, now, app, t), width, pos == sel, t))
        .collect();
    if matches.is_empty() {
        let why = if loading {
            "Loading…"
        } else if items.is_empty() {
            "No conversations found."
        } else {
            "Nothing matches. Backspace widens the search."
        };
        panel.body = note(why, width, t);
    }
    panel.follow = Some(sel);
    show(f, area, width, panel, viewport, t);
}

/// One message for many sessions: the message being typed, then each live
/// session with a tick.
#[allow(clippy::too_many_arguments)]
pub(super) fn broadcast_view(
    f: &mut Frame,
    area: Rect,
    text: &str,
    targets: &[(u64, bool)],
    sel: usize,
    app: &App,
    t: &Theme,
    viewport: &mut DialogViewport,
) {
    const MAX_TEXT_LINES: usize = 6;
    let count = targets.iter().filter(|(_, on)| *on).count();
    let mut panel = Panel::new("Message sessions");
    panel.summary = vec![Span::raw(format!("to {}", counted(count, "session")))];
    let target_rows: Vec<(Vec<Span<'static>>, Vec<Span<'static>>)> = targets
        .iter()
        .filter_map(|(id, on)| {
            let s = app.session(*id)?;
            let (glyph, glyph_style) = status_glyph(s.status, app, t);
            let left = vec![
                Span::styled(if *on { "● " } else { "○ " }, if *on { t.accent() } else { t.faint() }),
                // Dot goes by its own name, with no number.
                Span::styled(format!("{:>2} ", app.position(*id).map_or(String::new(), |n| n.to_string())), t.dim()),
                Span::styled(glyph.to_string(), glyph_style),
                Span::styled(format!(" {:<7}", s.tool.label()), plain().fg(t.tool(s.tool))),
                Span::styled(s.title.clone(), if *on { plain() } else { t.dim() }),
            ];
            let right = if !s.takes_messages() {
                vec![Span::styled("waiting on a question · skipped", plain().fg(t.needs))]
            } else if !s.alive {
                vec![Span::styled("ended · skipped", t.dim())]
            } else {
                Vec::new()
            };
            Some((left, right))
        })
        .collect();
    let natural = target_rows
        .iter()
        .map(|(l, r)| width_of(l) + if r.is_empty() { 0 } else { width_of(r) + 2 })
        .max()
        .unwrap_or(0)
        .clamp(60, READING)
        .max(panel.chrome_width());
    let width = line_width(area, natural);
    // The message's last lines, a caret after them.
    let room = text_width(width).saturating_sub(4 + 2);
    let lines: Vec<&str> = text.split('\n').collect();
    let first = lines.len().saturating_sub(MAX_TEXT_LINES);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    for (i, line) in lines.iter().enumerate().skip(first) {
        let last = i + 1 == lines.len();
        let lead = Span::styled(if i == first { "› " } else { "  " }, t.bold().fg(t.accent));
        let mut spans = vec![lead, Span::styled(keep_tail(line, room.saturating_sub(usize::from(last))), t.bold())];
        if last {
            spans.push(Span::styled("▏", t.accent()));
        }
        rows.push(spans);
    }
    if text.is_empty() {
        rows = vec![vec![
            Span::styled("› ", t.bold().fg(t.accent)),
            Span::styled("▏", t.accent()),
            Span::styled("the message every ticked session gets", t.faint()),
        ]];
    }
    panel.top = input_box(rows.clone(), text_width(width), t);
    panel.top.push(Line::raw(""));
    panel.top.push(section(vec![Span::raw("To")], vec![Span::raw(format!("{count} of {}", targets.len()))], width, t));
    // A short screen keeps the sessions in view: the message's last line
    // alone, without its box.
    if body_room(area, head_rows(&panel) + 1) < 3 {
        let mut last = rows.pop().unwrap_or_default();
        if let Some(lead) = last.first_mut() {
            *lead = Span::styled("› ", t.bold().fg(t.accent));
        }
        panel.top = vec![Line::from([vec![Span::raw(" ".repeat(INSET))], last].concat())];
    }
    panel.body = target_rows
        .into_iter()
        .enumerate()
        .map(|(pos, (left, right))| row(left, right, width, pos == sel, t))
        .collect();
    panel.follow = Some(sel);
    show(f, area, width, panel, viewport, t);
}

/// The cells of the details beside the QR code.
const PAIR_TEXT: usize = 46;

/// :pair: the QR code, dark on light whatever the theme, beside the same
/// details as text and the paired phones.
pub(super) fn pair_view(f: &mut Frame, area: Rect, pairing: &Pairing, t: &Theme, viewport: &mut DialogViewport) {
    let now = util::now_secs();
    let live = pairing.offer.as_ref().ok().filter(|_| !pairing.expired(now));
    let mut qr = live.map(|o| pairing::half_blocks(&o.modules)).unwrap_or_default();
    let mut qr_width = qr.first().map_or(0, |r| r.chars().count());
    // The code may fill the screen above the footer, between the window's
    // borders: its title is then beside the code, not above it.
    let small = qr_width > 0
        && ((area.width as usize) < PAIR_TEXT + qr_width + 2 * INSET + 10 || (area.height as usize) < qr.len() + 3);
    if small {
        qr.clear();
        qr_width = 0;
    }
    let mut panel = Panel::new("Pair a phone");
    panel.summary = vec![Span::raw(format!("{} paired", pairing.devices.len()))];
    panel.tight = qr_width > 0;
    let mut info: Vec<Vec<Span<'static>>> = Vec::new();
    if panel.tight {
        info.push(Vec::new());
        info.push(vec![Span::styled(panel.title.clone(), t.bold().fg(t.accent))]);
        info.push(Vec::new());
    }
    let label = |name: &str| Span::styled(format!("{name:<9}"), t.dim());
    match (&pairing.offer, live) {
        (Ok(_), Some(o)) => {
            let scan = if small {
                "Make the window larger for the QR code, or type"
            } else {
                "Scan with the Cepheus app, or type"
            };
            for line in super::wrap_words(&format!("{scan} these into it:"), PAIR_TEXT) {
                info.push(vec![Span::raw(line)]);
            }
            info.push(Vec::new());
            info.push(vec![label("Address"), Span::styled(o.address.clone(), t.bold())]);
            let left = o.expires.saturating_sub(now);
            info.push(vec![
                label("Code"),
                Span::styled(o.code.clone(), t.bold().fg(t.accent)),
                Span::styled(format!("  {}:{:02} left", left / 60, left % 60), t.dim()),
            ]);
            info.push(vec![label("SHA-256")]);
            for (n, chunk) in o.fingerprint.as_bytes().chunks(16).enumerate() {
                let groups: Vec<&str> = chunk.chunks(4).map(|g| std::str::from_utf8(g).unwrap_or_default()).collect();
                let lead = if n == 0 { info.pop().unwrap_or_default() } else { vec![Span::raw(" ".repeat(9))] };
                info.push([lead, vec![Span::raw(groups.join(" "))]].concat());
            }
        }
        (Ok(_), None) => info.push(vec![Span::styled("The code expired; n makes a new one.", t.dim())]),
        (Err(why), _) => {
            for line in super::wrap_words(why, PAIR_TEXT) {
                info.push(vec![Span::styled(line, plain().fg(t.needs))]);
            }
        }
    }
    info.push(Vec::new());
    info.push(vec![
        Span::styled("PAIRED PHONES", t.bold()),
        Span::styled(format!("  {}", pairing.devices.len()), t.dim()),
    ]);
    if pairing.devices.is_empty() {
        info.push(vec![Span::styled("None yet.", t.dim())]);
    }
    let mut selected_row = None;
    for (i, d) in pairing.devices.iter().enumerate() {
        let seen = d
            .last_seen
            .map_or("never signed in".to_string(), |at| format!("seen {} ago", short_duration(now.saturating_sub(at))));
        let selected = i == pairing.sel;
        let mut spans = fit(
            vec![
                Span::styled(if selected { "▌ " } else { "  " }, t.accent()),
                Span::styled(d.name.clone(), if selected { t.bold() } else { plain() }),
                Span::styled(format!("  {seen}"), t.dim()),
            ],
            PAIR_TEXT,
        );
        if selected {
            selected_row = Some(info.len());
            let pad = PAIR_TEXT.saturating_sub(width_of(&spans));
            spans.push(Span::raw(" ".repeat(pad)));
            spans = spans.into_iter().map(|s| s.patch_style(Style::default().bg(t.selected_bg))).collect();
        }
        info.push(spans);
    }
    // The code beside the details, pure black on white rather than palette
    // colours a theme could swap.
    let qr_style = Style::default().fg(Color::Rgb(0, 0, 0)).bg(Color::Rgb(255, 255, 255));
    let gap = if qr_width > 0 { 3 } else { 0 };
    let rows = qr.len().max(info.len());
    let mut info = info.into_iter();
    let body: Vec<Line<'static>> = (0..rows)
        .map(|n| {
            let mut spans = vec![Span::raw(" ".repeat(INSET))];
            if qr_width > 0 {
                match qr.get(n) {
                    Some(line) => spans.push(Span::styled(line.clone(), qr_style)),
                    None => spans.push(Span::raw(" ".repeat(qr_width))),
                }
                spans.push(Span::raw(" ".repeat(gap)));
            }
            spans.extend(info.next().unwrap_or_default());
            Line::from(spans)
        })
        .collect();
    let width = line_width(area, (qr_width + gap + PAIR_TEXT).max(panel.chrome_width()));
    panel.body = body;
    panel.follow = selected_row;
    show(f, area, width, panel, viewport, t);
}
