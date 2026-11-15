//! Rendering for the TUI: sidebar, session pane, footer and dialogs.
//!
//! Every colour comes from `app.theme` (derived from the outer terminal's
//! palette) or is the terminal default, so the chrome follows the kitty theme;
//! hosted screens keep their own colours via `screen::render_frame`.

use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::gitwt::BranchStatus;
use crate::hotkeys::{Hotkey, HOTKEYS};
use crate::kitty;
use crate::lua::HeaderPlace;
use crate::pet::Mood;
use crate::protocol::{HostInfo, HostStats, LogEntry, SessionInfo, Status, Tool, Usage, UsageWindow};
use crate::resources::{self, Resources};
use crate::screen;
use crate::theme::Theme;
use crate::tui::dot_chat::AVATAR_COLUMNS;
use crate::tui::{App, Dialog, DialogViewport, Mode, Sel};
use crate::util::{self, short_duration, truncate_width};

mod attention;
mod dialogs;
mod dot_chat;
mod help;
mod job_sessions;
mod markdown;
mod overlays;
mod panel;
mod usage;
mod work_queue;

pub use usage::usage_text;

pub(crate) fn job_sessions_scroll_limit(app: &App) -> usize {
    job_sessions::scroll_limit(app)
}

pub(crate) fn dot_scroll_limit(app: &App) -> usize {
    dot_chat::scroll_limit(app)
}

pub(crate) fn dot_page(app: &App) -> usize {
    dot_chat::page(app)
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn plain() -> Style {
    Style::default()
}

/// Draw the whole UI; returns the hosted cursor (x, y, shape) to show.
pub fn render(f: &mut Frame, app: &mut App) -> Option<(u16, u16, u8)> {
    let area = f.area();
    if area.height < 4 || area.width < 30 {
        f.render_widget(Paragraph::new("cepheus: window too small"), area);
        return None;
    }
    let t = app.theme.clone();
    let footer = Rect { x: area.x, y: area.bottom() - 1, width: area.width, height: 1 };
    let main = Rect { height: area.height - 1, ..area };
    let side_width = if app.sidebar { app.cfg.sidebar_width.min(main.width / 2) } else { 0 };
    let pane = if side_width > 0 {
        Rect { x: main.x + side_width + 1, width: main.width - side_width - 1, ..main }
    } else {
        main
    };
    app.set_pane(pane);
    if app.dot_native() {
        dot_chat::prepare(app);
    }
    if side_width > 0 {
        sidebar(f, Rect { width: side_width, ..main }, app, &t);
        // The divider stays faint in every mode; the footer's mode tag shows where keys are routed.
        let line = t.faint();
        let x = main.x + side_width;
        for y in main.top()..main.bottom() {
            if let Some(cell) = f.buffer_mut().cell_mut((x, y)) {
                cell.set_symbol("│").set_style(line);
            }
        }
    } else {
        app.row_hits.clear();
        app.dev_hit = None;
    }
    let mut cursor = pane_view(f, pane, app, &t);
    if let Some(selection) = &app.selection {
        selection.highlight(pane, f.buffer_mut(), &t);
        if selection.dragged {
            cursor = None;
        }
    }
    footer_view(f, footer, app, &t);
    if app.dialog.is_some() {
        overlays::prepare_ask(app, area);
    }
    if let Some(dialog) = &app.dialog {
        scrim(f, main, &t);
        let mut viewport = app.dialog_viewport.clone();
        viewport.begin(dialog);
        // The quick-question box shows the terminal's own cursor.
        let cursor = match dialog {
            Dialog::Ask { draft, route, scroll } => {
                overlays::ask_view(f, area, draft, *route, *scroll, app, &t, &mut viewport)
            }
            _ => {
                dialog_view(f, area, dialog, app, &t, &mut viewport);
                None
            }
        };
        app.dialog_viewport = viewport;
        if let Some((x, y, _)) = cursor {
            f.set_cursor_position((x, y));
        }
        return cursor;
    }
    app.dialog_viewport = DialogViewport::default();
    if let Some((x, y, _)) = cursor {
        f.set_cursor_position((x, y));
    }
    cursor
}

/// Fade what lies behind an open window, all but the footer, so the
/// window reads as in front and the sidebar's cut-off text stays quiet.
fn scrim(f: &mut Frame, area: Rect, t: &Theme) {
    let faded = Style::default().fg(t.faint).bg(Color::Reset).remove_modifier(Modifier::all());
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(faded);
            }
        }
    }
}

fn spinner(app: &App) -> &'static str {
    SPINNER[(app.started.elapsed().as_millis() / 120) as usize % SPINNER.len()]
}

fn status_glyph(status: Status, app: &App, t: &Theme) -> (&'static str, Style) {
    match status {
        Status::Starting => ("…", t.dim()),
        Status::Working => (spinner(app), plain().fg(t.working)),
        Status::NeedsYou => ("!", plain().fg(t.needs).add_modifier(Modifier::BOLD)),
        Status::Finished => ("✓", plain().fg(t.finished).add_modifier(Modifier::BOLD)),
        Status::Idle => ("·", t.dim()),
        Status::Ended => ("×", t.faint()),
    }
}

/// Keep the spans that fit in `width` cells, truncating the one that crosses it.
fn fit(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let w = span.content.width();
        if used + w > width {
            let room = width.saturating_sub(used);
            if room > 1 {
                out.push(Span::styled(truncate_width(&span.content, room), span.style));
            }
            break;
        }
        used += w;
        out.push(span);
    }
    out
}

fn pad_line(mut spans: Vec<Span<'static>>, width: u16, bg: Option<Color>) -> Line<'static> {
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    if used < width as usize {
        spans.push(Span::raw(" ".repeat(width as usize - used)));
    }
    if let Some(bg) = bg {
        for span in &mut spans {
            span.style = span.style.bg(bg);
        }
    }
    Line::from(spans)
}

/// A usage bar read like a fuel gauge: what is left of the window fills it,
/// in the colour of how much is used, with an eighth-block for the partial
/// cell and a track in the faint shade.
fn gauge(used_percent: f64, cells: usize, t: &Theme) -> Vec<Span<'static>> {
    const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let level = t.level(used_percent);
    let left = 100.0 - used_percent.clamp(0.0, 100.0);
    let eighths = ((left / 100.0) * cells as f64 * 8.0).round() as usize;
    let full = (eighths / 8).min(cells);
    let mut spans = vec![Span::styled("█".repeat(full), plain().fg(level))];
    let mut used = full;
    if full < cells && !eighths.is_multiple_of(8) {
        spans.push(Span::styled(PARTS[eighths % 8], plain().fg(level).bg(t.faint)));
        used += 1;
    }
    spans.push(Span::styled("█".repeat(cells - used), t.faint()));
    spans.retain(|s| !s.content.is_empty());
    spans
}

/// One usage row: a model and the window of its that is nearest its limit.
#[derive(Debug, PartialEq)]
struct ModelUsage {
    program: String,
    name: String,
    color: Color,
    window: String,
    used: f64,
    resets_at: Option<u64>,
}

/// One row per model. A program's own windows go under its name and a model's own (`Argon wk`, `Fable wk`)
/// under the model's. Of a model's windows the row shows the most used, or of two as used the one that resets
/// later, labelled with that window. Only used windows count, so a model none of whose windows has a reported
/// share above nothing has no row.
fn model_usage<'a>(
    rows: &mut Vec<ModelUsage>,
    program: &str,
    color: Color,
    windows: impl IntoIterator<Item = &'a UsageWindow>,
) {
    let first = rows.len();
    let key = |used: f64, resets_at: Option<u64>| (used, resets_at.unwrap_or(u64::MAX));
    for w in windows {
        let Some(used) = w.used_percent.filter(|used| used.is_finite() && *used > 0.0) else { continue };
        let (name, window) = w.label.rsplit_once(' ').unwrap_or((program, &w.label));
        let row = ModelUsage {
            program: program.into(),
            name: name.into(),
            color,
            window: window.into(),
            used,
            resets_at: w.resets_at,
        };
        match rows[first..].iter_mut().find(|shown| shown.name == row.name) {
            Some(shown) if key(row.used, row.resets_at) > key(shown.used, shown.resets_at) => *shown = row,
            Some(_) => {}
            None => rows.push(row),
        }
    }
}

