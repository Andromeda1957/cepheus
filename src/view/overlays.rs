//! The overlays the commands open, drawn as F2 is: every branch on every host (F3), quick questions (F4), the
//! security scans, reports and scripts' pickers; and the sections and badges scripts add.
//!
//! The windows share one frame: text inset two columns behind the selection bar, groups under a bold heading
//! with a faint rule, a search field only during `/` search, columns aligned from their widest cell and the
//! position in a long list on the right border.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::panel::{
    line_width, note, search_rows, section, show, tiles, tiles_width, Cell, Column, Panel, Tab, Table, Tile,
};
use super::{fit, pad_line, plain, spinner, tool_icon, wrap_text, wrap_words};
use crate::ask::{self, Route};
use crate::lua::{ItemKind, Pick, Section, Segment};
use crate::protocol::{BranchInfo, SessionInfo};
use crate::theme::Theme;
use crate::tui::{choose_matches, App, BranchRow, DialogViewport};
use crate::util::{self, short_duration};

/// A theme style a script named; anything unknown is plain.
pub fn named_style(name: &str, t: &Theme) -> Style {
    match name {
        "accent" => t.accent(),
        "dim" => t.dim(),
        "faint" => t.faint(),
        "bold" => t.bold(),
        "ok" => plain().fg(t.ok),
        "warn" => plain().fg(t.warn),
        "bad" => plain().fg(t.bad),
        "working" => plain().fg(t.working),
        "needs" => plain().fg(t.needs),
        "finished" => plain().fg(t.finished),
        "task" => plain().fg(t.task),
        "review" => plain().fg(t.review),
        "claude" => plain().fg(t.claude),
        "codex" => plain().fg(t.codex),
        "antigravity" => plain().fg(t.antigravity),
        "grok" => plain().fg(t.grok),
        "opencode" => plain().fg(t.opencode),
        "strong" => t.bold().fg(t.accent),
        _ => plain(),
    }
}

pub fn segment_spans(parts: &[Segment], t: &Theme) -> Vec<Span<'static>> {
    parts.iter().map(|p| Span::styled(p.text.clone(), named_style(&p.style, t))).collect()
}

/// A script's section as sidebar lines: its title, then its lines.
pub fn section_lines(section: &Section, width: u16, t: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if !section.title.is_empty() {
        out.push(Line::from(fit(vec![Span::styled(format!(" {}", section.title), t.bold())], width as usize)));
    }
    for line in &section.lines {
        out.push(Line::from(fit(segment_spans(line, t), width as usize)));
    }
    out
}

/// Where a window's text starts: the selection bar, then a space.
pub(super) const INSET: usize = 2;

pub(super) fn width_of(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// One row: `left` from the inset and `right` against the right edge; the
/// selected row has the accent bar and the selection background.
pub(super) fn row(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
    selected: bool,
    t: &Theme,
) -> Line<'static> {
    let right_width = width_of(&right);
    let room = width.saturating_sub(INSET + 1 + if right_width > 0 { right_width + 2 } else { 0 });
    let mut spans = vec![Span::styled(if selected { "▌ " } else { "  " }, t.accent())];
    let left = fit(left, room);
    let gap = room.saturating_sub(width_of(&left));
    spans.extend(left);
    if right_width > 0 {
        spans.push(Span::raw(" ".repeat(gap + 2)));
        spans.extend(right);
    }
    pad_line(spans, width as u16, selected.then_some(t.selected_bg))
}

/// The search field of a list that typing filters, with how many rows it
/// keeps on the right.
pub(super) fn search_line(filter: &str, count: &str, width: usize, t: &Theme) -> Line<'static> {
    let mut left = vec![Span::styled("/ ", t.accent())];
    if filter.is_empty() {
        left.push(Span::styled("▏", t.accent()));
        left.push(Span::styled("type to filter", t.faint()));
    } else {
        left.push(Span::styled(filter.to_string(), t.bold()));
        left.push(Span::styled("▏", t.accent()));
    }
    row(left, vec![Span::styled(count.to_string(), t.dim())], width, false, t)
}

/// Where the window is in a list taller than it, drawn over the right
/// border beside the rows from `top` for `height` rows.
pub(super) fn scrollbar(f: &mut Frame, rect: Rect, top: u16, height: u16, total: usize, offset: usize, t: &Theme) {
    let shown = height as usize;
    if total <= shown || shown == 0 || rect.width < 2 {
        return;
    }
    let thumb = (shown * shown).div_ceil(total).clamp(1, shown);
    let hidden = total - shown;
    let start = (offset.min(hidden) * (shown - thumb) + hidden / 2) / hidden;
    let x = rect.right() - 1;
    let buf = f.buffer_mut();
    for y in start..start + thumb {
        if let Some(cell) = buf.cell_mut((x, top + y as u16)) {
            cell.set_symbol("┃").set_style(t.bold().fg(t.accent));
        }
    }
}

