//! `:usage`: what each program's models have left of their limits, every window of every used model. The
//! sidebar keeps a usage part only when the config lists it.

use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::panel::{line_width, meter, note, section, show, tiles, tiles_width, Cell, Column, Panel, Table, Tile};
use super::plain;
use crate::protocol::{Usage, UsageWindow};
use crate::theme::Theme;
use crate::tui::{App, DialogViewport};
use crate::util::{self, short_duration};

/// The cells of a window's bar.
const BAR: usize = 20;

/// A program's windows, model by model.
struct Program<'a> {
    name: &'static str,
    color: Color,
    windows: Vec<&'a UsageWindow>,
}

fn programs<'a>(usage: &'a Usage, t: &Theme) -> Vec<Program<'a>> {
    let claude = usage.claude.iter().chain(&usage.claude_models).collect();
    [
        Program { name: "Claude", color: t.claude, windows: claude },
        Program { name: "Codex", color: t.codex, windows: usage.codex.iter().collect() },
        Program { name: "Grok", color: t.grok, windows: usage.grok.iter().collect() },
        Program { name: "Gemini", color: t.antigravity, windows: usage.gemini.iter().collect() },
    ]
    .into_iter()
    .collect()
}

/// A window's model and its own name: `Fable wk` is Fable's week, `5h` the
/// program's own five hours.
fn split<'a>(program: &'a str, label: &'a str) -> (&'a str, &'a str) {
    label.rsplit_once(' ').unwrap_or((program, label))
}

fn used(w: &UsageWindow) -> Option<f64> {
    w.used_percent.filter(|u| u.is_finite())
}

/// The windows of the models that have been used: a model whose every window reports nothing used stays out, as
/// in the sidebar.
fn used_windows<'a>(program: &Program<'a>) -> Vec<&'a UsageWindow> {
    let models: Vec<&str> = program
        .windows
        .iter()
        .filter(|w| used(w).is_some_and(|u| u > 0.0))
        .map(|w| split(program.name, &w.label).0)
        .collect();
    program.windows.iter().copied().filter(|w| models.contains(&split(program.name, &w.label).0)).collect()
}

fn left_cell(w: &UsageWindow, t: &Theme) -> Cell {
    match used(w) {
        Some(u) => {
            let left = 100.0 - u.clamp(0.0, 100.0);
            let mut cell = meter(left, BAR, t.level(u), t);
            cell.push(Span::styled(format!(" {left:>3.0}%"), plain().fg(t.level(u))));
            cell
        }
        None => vec![Span::styled("not reported", t.dim())],
    }
}

/// The usage window.
pub(super) fn usage_view(f: &mut Frame, area: Rect, app: &App, t: &Theme, viewport: &mut DialogViewport) {
    let now = util::now_secs();
    let mut panel = Panel::new("Usage");
    let shown: Vec<(Program, Vec<&UsageWindow>)> = programs(&app.state.usage, t)
        .into_iter()
        .map(|p| {
            let windows = used_windows(&p);
            (p, windows)
        })
        .filter(|(_, windows)| !windows.is_empty())
        .collect();
    let updated = shown.iter().flat_map(|(_, ws)| ws.iter().map(|w| w.updated_at)).max();
    if let Some(at) = updated {
        panel.summary = vec![Span::raw(format!("updated {} ago", short_duration(now.saturating_sub(at))))];
    }
    // Each program's tightest window.
    let stats: Vec<Tile> = shown
        .iter()
        .filter_map(|(p, windows)| {
            let tightest =
                windows.iter().filter_map(|w| used(w).map(|u| (u, *w))).max_by(|a, b| a.0.total_cmp(&b.0))?;
            let (u, w) = tightest;
            let resets = w.resets_at.map(|at| format!(" · resets in {}", short_duration(at.saturating_sub(now))));
            Some(Tile {
                value: vec![Span::styled(format!("{:.0}% left", 100.0 - u.clamp(0.0, 100.0)), plain().fg(t.level(u)))],
                label: format!("{} {}{}", p.name, split(p.name, &w.label).1, resets.unwrap_or_default()),
            })
        })
        .collect();
    let rows: Vec<(usize, Vec<Cell>)> = shown
        .iter()
        .enumerate()
        .flat_map(|(n, (p, windows))| {
            windows.iter().map(move |w| {
                let (model, window) = split(p.name, &w.label);
                let resets = w.resets_at.map_or_else(
                    || vec![Span::styled("–", t.dim())],
                    |at| vec![Span::styled(format!("in {}", short_duration(at.saturating_sub(now))), t.dim())],
                );
                (
                    n,
                    vec![
                        vec![Span::styled(model.to_string(), t.bold().fg(p.color))],
                        vec![Span::styled(window.to_string(), t.dim())],
                        left_cell(w, t),
                        resets,
                    ],
                )
            })
        })
        .collect();
    let columns = vec![Column::left("Model"), Column::left("Window"), Column::left("Left"), Column::right("Resets")];
    let table = Table::measure(columns, rows.iter().map(|(_, cells)| cells.as_slice()));
    let width = line_width(area, table.width().max(tiles_width(&stats)).max(panel.chrome_width()).max(56));
    let table = table.fit(width);
    if rows.is_empty() {
        panel.body = note(
            "No usage reported yet. Each program's limits show here once a session of it has used some.",
            width,
            t,
        );
        show(f, area, width, panel, viewport, t);
        return;
    }
    panel.top = tiles(&stats, width, t);
    panel.top.push(Line::raw(""));
    panel.top.push(table.header(width, t));
    let mut body = Vec::new();
    let mut last = None;
    for (n, cells) in rows {
        if last != Some(n) {
            if last.is_some() {
                body.push(Line::raw(""));
            }
            let (p, windows) = &shown[n];
            let count = if windows.len() == 1 { "1 window".to_string() } else { format!("{} windows", windows.len()) };
            body.push(section(vec![Span::styled(p.name, plain().fg(p.color))], vec![Span::raw(count)], width, t));
            last = Some(n);
        }
        body.push(table.row(cells, width, false, t));
    }
    panel.body = body;
    show(f, area, width, panel, viewport, t);
}

/// The usage window as plain text, for the phone.
pub fn usage_text(app: &App) -> Vec<String> {
    let now = util::now_secs();
    let mut lines = Vec::new();
    for p in programs(&app.state.usage, &app.theme) {
        for w in used_windows(&p) {
            let (model, window) = split(p.name, &w.label);
            let left =
                used(w).map_or("not reported".to_string(), |u| format!("{:.0}% left", 100.0 - u.clamp(0.0, 100.0)));
            let resets = w
                .resets_at
                .map(|at| format!(" · resets in {}", short_duration(at.saturating_sub(now))))
                .unwrap_or_default();
            lines.push(format!("{model} {window}: {left}{resets}"));
        }
    }
    if lines.is_empty() {
        lines.push("No usage reported yet.".into());
    }
    lines
}