/// The usage table: every program's models, one row each, with the window,
/// a bar of what is left, the share left and when the window resets; only
/// the models named in `only` (a model's or its program's name) when it
/// names any.
fn usage_lines(lines: &mut Vec<Line<'static>>, usage: &Usage, only: &[String], width: u16, t: &Theme) {
    let mut rows = Vec::new();
    model_usage(&mut rows, "Claude", t.claude, usage.claude.iter().chain(&usage.claude_models));
    model_usage(&mut rows, "Codex", t.codex, &usage.codex);
    model_usage(&mut rows, "Grok", t.grok, &usage.grok);
    model_usage(&mut rows, "Gemini", t.antigravity, &usage.gemini);
    if !only.is_empty() {
        let named = |name: &str| only.iter().any(|o| o.eq_ignore_ascii_case(name));
        rows.retain(|row| named(&row.name) || named(&row.program));
    }
    if rows.is_empty() {
        return;
    }
    let now = util::now_secs();
    let rows: Vec<_> = rows
        .into_iter()
        .map(|row| {
            let reset = row.resets_at.map(|at| short_duration(at.saturating_sub(now))).unwrap_or_default();
            (truncate_width(&row.name, 7), truncate_width(&row.window, 8), reset, row)
        })
        .collect();
    let label_width = rows.iter().map(|(_, window, ..)| window.width()).max().unwrap_or(0).max(3);
    let reset_width = rows.iter().map(|(_, _, reset, _)| reset.width()).max().unwrap_or(0);
    let fixed_width = 8 + label_width + 1 + 10 + usize::from(reset_width > 0) + reset_width;
    let bar_cells = (width as usize).saturating_sub(fixed_width).min(10);
    lines.push(Line::raw(""));
    for (name, window, reset, row) in &rows {
        let name = format!(" {name}{}", " ".repeat(7usize.saturating_sub(name.width())));
        let window = format!("{window}{} ", " ".repeat(label_width.saturating_sub(window.width())));
        let mut spans = vec![Span::styled(name, t.bold().fg(row.color)), Span::styled(window, t.dim())];
        spans.extend(gauge(row.used, bar_cells, t));
        spans.push(Span::raw(format!(" {:>3.0}%", 100.0 - row.used.clamp(0.0, 100.0))));
        spans.push(Span::styled(" left", t.dim()));
        if !reset.is_empty() {
            spans.push(Span::styled(format!(" {reset:>reset_width$}"), t.dim()));
        }
        lines.push(Line::from(fit(spans, width as usize)));
    }
}

/// A program's mark in the list, drawn in its colour where its name was, so the worktree beside it has room.
/// Claude's and OpenAI's logos (Codicons) and the four-pointed star of Gemini, which Antigravity runs, come
/// from the Nerd Font symbols kitty bundles; that font has no Grok or opencode logo, so Grok gets the slashed
/// circle of its mark and opencode a framed square.
pub fn tool_icon(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "\u{ec82}",
        Tool::Codex => "\u{ec81}",
        Tool::Antigravity => "\u{f0ae2}",
        Tool::Grok => "⊘",
        Tool::OpenCode => "▣",
        Tool::Shell => "\u{f489}",
        Tool::Dynamic(_) => "◇",
    }
}

/// A session's two rows in the list: its place, state and title, then its program's mark, how it stands and its
/// worktree. The highlighted session keeps the same two rows, so moving through the list never reflows it.
fn session_lines(
    index: usize,
    s: &SessionInfo,
    selected: bool,
    width: u16,
    app: &App,
    t: &Theme,
) -> Vec<Line<'static>> {
    if s.tool == Tool::Dynamic("dot") {
        return dot_lines(s, selected, width, app, t);
    }
    let (glyph, glyph_style) = status_glyph(s.status, app, t);
    let bg = selected.then_some(t.selected_bg);
    let marker = || Span::styled(if selected { "▌" } else { " " }.to_string(), t.accent());
    // The title in bold stands out from the details under it; an ended
    // session's steps back.
    let title_style = if s.status == Status::Ended { t.dim() } else { t.bold() };
    let room = (width as usize).saturating_sub(7);
    let mut lines = vec![pad_line(
        vec![
            marker(),
            Span::styled(format!("{index:>2} "), t.dim()),
            Span::styled(glyph.to_string(), glyph_style),
            Span::raw(" "),
            Span::styled(truncate_width(&s.title, room), title_style),
        ],
        width,
        bg,
    )];
    let now = util::now_secs();
    let since = short_duration(now.saturating_sub(s.status_since));
    let mut head: Vec<Span<'static>> =
        vec![marker(), Span::raw("     "), Span::styled(tool_icon(s.tool).to_string(), plain().fg(t.tool(s.tool)))];
    let status = match s.status {
        Status::NeedsYou => Some((format!(" {}: {}", s.status.label().to_lowercase(), s.detail), plain().fg(t.needs))),
        Status::Ended => Some((format!(" {}", s.detail), t.dim())),
        Status::Starting => Some((" starting".to_string(), t.dim())),
        // The spinner says it works; the time alone leaves the worktree room.
        Status::Working => Some((format!(" {since}"), plain().fg(t.working))),
        // The green check says it all.
        Status::Finished => None,
        // So does the dot: how long it has been idle. A terminal has no
        // turns to time.
        Status::Idle if s.tool == Tool::Shell => None,
        Status::Idle => Some((format!(" {since}"), t.dim())),
    };
    if let Some((text, style)) = status {
        head.push(Span::styled(text, style));
    }
    // Where the branch was, how the agent works: its running sub-agents, what it is doing and when it last did
    // something; the branches have F3.
    let mut tail: Vec<Span<'static>> = Vec::new();
    if s.tool == Tool::Shell {
        let dir = s.cwd.as_deref().and_then(|c| Path::new(c).file_name()).map(|n| n.to_string_lossy().into_owned());
        if let Some(dir) = dir {
            tail.push(Span::styled(format!(" {dir}"), t.dim()));
        }
    }
    let agents = s.active_subagents();
    if agents > 0 {
        tail.push(Span::styled(format!(" ⑂{agents}"), t.bold().fg(t.task)));
    }
    if s.status == Status::Finished && s.status_since > 0 {
        tail.push(Span::styled(format!(" {since}"), t.dim()));
    }
    if let Some(activity) = s.activity.as_ref().filter(|_| s.status == Status::Working) {
        tail.push(Span::styled(format!(" {activity}"), t.dim()));
    } else if s.last_active > 0 && s.alive && matches!(s.status, Status::Idle | Status::NeedsYou) {
        let ago = short_duration(now.saturating_sub(s.last_active));
        tail.push(Span::styled(format!(" · active {ago} ago"), t.faint()));
    }
    let running = s.containers.iter().filter(|c| c.state == "running").count();
    if running > 0 {
        tail.push(Span::styled(format!(" ⬢{running}"), plain().fg(t.task)));
    }
    if s.review {
        tail.push(Span::styled(" review", plain().fg(t.review)));
    }
    if let Some(badges) = app.lua_badges.get(&s.id) {
        tail.extend(overlays::segment_spans(badges, t));
    }
    if !app.hosts.is_empty() || s.host.is_some() {
        tail.push(Span::styled(format!(" · {}", app.host_label(s.host.as_deref())), t.faint()));
    }
    tail.extend(attention::badges(app, s.id, t));
    head.extend(tail);
    lines.push(pad_line(fit(head, width as usize), width, bg));
    lines
}

/// How Dot is doing, as its pet shows it.
fn dot_mood(s: &SessionInfo) -> Mood {
    match s.status {
        _ if !s.alive || s.status == Status::Ended => Mood::Still,
        Status::Working => Mood::Working,
        Status::NeedsYou => Mood::Waiting,
        _ => Mood::Ready,
    }
}