/// The agents on a branch in a fixed column: one is named; several are
/// counted by program, in the order they appear.
fn agents_cell(sessions: &[&SessionInfo], width: usize, t: &Theme) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    match sessions {
        [] => {}
        [one] => {
            let style = plain().fg(t.tool(one.tool));
            spans.push(Span::styled(format!("{} ", tool_icon(one.tool)), style));
            spans.push(Span::styled(one.title.clone(), style));
        }
        several => {
            let mut counts: Vec<(crate::protocol::Tool, usize)> = Vec::new();
            for s in several {
                match counts.iter_mut().find(|(tool, _)| *tool == s.tool) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((s.tool, 1)),
                }
            }
            for (i, (tool, n)) in counts.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled(format!("{} {n}", tool_icon(tool)), plain().fg(t.tool(tool))));
            }
        }
    }
    fit(spans, width)
}

/// F3: every branch on every host, the agents on each, newest first after the main branch. Columns line up from
/// their widest cell, and branches no worktree has follow the rest under their own heading.
pub fn branches_view(
    f: &mut Frame,
    area: Rect,
    sel: usize,
    filter: &str,
    app: &App,
    t: &Theme,
    viewport: &mut DialogViewport,
) {
    let rows: Vec<BranchRow> = app.branch_rows(filter);
    let total = if filter.is_empty() { rows.len() } else { app.branch_rows("").len() };
    let now = util::now_secs();
    let counts = |info: &BranchInfo| {
        let mut spans = Vec::new();
        if let Some(a) = info.ahead.filter(|a| *a > 0) {
            spans.push(Span::styled(format!("⇡{a}"), plain().fg(t.warn)));
        }
        if let Some(b) = info.behind.filter(|b| *b > 0) {
            if !spans.is_empty() {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(format!("⇣{b}"), t.dim()));
        }
        spans
    };
    let cells = |branch: &BranchRow| -> Vec<Cell> {
        let info = branch.info;
        let name_style = if info.main {
            t.bold().fg(t.accent)
        } else if info.path.is_empty() {
            t.dim()
        } else {
            plain().fg(t.task)
        };
        let age = info.committed_at.map(|at| short_duration(now.saturating_sub(at))).unwrap_or_default();
        // A branch with nothing of its own shows the commit it shares.
        let subject_style = if info.ahead == Some(0) && !info.main { t.faint() } else { t.dim() };
        vec![
            vec![Span::styled(info.branch.clone(), name_style)],
            counts(info),
            vec![Span::styled(age, t.dim())],
            agents_cell(&branch.sessions, 28, t),
            vec![Span::styled(info.subject.clone().unwrap_or_default(), subject_style)],
        ]
    };
    let all: Vec<Vec<Cell>> = rows.iter().map(cells).collect();
    let columns = vec![
        Column::left("Branch"),
        Column::left("vs main"),
        Column::right("Age"),
        Column::left("Agents"),
        Column::left("Last commit"),
    ];
    let table = Table::measure(columns, all.iter().map(Vec::as_slice));
    let with_agents = rows.iter().filter(|r| !r.sessions.is_empty()).count();
    let mut panel = Panel::new("Branches");
    panel.summary = vec![Span::raw(format!("{total} branches · {with_agents} with agents"))];
    let width = line_width(area, table.width().max(panel.chrome_width()));
    let table = table.fit(width);
    if viewport.search.is_some() {
        let count = if filter.is_empty() { format!("{total}") } else { format!("{} of {total}", rows.len()) };
        panel.top.extend(search_rows(filter, &count, width, t));
    }
    panel.top.push(table.header(width, t));
    let several_hosts = !app.hosts.is_empty();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_line = 0;
    let mut host: Option<Option<String>> = None;
    let mut detached = false;
    for (i, (branch, mut cells)) in rows.iter().zip(all).enumerate() {
        if several_hosts && host.as_ref() != Some(&branch.host) {
            host = Some(branch.host.clone());
            detached = false;
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            let here = rows.iter().filter(|r| r.host == branch.host).count();
            let label = app.host_label(branch.host.as_deref());
            lines.push(section(vec![Span::raw(label)], vec![Span::raw(format!("{here}"))], width, t));
        }
        if !detached && branch.detached() {
            detached = true;
            let here = rows.iter().filter(|r| r.host == branch.host && r.detached()).count();
            lines.push(Line::raw(""));
            lines.push(section(
                vec![Span::styled("Not checked out", t.dim())],
                vec![Span::raw(format!("{here}"))],
                width,
                t,
            ));
        }
        let selected = i == sel;
        if selected {
            selected_line = lines.len();
            for span in &mut cells[0] {
                span.style = span.style.add_modifier(Modifier::BOLD);
            }
        }
        lines.push(table.row(cells, width, selected, t));
    }
    if rows.is_empty() {
        let why = if app.state.branches.is_empty() && app.hosts.iter().all(|h| h.stats.is_none()) {
            "Reading the branches… the service lists them every 15 seconds."
        } else {
            "No branch matches. Backspace widens the search."
        };
        lines.extend(note(why, width, t));
    }
    panel.body = lines;
    panel.follow = Some(selected_line);
    show(f, area, width, panel, viewport, t);
}

/// Who answers, as the quick-question overlay says it.
#[path = "../../plugins/quick-ask/view.rs"]
mod quick_ask;
pub use quick_ask::{ask_view, prepare_ask};
#[path = "../../plugins/security-scans/view.rs"]
mod security_scans;
pub use security_scans::scans_view;

/// The widest a window of running text grows before it wraps.
pub(super) const READING: usize = 116;

/// Text at the inset in a row `width` wide, wrapped under its own indent.
pub(super) fn text_rows(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let text = text.replace('\t', "    ");
    let indent = text.chars().take_while(|c| *c == ' ').count().min(super::panel::text_width(width) / 2);
    let lead = " ".repeat(INSET + indent);
    wrap_text(text.trim_start(), super::panel::text_width(width).saturating_sub(indent).max(1))
        .into_iter()
        .map(|line| Line::from(vec![Span::raw(lead.clone()), Span::styled(line, style)]))
        .collect()
}

/// A confirmation's choices, drawn as buttons with their keys.
pub(super) fn buttons(choices: &[(&str, &str)], t: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(INSET))];
    for (i, (key, label)) in choices.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        let bg = if i == 0 { t.selected_bg } else { t.surface };
        spans.push(Span::styled(format!(" {key} "), t.bold().fg(t.accent).bg(bg)));
        spans.push(Span::styled(format!("{label} "), if i == 0 { t.bold().bg(bg) } else { t.dim() }));
    }
    Line::from(spans)
}

/// Lines to read: install plans and results, skills, hosts, options.
#[allow(clippy::too_many_arguments)]
pub fn report_view(
    f: &mut Frame,
    area: Rect,
    title: &str,
    lines: &[String],
    scroll: usize,
    confirm: bool,
    waiting: usize,
    app: &App,
    t: &Theme,
    viewport: &mut DialogViewport,
) {
    let mut panel = Panel::new(title);
    if waiting > 0 {
        panel.summary = vec![Span::styled(format!("{} waiting for {waiting} host(s)", spinner(app)), t.dim())];
    }
    let natural = lines.iter().map(|line| line.width()).max().unwrap_or(0).min(READING);
    let width = line_width(area, natural.max(panel.chrome_width()));
    let mut body: Vec<Line<'static>> = Vec::new();
    for line in lines {
        let style = if line.starts_with('✓') {
            t.bold().fg(t.ok)
        } else if line.starts_with('✗') {
            t.bold().fg(t.needs)
        } else {
            plain()
        };
        if line.trim().is_empty() {
            body.push(Line::raw(""));
        } else {
            body.extend(text_rows(line, style, width));
        }
    }
    panel.top = super::panel::search_lines(&mut body, viewport, width, t);
    if confirm {
        panel.bottom = vec![Line::raw(""), buttons(&[("y", "Do it"), ("n", "Cancel")], t)];
    }
    panel.body = body;
    panel.scroll = Some(scroll);
    show(f, area, width, panel, viewport, t);
}

/// Which table each of a picker's rows lines up in: 0 for the picker's own
/// columns, else one per heading that names its own; and those tables.
fn pick_tables(pick: &Pick, t: &Theme) -> (Vec<usize>, Vec<Table>) {
    let column = |c: &crate::lua::PickColumn| {
        if c.right {
            Column::right(c.title.clone())
        } else {
            Column::left(c.title.clone())
        }
    };
    let mut specs: Vec<Vec<Column>> = vec![pick.columns.iter().map(column).collect()];
    let mut owner = Vec::with_capacity(pick.items.len());
    let mut current = 0;
    for item in &pick.items {
        if item.kind == ItemKind::Heading {
            current = if item.columns.is_empty() {
                0
            } else {
                specs.push(item.columns.iter().map(column).collect());
                specs.len() - 1
            };
        }
        owner.push(current);
    }
    let tables = specs
        .into_iter()
        .enumerate()
        .map(|(n, columns)| {
            let rows: Vec<Vec<Cell>> = pick
                .items
                .iter()
                .zip(&owner)
                .filter(|(item, &o)| o == n && item.kind != ItemKind::Heading && !item.cells.is_empty())
                .map(|(item, _)| item.cells.iter().map(|cell| segment_spans(cell, t)).collect())
                .collect();
            Table::measure(columns, rows.iter().map(Vec::as_slice))
        })
        .collect();
    (owner, tables)
}