/// One row of Dot's pet: kitty placeholder cells once the art is on the
/// terminal, else the plain mark.
fn dot_avatar(app: &App, mood: Mood, row: usize, t: &Theme) -> Span<'static> {
    let columns = AVATAR_COLUMNS as usize;
    match app.dot_avatar(mood) {
        Some(id) => Span::styled(kitty::row(row, columns), plain().fg(kitty::color(id))),
        None if row == 0 => {
            let icon = tool_icon(Tool::Dynamic("dot"));
            let pad = columns.saturating_sub(1 + icon.width());
            Span::styled(format!(" {icon}{}", " ".repeat(pad)), plain().fg(t.tool(Tool::Dynamic("dot"))))
        }
        None => Span::raw(" ".repeat(columns)),
    }
}

/// Dot drawn like an agent: its pet and its own name. Dot is always on in ChatGPT, so no ready, working,
/// finished, off or host words follow it; only a real failure between the deck and Dot shows under the name.
fn dot_lines(s: &SessionInfo, selected: bool, width: u16, app: &App, t: &Theme) -> Vec<Line<'static>> {
    let bg = selected.then_some(t.selected_bg);
    let marker = || Span::styled(if selected { "▌" } else { " " }.to_string(), t.accent());
    let mood = dot_mood(s);
    let room = (width as usize).saturating_sub(AVATAR_COLUMNS as usize + 3);
    let title = vec![
        marker(),
        dot_avatar(app, mood, 0, t),
        Span::raw(" "),
        Span::styled(truncate_width(&app.dot_name(), room), t.bold()),
    ];
    let mut detail = vec![marker(), dot_avatar(app, mood, 1, t), Span::raw(" ")];
    if let Some(failure) = app.dot_failure().filter(|_| s.alive && s.host.is_none()) {
        detail.push(Span::styled("✗ ", t.bold().fg(t.bad)));
        detail.push(Span::styled(util::one_line(failure), plain().fg(t.bad)));
    }
    vec![pad_line(fit(title, width as usize), width, bg), pad_line(fit(detail, width as usize), width, bg)]
}

/// A section of sessions, kept in the supplied global order.
pub struct SessionGroup<'a> {
    /// None for this machine.
    pub host: Option<String>,
    pub label: String,
    /// Why the host is offline; none while it is online, and for this machine.
    pub offline: Option<String>,
    pub sessions: Vec<&'a SessionInfo>,
}

/// What the deck says of a host that is not online.
pub fn host_state(h: &HostInfo) -> String {
    match (&h.reason, h.online) {
        (_, true) => "online".into(),
        (Some(why), false) if why == "connecting" => "connecting…".into(),
        (Some(why), false) => format!("offline · {why}"),
        (None, false) => "offline".into(),
    }
}

/// A single session list retains the global saved order across machines.
/// Host availability remains visible in the pinned resource table.
pub fn session_groups<'a>(_app: &App, sessions: &[&'a SessionInfo]) -> Vec<SessionGroup<'a>> {
    vec![SessionGroup { host: None, label: "Sessions".into(), offline: None, sessions: sessions.to_vec() }]
}

/// The dev server button: what clicking it does next, in the colour of the
/// server's state. `None` while the service is too old to run one.
pub fn dev_button(app: &App, t: &Theme) -> Option<Vec<Span<'static>>> {
    if app.service_version < 2 {
        return None;
    }
    let dev = &app.state.dev_server;
    let port = dev.port.map(|p| format!(" :{p}")).unwrap_or_default();
    Some(if dev.pid.is_some() {
        vec![Span::styled("■ dev", t.bold().fg(t.ok)), Span::styled(port, plain().fg(t.ok))]
    } else if dev.started_at.is_some() {
        vec![Span::styled("… dev", t.dim()), Span::styled(" stopping", t.dim())]
    } else if dev.port_busy {
        vec![Span::styled("▶ dev", t.dim()), Span::styled(format!("{port} busy"), plain().fg(t.warn))]
    } else {
        vec![Span::styled("▶ dev", t.accent())]
    })
}

/// The header's title: the start directory, its branch and where that stands against its upstream, as the
/// owner's shell prompt shows it ("project main ⇡3"), cut to `room` cells. The deck's own name never leads it.
/// `inline` follows straight after, before the cut.
pub fn title_spans(
    dir: &str,
    git: Option<&BranchStatus>,
    inline: Vec<Span<'static>>,
    room: usize,
    t: &Theme,
) -> Vec<Span<'static>> {
    let mut repo = Vec::new();
    if !dir.is_empty() {
        repo.push(Span::styled(format!(" {dir}"), t.bold().fg(t.accent)));
    }
    if let Some(git) = git {
        repo.push(Span::styled(format!(" {}", git.branch), t.bold().fg(t.task)));
        let mut counts = String::new();
        if git.ahead > 0 {
            counts += &format!("⇡{}", git.ahead);
        }
        if git.behind > 0 {
            counts += &format!("⇣{}", git.behind);
        }
        if !counts.is_empty() {
            repo.push(Span::styled(format!(" {counts}"), t.bold().fg(t.warn)));
        }
        // The count of unmerged branches beside it is the bundled lua/unmerged_branches.lua, drawn here as an
        // inline header section.
    }
    repo.extend(inline);
    fit(repo, room)
}

/// CPU, memory in use and disk space used, each in the colour of how full it is.
fn readings(r: &Resources, t: &Theme) -> [Span<'static>; 3] {
    let percent = |v: Option<f64>| match v {
        Some(p) => Span::styled(format!("{p:.0}%"), plain().fg(t.level(p))),
        None => Span::styled("–", t.dim()),
    };
    let disk = match r.disk.and_then(|d| Some((d, d.percent_used()?))) {
        Some((d, full)) => Span::styled(resources::short_size(d.used), plain().fg(t.level(full))),
        None => Span::styled("–", t.dim()),
    };
    [percent(r.cpu), percent(r.memory), disk]
}

pub fn resource_line(r: &Resources, width: u16, t: &Theme) -> Line<'static> {
    let [cpu, memory, disk] = readings(r, t);
    let spans = vec![
        Span::styled(" CPU ", t.dim()),
        cpu,
        Span::styled("  RAM ", t.dim()),
        memory,
        Span::styled("  SSD ", t.dim()),
        disk,
        Span::styled(" used", t.dim()),
    ];
    Line::from(fit(spans, width as usize))
}

/// The hosts' stats table: each column's heading and its widest reading ("100%", "1023M"), two cells apart. A
/// heading over the columns instead of a label beside every reading leaves each row room for the disk.
const HOST_COLUMNS: [(&str, usize); 3] = [("CPU", 4), ("RAM", 4), ("SSD", 5)];
const HOST_GAP: usize = 2;

/// The cells a host's name may take in a sidebar `width` wide: its longest
/// label, shortened only when the readings would not fit beside it.
pub fn host_label_width(longest: usize, width: u16) -> usize {
    let readings: usize = HOST_COLUMNS.iter().map(|(_, w)| HOST_GAP + w).sum();
    longest.min((width as usize).saturating_sub(1 + readings)).max(1)
}

/// The heading over the hosts' stats rows.
pub fn host_resource_header(label_width: usize, width: u16, t: &Theme) -> Line<'static> {
    let mut text = " ".repeat(1 + label_width);
    for (name, w) in HOST_COLUMNS {
        text.push_str(&format!("{name:>0$}", HOST_GAP + w));
    }
    Line::from(fit(vec![Span::styled(text, t.dim())], width as usize))
}

/// A host's resources as the sidebar line reads them.
pub fn host_resources(stats: &HostStats) -> Resources {
    let disk = match (stats.disk_used, stats.disk_available) {
        (Some(used), Some(available)) => Some(resources::DiskSpace { used, available }),
        _ => None,
    };
    Resources { cpu: stats.cpu, memory: stats.memory, disk }
}

/// One host's line when the deck drives several: its name, then its CPU, memory and disk under
/// `host_resource_header`, or why it is offline.
pub fn host_resource_line(
    label: &str,
    label_width: usize,
    resources: Result<&Resources, &str>,
    width: u16,
    t: &Theme,
) -> Line<'static> {
    let name = truncate_width(label, label_width);
    let pad = label_width.saturating_sub(name.width());
    let mut spans = vec![Span::styled(format!(" {name}{}", " ".repeat(pad)), t.bold())];
    match resources {
        Ok(r) => {
            for ((_, w), reading) in HOST_COLUMNS.iter().zip(readings(r, t)) {
                spans.push(Span::styled(format!("{:>1$}", reading.content, HOST_GAP + w), reading.style));
            }
        }
        Err(why) => spans.push(Span::styled(format!("{}{why}", " ".repeat(HOST_GAP)), t.dim())),
    }
    Line::from(fit(spans, width as usize))
}