/// A script's picker (`deck.ui.pick`): choices, tables, figures and pages,
/// or a panel of headings and text, sized to what it shows. `/` opens live
/// search in either form.
pub fn choose_view(
    f: &mut Frame,
    area: Rect,
    pick: &Pick,
    sel: usize,
    filter: &str,
    t: &Theme,
    viewport: &mut DialogViewport,
) {
    let matches = choose_matches(&pick.items, filter);
    let choosable = |i: &usize| pick.items[*i].kind == ItemKind::Choice;
    let (owner, tables) = pick_tables(pick, t);
    let stats: Vec<Tile> =
        pick.stats.iter().map(|s| Tile { value: segment_spans(&s.value, t), label: s.label.clone() }).collect();
    let mut panel = Panel::new(pick.title.clone());
    if !pick.subtitle.is_empty() {
        panel.summary = vec![Span::raw(pick.subtitle.clone())];
    }
    panel.tabs = pick.tabs.iter().map(|tab| Tab::new(tab.label.clone(), tab.count.clone())).collect();
    panel.tab = pick.tab;
    // The window fits every row, so a short list is not a tall empty box, and keeps that size while search
    // filters it.
    let rows_width = pick
        .items
        .iter()
        .zip(&owner)
        .map(|(item, &o)| {
            let right = width_of(&segment_spans(&item.right, t));
            let text = width_of(&segment_spans(&item.text, t)) + if right > 0 { right + 2 } else { 0 };
            if item.cells.is_empty() {
                text
            } else {
                tables[o].width()
            }
        })
        .max()
        .unwrap_or(0);
    let natural = rows_width.max(tiles_width(&stats)).max(panel.chrome_width());
    let width = line_width(area, natural);
    let tables: Vec<Table> = tables.into_iter().map(|table| table.fit(width)).collect();
    if !stats.is_empty() {
        panel.top.extend(tiles(&stats, width, t));
        panel.top.push(Line::raw(""));
    }
    let searching = viewport.search.is_some();
    if searching {
        let total = pick.items.iter().filter(|item| item.kind == ItemKind::Choice).count();
        let kept = matches.iter().filter(|i| choosable(i)).count();
        let count = if filter.is_empty() { format!("{total}") } else { format!("{kept} of {total}") };
        panel.top.extend(search_rows(filter, &count, width, t));
    }
    // The picker's own columns stay put over its rows, once, with any notes
    // that lead the list kept above them. Columns without titles line up a
    // label and its value, with no header.
    let titled = |table: &Table| table.columns.iter().any(|c| !c.title.is_empty());
    let mut lead = 0;
    if titled(&tables[0]) && pick.items.iter().zip(&owner).any(|(item, &o)| o == 0 && !item.cells.is_empty()) {
        lead = matches
            .iter()
            .take_while(|&&i| pick.items[i].kind == ItemKind::Text && pick.items[i].cells.is_empty())
            .count();
        for &i in &matches[..lead] {
            let item = &pick.items[i];
            panel.top.push(row(segment_spans(&item.text, t), segment_spans(&item.right, t), width, false, t));
        }
        if lead > 0 {
            panel.top.push(Line::raw(""));
        }
        panel.top.push(tables[0].header(width, t));
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_line = 0;
    for (pos, &i) in matches.iter().enumerate().skip(lead) {
        let item = &pick.items[i];
        let right = segment_spans(&item.right, t);
        let table = &tables[owner[i]];
        let selected = item.kind == ItemKind::Choice && pos == sel;
        match item.kind {
            ItemKind::Heading => {
                if !lines.is_empty() {
                    lines.push(Line::raw(""));
                }
                lines.push(section(segment_spans(&item.text, t), right, width, t));
                if owner[i] != 0 && titled(table) {
                    lines.push(table.header(width, t));
                }
            }
            _ if !item.cells.is_empty() => {
                if selected {
                    selected_line = lines.len();
                }
                let mut cells: Vec<Cell> = item.cells.iter().map(|cell| segment_spans(cell, t)).collect();
                if let (true, Some(first)) = (selected, cells.first_mut()) {
                    for span in first {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                }
                lines.push(table.row(cells, width, selected, t));
            }
            _ => {
                if selected {
                    selected_line = lines.len();
                }
                let mut text = segment_spans(&item.text, t);
                if selected {
                    for span in &mut text {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                }
                lines.push(row(text, right, width, selected, t));
            }
        }
    }
    if matches.is_empty() {
        lines.extend(note("Nothing matches. Backspace widens the search.", width, t));
    }
    if matches.iter().any(choosable) {
        panel.follow = Some(selected_line);
    } else {
        // Nothing to choose: the arrows scroll the panel.
        panel.scroll = Some(sel);
    }
    panel.body = lines;
    show(f, area, width, panel, viewport, t);
}