/// One row of the stats table: a machine's name and its readings, or why it
/// has none.
pub struct MachineRow {
    pub label: String,
    pub readings: Result<Resources, String>,
}

/// The machines the stats table lists: this one, each host the deck drives, then each machine a script reports
/// with `deck.ui.machine`. `all` keeps the script machines that stay out of the sidebar, for `:machines`.
pub fn machine_rows(app: &App, all: bool) -> Vec<MachineRow> {
    let here = match &app.state.native_host {
        Some(host) => host.stats.as_ref().map(host_resources).unwrap_or_default(),
        None => app.resources.now.clone(),
    };
    let mut rows = vec![MachineRow { label: app.host_label(None), readings: Ok(here) }];
    for h in &app.hosts {
        let readings = h.stats.as_ref().filter(|_| h.online).map(host_resources).ok_or_else(|| host_state(h));
        rows.push(MachineRow { label: h.label.clone(), readings });
    }
    for m in app.lua_machines.iter().filter(|m| all || m.sidebar) {
        let disk = match (m.disk_used, m.disk_available) {
            (Some(used), Some(available)) => Some(resources::DiskSpace { used, available }),
            _ => None,
        };
        let readings = match &m.status {
            Some(why) => Err(why.clone()),
            None => Ok(Resources { cpu: m.cpu, memory: m.memory, disk }),
        };
        rows.push(MachineRow { label: m.label.clone(), readings });
    }
    rows
}

/// The stats table: the column headings, then a line per machine.
pub fn machine_table(rows: &[MachineRow], width: u16, t: &Theme) -> Vec<Line<'static>> {
    let longest = rows.iter().map(|r| r.label.width()).max().unwrap_or(0);
    let label_width = host_label_width(longest, width);
    let mut lines = vec![host_resource_header(label_width, width, t)];
    for row in rows {
        let readings = row.readings.as_ref().map_err(String::as_str);
        lines.push(host_resource_line(&row.label, label_width, readings, width, t));
    }
    lines
}

/// The global session list, recording where each row is.
fn session_part(lines: &mut Vec<Line<'static>>, hits: &mut Vec<(usize, usize, Sel)>, app: &App, width: u16, t: &Theme) {
    let sessions: Vec<_> = app.ordered_sessions().into_iter().filter(|s| !app.is_cloud(s)).collect();
    let mut index = 0;
    for group in session_groups(app, &sessions) {
        // Hosts without sessions already have a resource/status row above.
        // Empty headings here can push a false "more" marker below the last session.
        if group.host.is_some() && group.sessions.is_empty() {
            continue;
        }
        let live = group.sessions.iter().filter(|s| s.alive).count();
        let heading = match (&group.offline, group.host.is_some() || !app.hosts.is_empty()) {
            (Some(why), _) => vec![
                Span::styled(format!(" {}", group.label.to_uppercase()), t.bold()),
                Span::styled(format!("  {why}"), t.dim()),
            ],
            (None, true) => vec![
                Span::styled(format!(" {}", group.label.to_uppercase()), t.bold()),
                Span::styled(format!("  {live} live"), t.dim()),
            ],
            (None, false) => {
                vec![Span::styled(" SESSIONS", t.bold()), Span::styled(format!("  {live} live"), t.dim())]
            }
        };
        if index > 0 || group.host.is_some() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(fit(heading, width as usize)));
        if group.sessions.is_empty() && group.host.is_none() {
            lines.push(Line::from(hint_line("   none yet · {:new} starts one", t)));
        }
        for s in &group.sessions {
            index += 1;
            let selected = app.current == Some(Sel::Session(s.id));
            let rows = session_lines(app.position(s.id).unwrap_or(index), s, selected, width, app, t);
            hits.push((lines.len(), rows.len(), Sel::Session(s.id)));
            lines.extend(rows);
        }
    }
}

/// The cloud agents that are on. An agent turned off leaves no row, so with none on the section and its heading
/// go too, until `:dot on`.
fn cloud_part(lines: &mut Vec<Line<'static>>, hits: &mut Vec<(usize, usize, Sel)>, app: &App, width: u16, t: &Theme) {
    let active: Vec<_> = app.ordered_sessions().into_iter().filter(|s| app.is_cloud(s)).collect();
    if active.is_empty() {
        return;
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(" CLOUD AGENTS", t.bold()));
    for session in &active {
        let rows = session_lines(
            app.position(session.id).unwrap_or(1),
            session,
            app.current == Some(Sel::Session(session.id)),
            width,
            app,
            t,
        );
        hits.push((lines.len(), rows.len(), Sel::Session(session.id)));
        lines.extend(rows);
    }
}

/// The sidebar's parts in their order: `deck.opt.sidebar` when a script set it, else the config's
/// `sidebar.parts`, else the deck's own with the scripts' sections before the sessions. Built-in parts are
/// title, resources, dev, usage, cloud and sessions; usage has its own window (`:usage`) and is shown here only
/// when listed. A section listed by name shows even when it keeps itself off the sidebar. Slots, the queue and
/// jobs keep their own windows (`:slots`, `:queue`, `:jobs`). Legacy outside parts stay hidden, including in
/// custom sidebar orders.
pub fn sidebar_parts(app: &App) -> Vec<String> {
    let chosen = app.sidebar_order.clone().or_else(|| Some(app.cfg.sidebar.parts.clone()).filter(|p| !p.is_empty()));
    let listed = chosen.is_some();
    let mut parts = chosen.unwrap_or_else(|| {
        let mut parts: Vec<String> = ["title", "resources", "dev"].iter().map(|p| p.to_string()).collect();
        parts.extend(app.lua_sections.iter().filter(|s| s.sidebar && s.header.is_none()).map(|s| s.id.clone()));
        parts.push("sessions".to_string());
        parts
    });
    if !parts.iter().any(|p| p == "cloud") {
        let before = parts.iter().position(|p| p == "sessions").unwrap_or(parts.len());
        parts.insert(before, "cloud".into());
    }
    // Header sections sit on the title row, and `sidebar = false` ones
    // (the tasks window) stay off the deck's own order.
    parts.retain(|id| {
        id != "outside"
            && !app.sidebar_hidden.contains(id)
            && !app.lua_sections.iter().any(|s| s.id == *id && (s.header.is_some() || (!s.sidebar && !listed)))
    });
    parts
}

fn title_with_sections(
    app: &App,
    dir: &str,
    git: Option<&BranchStatus>,
    width: usize,
    t: &Theme,
) -> Vec<Span<'static>> {
    let first_lines = move |place: HeaderPlace| {
        app.lua_sections
            .iter()
            .filter(move |s| s.header == Some(place) && !app.sidebar_hidden.contains(&s.id))
            .filter_map(|s| s.lines.first().filter(|line| !line.is_empty()))
    };
    let mut inline: Vec<Span<'static>> = Vec::new();
    for line in first_lines(HeaderPlace::Inline) {
        inline.push(Span::raw(" "));
        inline.extend(overlays::segment_spans(line, t));
    }
    let mut right: Vec<Span<'static>> = Vec::new();
    for line in first_lines(HeaderPlace::Right) {
        if !right.is_empty() {
            right.push(Span::raw(" "));
        }
        right.extend(overlays::segment_spans(line, t));
    }
    if right.is_empty() {
        return title_spans(dir, git, inline, width, t);
    }
    let right = fit(right, width.saturating_sub(2));
    let right_width = right.iter().map(|s| s.content.width()).sum::<usize>();
    let mut spans = title_spans(dir, git, inline, width.saturating_sub(right_width + 2), t);
    let left_width = spans.iter().map(|s| s.content.width()).sum::<usize>();
    spans.push(Span::raw(" ".repeat(width.saturating_sub(left_width + right_width + 1))));
    spans.extend(right);
    spans.push(Span::raw(" "));
    spans
}

/// The parts that stay at the top while the rest scrolls.
fn pinned_part(app: &App, part: &str) -> bool {
    matches!(part, "title" | "resources" | "dev") || app.lua_sections.iter().any(|s| s.id == part && s.pinned)
}

fn sidebar(f: &mut Frame, area: Rect, app: &mut App, t: &Theme) {
    let width = area.width;
    let (top, rest): (Vec<String>, Vec<String>) = sidebar_parts(app).into_iter().partition(|p| pinned_part(app, p));
    // Repository status, resource usage, the dev button and pinned
    // sections stay at the top while the rest scrolls.
    let mut header: Vec<Line<'static>> = Vec::new();
    app.dev_hit = None;
    for part in &top {
        match part.as_str() {
            "title" => {
                let dir = Path::new(&app.state.start_dir)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let git = app.git.now.as_ref();
                header.push(pad_line(title_with_sections(app, &dir, git, width as usize, t), width, None));
            }
            "resources" => match machine_rows(app, false).as_slice() {
                // This machine alone keeps the labelled line.
                [MachineRow { readings: Ok(here), .. }] => header.push(resource_line(here, width, t)),
                rows => header.extend(machine_table(rows, width, t)),
            },
            "dev" => {
                let button = dev_button(app, t);
                let button_width =
                    button.as_ref().map_or(0, |b| b.iter().map(|s| s.content.width()).sum::<usize>() as u16);
                let button_row = header.len() as u16;
                if let Some(button) = button.filter(|_| button_width + 2 <= width && area.height > button_row) {
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(button);
                    header.push(Line::from(spans));
                    app.dev_hit = Some((area.y + button_row, area.x + 1, area.x + 1 + button_width));
                }
            }
            id => {
                if let Some(section) = app.lua_sections.iter().find(|s| s.id == id) {
                    header.extend(overlays::section_lines(section, width, t));
                }
            }
        }
    }
    let header_height = (header.len() as u16).min(area.height);
    f.render_widget(Paragraph::new(header), Rect { height: header_height, ..area });
    let area = Rect { y: area.y + header_height, height: area.height - header_height, ..area };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<(usize, usize, Sel)> = Vec::new();
    for part in &rest {
        match part.as_str() {
            "usage" => usage_lines(&mut lines, &app.state.usage, &app.cfg.sidebar.usage, width, t),
            "cloud" => cloud_part(&mut lines, &mut hits, app, width, t),
            "sessions" => {
                lines.push(Line::raw(""));
                session_part(&mut lines, &mut hits, app, width, t);
            }
            id => {
                if let Some(section) = app.lua_sections.iter().find(|s| s.id == id) {
                    lines.push(Line::raw(""));
                    lines.extend(overlays::section_lines(section, width, t));
                }
            }
        }
    }
    let height = area.height as usize;
    let selected = hits.iter().find(|(_, _, s)| Some(*s) == app.current).map(|(l, h, _)| (*l, *l + *h));
    let window = list_window(lines.len(), height, selected);
    let first = area.y + u16::from(window.top);
    let end = window.offset + window.shown;
    app.row_hits = hits
        .iter()
        .filter(|(l, ..)| *l >= window.offset && *l < end)
        .map(|(l, h, s)| (first + (*l - window.offset) as u16, (*h).min(end - *l) as u16, *s))
        .collect();
    let hidden = |keep: &dyn Fn(usize) -> bool| -> Vec<Sel> {
        hits.iter().filter(|(l, ..)| keep(*l)).map(|(_, _, s)| *s).collect()
    };
    let mut visible: Vec<Line<'static>> = Vec::new();
    if window.top {
        visible.push(more_line("↑", &hidden(&|l| l < window.offset), app, width, t));
    }
    visible.extend(lines.into_iter().skip(window.offset).take(window.shown));
    if window.bottom {
        visible.push(more_line("↓", &hidden(&|l| l >= end), app, width, t));
    }
    f.render_widget(Paragraph::new(visible), area);
}

/// The part of the list in view: from line `offset`, `shown` lines, with a
/// line above (`top`) or below (`bottom`) that says what is out of view.
#[derive(Debug, PartialEq)]
pub struct ListWindow {
    pub offset: usize,
    pub shown: usize,
    pub top: bool,
    pub bottom: bool,
}

/// Which of `total` list lines fit in `height` rows with the selected row,
/// lines `selected.0` up to `selected.1`, in view.
pub fn list_window(total: usize, height: usize, selected: Option<(usize, usize)>) -> ListWindow {
    if total <= height || height < 3 {
        return ListWindow { offset: 0, shown: total.min(height), top: false, bottom: false };
    }
    let end = selected.map_or(0, |(_, end)| end);
    let mut offset = 0;
    if end > height - 1 {
        // Room for both "more" lines, unless the list's end is in view.
        offset = (end - (height - 2)).min(total - (height - 1));
    }
    let top = offset > 0;
    let room = height - usize::from(top);
    let bottom = offset + room < total;
    ListWindow { offset, shown: room - usize::from(bottom), top, bottom }
}

/// "↓ 12 more · 1 needs you · 2 ✓": the rows out of view, and how many of
/// them want the owner.
fn more_line(arrow: &str, rows: &[Sel], app: &App, width: u16, t: &Theme) -> Line<'static> {
    let sessions: Vec<&SessionInfo> = rows
        .iter()
        .filter_map(|r| match r {
            Sel::Session(id) => app.session(*id),
            Sel::Job(_) => None,
            #[cfg(test)]
            Sel::Outside(_) => None,
        })
        .collect();
    let needs = sessions.iter().filter(|s| s.status == Status::NeedsYou).count();
    let finished = sessions.iter().filter(|s| s.status == Status::Finished).count();
    let more = if rows.is_empty() { " more".to_string() } else { format!(" {} more", rows.len()) };
    let mut spans = vec![Span::styled(format!(" {arrow}"), t.accent()), Span::styled(more, t.dim())];
    if needs > 0 {
        spans.push(Span::styled(" · ", t.faint()));
        spans.push(Span::styled(format!("{needs} needs you"), t.bold().fg(t.needs)));
    }
    if finished > 0 {
        spans.push(Span::styled(" · ", t.faint()));
        spans.push(Span::styled(format!("{finished} ✓"), plain().fg(t.finished)));
    }
    Line::from(fit(spans, width as usize))
}

fn card(f: &mut Frame, pane: Rect, title: Span<'static>, body: Vec<Line<'static>>, t: &Theme) {
    // Wide enough for the longest line, so a long path stays beside its label.
    let content = body.iter().map(Line::width).max().unwrap_or(0) as u16 + 2;
    let width = content.max(76).min(pane.width.saturating_sub(4));
    let height = (body.len() as u16 + 2).min(pane.height);
    let rect = Rect {
        x: pane.x + (pane.width.saturating_sub(width)) / 2,
        y: pane.y + (pane.height.saturating_sub(height)) / 3,
        width,
        height,
    };
    // A title without its own colour would keep the border's faint colour.
    let style = if title.style.fg.is_none() { title.style.fg(Color::Reset) } else { title.style };
    let title = Span::styled(format!(" {} ", title.content), style.add_modifier(Modifier::BOLD));
    let block =
        Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(t.faint()).title(title);
    f.render_widget(Paragraph::new(body).block(block).wrap(Wrap { trim: false }), rect);
}

fn kv(key: &str, value: impl Into<String>, t: &Theme) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{key:<14}"), t.dim()), Span::raw(value.into())])
}

/// A path with the home directory written `~`, as Claude and Codex show it.
fn home_path(path: &str, home: Option<&str>) -> String {
    let rest = home.filter(|h| !h.is_empty() && *h != "/").and_then(|h| path.strip_prefix(h));
    match rest {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Hint text with `{key}` marks: keys in the accent colour, ` · ` separators
/// faint and everything else dim.
fn hint_line(text: &str, t: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, item) in text.split(" · ").enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", t.faint()));
        }
        for (j, part) in item.split(['{', '}']).enumerate() {
            if !part.is_empty() {
                spans.push(Span::styled(part.to_string(), if j % 2 == 1 { t.accent() } else { t.dim() }));
            }
        }
    }
    spans
}

/// Whether a screen has nothing drawn on it.
fn frame_is_blank(frame: &screen::ScreenFrame) -> bool {
    frame.lines.iter().flatten().all(|r| r.t.trim().is_empty())
}

/// Describe what a live session with no rendered output is waiting for. SessionStart hooks may run before a
/// Claude session draws its composer.
fn waiting_note(f: &mut Frame, pane: Rect, tool: crate::protocol::Tool, t: &Theme) {
    let name = match tool {
        crate::protocol::Tool::Claude => "Claude Code",
        other => other.label(),
    };
    let mut lines = vec![Line::styled(format!("Waiting for {name} to draw its screen…"), t.dim())];
    if tool == crate::protocol::Tool::Claude {
        lines.push(Line::styled("It runs its SessionStart hooks first.", t.dim()));
    }
    let height = (lines.len() as u16).min(pane.height);
    let rect = Rect { y: pane.y + pane.height.saturating_sub(height) / 2, height, ..pane };
    f.render_widget(Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center), rect);
}

fn pane_view(f: &mut Frame, pane: Rect, app: &App, t: &Theme) -> Option<(u16, u16, u8)> {
    match app.current {
        Some(Sel::Job(id)) => {
            job_sessions::pane(f, pane, app, id, t);
            None
        }
        Some(Sel::Session(id)) => {
            let s = app.session(id)?;
            if app.dot_native() {
                return dot_chat::pane(f, pane, app, t);
            }
            if let Some((fid, frame)) = &app.frame {
                if *fid == id {
                    let frame = app.session_frame(id).unwrap_or(frame);
                    // Keep the text under a moving drag stable while the
                    // agent continues sending screen updates underneath it.
                    let frame = app.selection.as_ref().filter(|s| s.pressed).map_or(frame, |s| &s.frame);
                    screen::render_frame(frame, pane, f.buffer_mut());
                    if s.alive && frame.offset == 0 && frame_is_blank(frame) {
                        waiting_note(f, pane, s.tool, t);
                    }
                    if frame.offset > 0 {
                        let text = app.history_label().unwrap_or_else(|| {
                            format!(" ↑ {} of {} lines · legacy source viewport ", frame.offset, frame.history)
                        });
                        let w = (text.width() as u16).min(pane.width);
                        let rect = Rect { x: pane.right() - w, y: pane.y, width: w, height: 1 };
                        f.render_widget(Paragraph::new(text).style(t.tag(t.accent)), rect);
                    }
                    if !s.alive {
                        let text = format!(" {} · Enter reopens · :close removes ", s.detail);
                        let w = (text.width() as u16).min(pane.width);
                        let rect = Rect { x: pane.x, y: pane.bottom() - 1, width: w, height: 1 };
                        // Not the working colour: nothing runs.
                        let style = plain().fg(Color::Reset).bg(t.selected_bg).add_modifier(Modifier::BOLD);
                        f.render_widget(Paragraph::new(text).style(style), rect);
                        return None;
                    }
                    // As in Neovim, the cursor shows where typing goes: only
                    // in Insert mode is it in the session.
                    if frame.offset > 0 || app.mode != Mode::Insert {
                        return None;
                    }
                    return frame
                        .cursor
                        .map(|c| (pane.x + c.x, pane.y + c.y, c.shape))
                        .filter(|(x, y, _)| *x < pane.right() && *y < pane.bottom());
                }
            }
            let tool_title = Span::styled(s.tool.label().to_string(), plain().fg(t.tool(s.tool)));
            if s.alive {
                // A running session's screen is on its way; only a program
                // still starting has nothing to show yet.
                if s.status == Status::Starting {
                    card(f, pane, tool_title, vec![Line::styled("Starting…", t.dim())], t);
                }
            } else {
                let mut body = vec![
                    kv("Program", s.tool.label(), t),
                    kv("Title", s.title.clone(), t),
                    kv("Status", "not running", t),
                ];
                match &s.conversation {
                    Some(conv) => {
                        body.push(kv("Conversation", conv.clone(), t));
                        body.push(Line::raw(""));
                        body.push(Line::from(hint_line(
                            "{Enter} resumes it here · {:close} removes it from the deck",
                            t,
                        )));
                    }
                    None => body.push(Line::styled("No conversation was recorded, so it cannot be resumed.", t.dim())),
                }
                card(f, pane, Span::raw("Ended session"), body, t);
            }
            None
        }
        #[cfg(test)]
        Some(Sel::Outside(pid)) => {
            let o = app.outside(pid)?;
            let home = std::env::var("HOME").ok();
            let pid = if o.host.is_some() { crate::protocol::split_pid(o.pid).1 } else { o.pid };
            let mut body = vec![
                kv("Host", app.host_label(o.host.as_deref()), t),
                kv("Program", format!("{} (pid {pid})", o.tool.label()), t),
                kv("Title", o.title.clone(), t),
                kv(
                    "State",
                    match o.busy {
                        Some(true) => "working",
                        Some(false) => "idle",
                        None => "unknown",
                    },
                    t,
                ),
                kv("Directory", home_path(&o.cwd, home.as_deref()), t),
            ];
            if let Some(wt) = &o.worktree {
                body.push(kv("Worktree", home_path(&wt.path, home.as_deref()), t));
            }
            if let Some(conv) = &o.conversation {
                body.push(kv("Conversation", conv.clone(), t));
            }
            let window = match (&o.host, &o.kitty_window, &o.workspace) {
                (Some(_), _, _) => "Remote host; local window focus unavailable".to_string(),
                (_, Some(w), Some(ws)) => format!("kitty window {w} on workspace {ws}"),
                (_, Some(w), None) => format!("kitty window {w}"),
                _ => "No desktop window found".to_string(),
            };
            body.push(kv("Window", window, t));
            body.push(Line::raw(""));
            body.push(Line::from(hint_line(
                &app.keys.fill(if o.host.is_none() && o.kitty_pid.is_some() {
                    "{Enter} moves it into the deck · {:window} goes to its window"
                } else {
                    "{Enter} moves it into the deck"
                }),
                t,
            )));
            card(f, pane, Span::raw("Running outside the deck"), body, t);
            None
        }
        None => {
            let key = |k: &str, v: &str| {
                Line::from(vec![Span::styled(format!("{k:<14}"), t.accent()), Span::raw(v.to_string())])
            };
            let mut body = vec![
                Line::raw("Every Claude Code, Codex, Antigravity, Grok and opencode session, in one place."),
                Line::raw(""),
                key("Enter", "start a session in the primary checkout"),
            ];
            body.push(key(":resume", "resume an earlier conversation"));
            body.extend([
                key(":help", "every key and command"),
                Line::raw(""),
                Line::styled(
                    format!(
                        "Local agents start in {}",
                        home_path(&app.state.start_dir, std::env::var("HOME").ok().as_deref())
                    ),
                    t.dim(),
                ),
            ]);
            card(f, pane, Span::styled("cepheus", t.accent()), body, t);
            None
        }
    }
}

/// The highlighted session as the footer names it: its place in the list
/// and its program; with the list hidden, its title and state too.
fn session_spans(app: &App, s: &SessionInfo, t: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    // Dot, always on in ChatGPT, goes by its own name and has no status.
    if s.tool == Tool::Dynamic("dot") {
        spans.push(Span::styled(app.dot_name(), plain().fg(t.tool(s.tool))));
        return spans;
    }
    if let Some(pos) = app.position(s.id) {
        spans.push(Span::styled(format!("{pos} "), t.dim()));
    }
    spans.push(Span::styled(s.tool.label().to_string(), plain().fg(t.tool(s.tool))));
    if !app.sidebar {
        spans.push(Span::styled(
            format!(" \u{201c}{}\u{201d} ", truncate_width(&util::one_line(&s.title), 40)),
            t.bold(),
        ));
        let (glyph, style) = status_glyph(s.status, app, t);
        spans.push(Span::styled(glyph.to_string(), style));
        match s.status {
            Status::Finished => {}
            Status::NeedsYou => spans.push(Span::styled(" needs you", plain().fg(t.needs))),
            other => spans.push(Span::styled(format!(" {}", other.label().to_lowercase()), t.dim())),
        }
    }
    spans
}

fn footer_view(f: &mut Frame, area: Rect, app: &App, t: &Theme) {
    let (tag, tag_style) = match app.mode {
        Mode::Normal => (" NORMAL ", t.tag(t.accent)),
        Mode::Insert => (" INSERT ", t.tag(t.finished)),
        Mode::Command => (" COMMAND ", t.tag(t.working)),
    };
    // With the list hidden nothing else says which session the pane shows.
    let lead = match app.current_session().filter(|_| !app.sidebar) {
        Some(s) => session_spans(app, s, t),
        None => Vec::new(),
    };
    // Dot is always on rather than a worker that finishes: it is not counted.
    let count = |status: Status| app.state.sessions.iter().filter(|s| s.status == status && !app.is_cloud(s)).count();
    let (working, needs, finished) = (count(Status::Working), count(Status::NeedsYou), count(Status::Finished));
    let working_style = if working > 0 { plain().fg(t.working) } else { t.dim() };
    let needs_style = if needs > 0 { t.bold().fg(t.needs) } else { t.dim() };
    let finished_style = if finished > 0 { plain().fg(t.finished) } else { t.dim() };
    let mut right = Vec::new();
    let unseen = app.unseen_errors();
    if unseen > 0 {
        let label = if unseen == 1 { "1 error".to_string() } else { format!("{unseen} errors") };
        // While typing, F2 goes to the session; the list is one Ctrl-w w away.
        if let Some(key) = app.keys.label(Hotkey::Errors).filter(|_| app.mode != Mode::Insert) {
            right.push(Span::styled(format!(" {key}"), t.bold().fg(t.needs)));
        }
        right.push(Span::styled(format!(" {label}"), plain().fg(t.needs)));
        right.push(Span::styled(" ·", t.faint()));
    }
    right.extend([
        Span::styled(format!(" {working} working"), working_style),
        Span::styled(" · ", t.faint()),
        Span::styled(format!("{needs} need you"), needs_style),
        Span::styled(" · ", t.faint()),
        Span::styled(format!("{finished} ✓ "), finished_style),
    ]);
    let right_width = right.iter().map(|s| s.content.width()).sum::<usize>() as u16;
    let left_width = area.width.saturating_sub(right_width);
    let room = (left_width as usize).saturating_sub(tag.width() + 2);
    let mut spans = vec![Span::styled(tag, tag_style), Span::raw(" ")];
    match &app.notice {
        _ if app.mode == Mode::Command => {
            let (before, after) = app.command.split();
            let (before, after) = around_cursor(before, after, room.saturating_sub(2));
            let left = room.saturating_sub(before.width() + after.width() + 2);
            spans.push(Span::raw(format!(":{before}")));
            spans.push(Span::styled("▏", t.accent()));
            spans.push(Span::raw(after));
            if let Some(c) = &app.completion {
                let mut choices = Vec::new();
                for (i, item) in c.items.iter().enumerate() {
                    let style = if c.at == Some(i) { t.bold().fg(t.accent) } else { t.dim() };
                    choices.push(Span::styled(format!(" {item}"), style));
                }
                spans.extend(fit(choices, left));
            }
        }
        Some((text, error, _)) => {
            let style = if *error { t.bold().fg(t.needs) } else { t.accent() };
            // The footer is one row and drops control characters, so a
            // notice's further lines (a broadcast's per-host outcomes, an
            // outage's reason) follow its summary after a separator rather
            // than running into it.
            let text = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" · ");
            spans.push(Span::styled(truncate_width(&text, room), style));
        }
        None => spans.extend(fit(lead, room)),
    }
    f.render_widget(Paragraph::new(Line::from(spans)), Rect { width: left_width, ..area });
    if right_width < area.width {
        f.render_widget(Paragraph::new(Line::from(right)), Rect { x: area.x + left_width, width: right_width, ..area });
    }
}

/// The list's own keys for the help sheet, `<window>` being the window
/// hotkey; the other hotkeys (`HOTKEYS`) follow.
const HELP: &[(&str, &str)] = &[
    ("↑ ↓", "pick a session; the pane shows it live (j k work too)"),
    ("Enter", "type into the selected session"),
    ("<window> l", "type into the selected session (→ too) · <window> w, <window> h or ← back to the list"),
    ("Esc", "while typing it goes to the session: it interrupts the agent"),
    ("Tab  1-9", "next session that needs you or has finished · jump to session N"),
    ("PgUp PgDn", "local history · Shift/Ctrl: 10 pages · Ctrl-Home: oldest retained · End: live"),
    ("History", "page keys work while typing; typing/paste returns live; Normal commands keep their roles"),
    ("Drag", "select draft or output text; release to copy"),
    (":", "enter a command; Tab completes; ↑ ↓ (Ctrl-p Ctrl-n) recall history; the full command list follows below"),
];

/// The help sheet's hotkey rows: each key, or "(none)", and what it does.
fn hotkey_rows(app: &App) -> Vec<(String, String)> {
    let keys = &app.keys;
    let label = |hotkey: Hotkey| keys.label(hotkey).or_else(|| hotkey.command().map(|c| format!(":{c}")));
    let mut rows = Vec::new();
    for (hotkey, ..) in HOTKEYS {
        match hotkey {
            Hotkey::Window => {}
            _ => {
                if let Some(label) = label(*hotkey) {
                    rows.push((label, keys.fill(hotkey.about())));
                }
            }
        }
    }
    rows
}

fn dialog_view(f: &mut Frame, area: Rect, dialog: &Dialog, app: &App, t: &Theme, viewport: &mut DialogViewport) {
    match dialog {
        Dialog::Attention { tab, sel } => attention::overlay(f, area, app, *tab, *sel, t, viewport),
        Dialog::AttentionSend => attention::draft(f, area, app, t),
        Dialog::WorkQueue { tab, sel } => work_queue::overlay(f, area, app, *tab, *sel, t, viewport),
        Dialog::New { sel, model, effort } => dialogs::new_view(f, area, *sel, *model, *effort, app, t, viewport),
        Dialog::Confirm { text, action } => dialogs::confirm_view(f, area, text, action, t, viewport),
        Dialog::Picker { items, sel, filter, loading, .. } => {
            dialogs::resume_view(f, area, items, *sel, filter, *loading, app, t, viewport)
        }
        Dialog::Broadcast { text, targets, sel } => {
            dialogs::broadcast_view(f, area, text, targets, *sel, app, t, viewport)
        }
        Dialog::Branches { sel, filter } => overlays::branches_view(f, area, *sel, filter, app, t, viewport),
        Dialog::Ask { draft, route, scroll } => {
            overlays::ask_view(f, area, draft, *route, *scroll, app, t, viewport);
        }
        Dialog::Scans { sel } => overlays::scans_view(f, area, *sel, app, t, viewport),
        Dialog::Report { title, lines, scroll, confirm, waiting } => {
            overlays::report_view(f, area, title, lines, *scroll, confirm.is_some(), *waiting, app, t, viewport)
        }
        Dialog::Choose { pick, sel, filter, .. } => overlays::choose_view(f, area, pick, *sel, filter, t, viewport),
        Dialog::Errors { scroll } => errors_view(f, area, *scroll, app, t, viewport),
        Dialog::Machines => machines_view(f, area, app, t, viewport),
        Dialog::Usage => usage::usage_view(f, area, app, t, viewport),
        Dialog::Pair(pairing) => dialogs::pair_view(f, area, pairing, t, viewport),
        Dialog::Help => help::help_view(f, area, app, t, viewport),
    }
}

/// `:machines`: the stats table with every machine, the script machines the sidebar leaves out too. It redraws
/// from the latest readings while open. The phone draws the sidebar's table.
pub fn machines_lines(app: &App, width: u16, t: &Theme) -> Vec<Line<'static>> {
    machine_table(&machine_rows(app, true), width, t)
}

/// A reading as a meter and its figure, `figure_width` cells wide, or "–".
fn reading_cell(percent: Option<f64>, figure: String, figure_width: usize, t: &Theme) -> panel::Cell {
    match percent {
        Some(p) => {
            let mut cell = panel::meter(p, 12, t.level(p), t);
            cell.push(Span::styled(format!(" {figure:>figure_width$}"), plain().fg(t.level(p))));
            cell
        }
        None => vec![Span::styled("–", t.dim())],
    }
}

fn machines_view(f: &mut Frame, area: Rect, app: &App, t: &Theme, viewport: &mut DialogViewport) {
    let rows = machine_rows(app, true);
    let mut cells: Vec<Vec<panel::Cell>> = Vec::new();
    for row in &rows {
        let label = vec![Span::styled(row.label.clone(), t.bold())];
        cells.push(match &row.readings {
            Ok(r) => {
                let disk = r.disk.and_then(|d| Some((d, d.percent_used()?)));
                vec![
                    label,
                    reading_cell(r.cpu, r.cpu.map(|p| format!("{p:.0}%")).unwrap_or_default(), 4, t),
                    reading_cell(r.memory, r.memory.map(|p| format!("{p:.0}%")).unwrap_or_default(), 4, t),
                    match disk {
                        Some((d, full)) => {
                            let mut cell = panel::meter(full, 12, t.level(full), t);
                            cell.push(Span::styled(
                                format!(" {:>5}", resources::short_size(d.used)),
                                plain().fg(t.level(full)),
                            ));
                            cell.push(Span::styled(
                                format!(" of {}", resources::short_size(d.used + d.available)),
                                t.dim(),
                            ));
                            cell
                        }
                        None => vec![Span::styled("–", t.dim())],
                    },
                ]
            }
            Err(_) => vec![label],
        });
    }
    let columns = vec![
        panel::Column::left("Machine"),
        panel::Column::left("CPU"),
        panel::Column::left("Memory"),
        panel::Column::left("Disk used"),
    ];
    let table = panel::Table::measure(columns, cells.iter().map(Vec::as_slice));
    let mut panel = panel::Panel::new("Machines");
    let online = rows.iter().filter(|r| r.readings.is_ok()).count();
    panel.summary = vec![Span::raw(if online == rows.len() {
        format!("{} machines", rows.len())
    } else {
        format!("{online} of {} reading", rows.len())
    })];
    let width = panel::line_width(area, table.width().max(panel.chrome_width()));
    let table = table.fit(width);
    panel.top.push(table.header(width, t));
    let mut body = Vec::new();
    for (row, cells) in rows.iter().zip(cells) {
        match &row.readings {
            Ok(_) => body.push(table.row(cells, width, false, t)),
            Err(why) => body.push(table.lead(cells, vec![Span::styled(why.clone(), t.dim())], width, t)),
        }
    }
    let mut top = panel::search_lines(&mut body, viewport, width, t);
    top.append(&mut panel.top);
    panel.top = top;
    panel.body = body;
    panel::show(f, area, width, panel, viewport, t);
}

/// F2: the service's and the deck's errors, newest first, and nothing else. An error that recurred is one entry
/// with a dim `×N` after its text. Each entry shows its age, its text wraps under itself, and an error's
/// further lines (a failed dev server's last output) follow dim; a blank line separates entries.
fn errors_view(f: &mut Frame, area: Rect, scroll: usize, app: &App, t: &Theme, viewport: &mut DialogViewport) {
    use overlays::INSET;
    let now = util::now_secs();
    let mut list = app.error_list();
    list.sort_by_key(|e| std::cmp::Reverse(e.at));
    let mut errors: Vec<(LogEntry, usize)> = Vec::new();
    for entry in list {
        let times = 1 + entry.repeats as usize;
        if let Some((_, count)) = errors.iter_mut().find(|(seen, _)| seen.text == entry.text) {
            *count += times;
        } else {
            errors.push((entry, times));
        }
    }
    // Each entry's age, with how often it happened, against the right edge.
    let when = |e: &LogEntry, count: usize| {
        let ago = format!("{} ago", short_duration(now.saturating_sub(e.at)));
        if count > 1 {
            format!("×{count}  {ago}")
        } else {
            ago
        }
    };
    let when_width = errors.iter().map(|(e, count)| when(e, *count).width()).max().unwrap_or(0);
    let mark = 3;
    let natural = errors
        .iter()
        .flat_map(|(e, _)| e.text.lines().map(|line| line.width()))
        .max()
        .unwrap_or(0)
        .min(overlays::READING)
        + mark
        + when_width
        + 3;
    let mut panel = panel::Panel::new("Errors");
    panel.summary = match errors.len() {
        0 => Vec::new(),
        1 => vec![Span::styled("1 error", plain().fg(t.bad))],
        n => vec![Span::styled(format!("{n} errors"), plain().fg(t.bad))],
    };
    let width = panel::line_width(area, natural.max(panel.chrome_width()).max(40));
    let room = panel::text_width(width);
    let text_width = room.saturating_sub(mark + when_width + 3).max(16);
    let mut body: Vec<Line<'static>> = Vec::new();
    if app.service_version < 2 {
        body.extend(panel::note(
            "The running service is an older build: only the deck's own errors are listed until it is restarted.",
            width,
            t,
        ));
        body.push(Line::raw(""));
    }
    if errors.is_empty() {
        body.push(Line::from(vec![
            Span::raw(" ".repeat(INSET)),
            Span::styled("✓", t.bold().fg(t.ok)),
            Span::styled("  No errors", t.dim()),
        ]));
    }
    let indented =
        |part: String, style: Style| Line::from(vec![Span::raw(" ".repeat(INSET + mark)), Span::styled(part, style)]);
    for (k, (e, count)) in errors.iter().enumerate() {
        if k > 0 {
            body.push(Line::raw(""));
        }
        let mut lines = e.text.lines();
        let mut head = wrap_text(lines.next().unwrap_or_default(), text_width).into_iter();
        let first = head.next().unwrap_or_default();
        let gap = room.saturating_sub(mark + first.width() + when_width);
        body.push(Line::from(vec![
            Span::raw(" ".repeat(INSET)),
            Span::styled("✗  ", t.bold().fg(t.bad)),
            Span::styled(first, t.bold()),
            Span::raw(" ".repeat(gap)),
            Span::styled(format!("{:>when_width$}", when(e, *count)), t.dim()),
        ]));
        body.extend(head.map(|part| indented(part, t.bold())));
        // An error's further lines, such as a failed dev server's last
        // output, follow dim.
        for line in lines {
            body.extend(wrap_text(line, text_width).into_iter().map(|part| indented(part, t.dim())));
        }
    }
    panel.top = panel::search_lines(&mut body, viewport, width, t);
    panel.body = body;
    panel.scroll = Some(scroll);
    panel::show(f, area, width, panel, viewport, t);
}

/// `text` broken at spaces into lines of at most `width` cells, keeping
/// its leading indent on the first; a word longer than a line is split.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let text = text.replace('\t', "    ");
    let indent: String = text.chars().take_while(|c| *c == ' ').collect();
    let mut lines: Vec<String> = Vec::new();
    let mut line = if indent.width() < width { indent } else { String::new() };
    let mut empty = true;
    for word in text.split_whitespace() {
        for piece in split_width(word, width) {
            if !empty && line.width() + 1 + piece.width() > width {
                lines.push(std::mem::take(&mut line));
                empty = true;
            }
            if !empty {
                line.push(' ');
            }
            line.push_str(&piece);
            empty = false;
        }
    }
    if !empty || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// `word` in pieces of at most `width` cells.
fn split_width(word: &str, width: usize) -> Vec<String> {
    let mut pieces = vec![String::new()];
    for c in word.chars() {
        let last = pieces.last_mut().expect("never empty");
        if !last.is_empty() && last.width() + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) > width {
            pieces.push(String::new());
        }
        pieces.last_mut().expect("never empty").push(c);
    }
    pieces
}

/// `text` broken into lines of at most `width` cells at spaces.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The end of `s` within `width` cells, led by an ellipsis when cut: the end
/// of a line being typed is the part to keep in view.
fn keep_tail(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut tail = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        used += w;
        tail.push(c);
    }
    format!("…{}", tail.into_iter().rev().collect::<String>())
}

/// The command line either side of its cursor within `width` cells: the
/// cursor stays in view, and what follows it keeps up to half the room.
fn around_cursor(before: &str, after: &str, width: usize) -> (String, String) {
    let before = keep_tail(before, width - after.width().min(width / 2));
    let after = truncate_width(after, width.saturating_sub(before.width()));
    (before, after)
}

#[cfg(test)]
#[path = "tests/view.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/screenshots.rs"]
mod screenshots;
