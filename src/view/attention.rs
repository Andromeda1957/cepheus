//! Attention rendering is metadata-only. Private text exists only after an explicit fetch.
use super::*;
use crate::tui::attention::{self as model, Tab};

pub(super) fn badges(app: &App, session: u64, t: &Theme) -> Vec<Span<'static>> {
    if !app.attention_available() {
        return vec![];
    }
    let snapshot = &app.state.attention;
    let mut spans = Vec::new();
    if let Some(n) = snapshot.queued.get(&session).filter(|n| **n > 0) {
        spans.push(Span::styled(format!(" · {n} queued"), t.dim()));
    }
    if snapshot.supervisor.audit.iter().any(|a| a.session == session) {
        spans.push(Span::styled(" · supervisor audit", t.dim()));
    }
    spans
}
fn safe(text: impl AsRef<str>) -> String {
    util::one_line(text.as_ref())
}

/// `:attention`: the idle deliveries and the supervisor, as two pages.
pub(super) fn overlay(
    f: &mut Frame,
    area: Rect,
    app: &App,
    tab: Tab,
    sel: usize,
    t: &Theme,
    viewport: &mut DialogViewport,
) {
    use panel::{line_width, note, section, show, tiles, Column, Panel, Table, Tile};
    let a = &app.state.attention;
    if let (Some(reader), Some((kind, text))) = (&app.attention.reader, &app.attention.private) {
        // The private body owns this viewport. Metadata list length and selection never move it.
        let mut panel = Panel::new("Private response");
        panel.summary = vec![Span::raw(safe(kind))];
        let natural = text.lines().map(|l| safe(l).width()).max().unwrap_or(0).min(overlays::READING);
        let width = line_width(area, natural.max(panel.chrome_width()));
        panel.top = note(&format!("{} · bounded to 64 Ki characters", safe(&reader.identity)), width, t);
        panel.top.push(Line::raw(""));
        for line in text.lines() {
            panel.body.extend(overlays::text_rows(&safe(line), plain(), width));
        }
        show(f, area, width, panel, viewport, t);
        return;
    }
    let now = util::now_secs();
    let ago = |at: u64| format!("{} ago", short_duration(now.saturating_sub(at)));
    let mut panel = Panel::new("Attention");
    let s = &a.supervisor;
    panel.summary = vec![Span::raw(format!("supervisor {}", if s.enabled { "on" } else { "off" }))];
    panel.tabs = vec![
        panel::Tab::new("Messages", a.messages.len().to_string()),
        panel::Tab::new("Supervisor", s.audit.len().to_string()),
    ];
    panel.tab = usize::from(tab == Tab::Supervisor);
    match tab {
        Tab::Messages => {
            let rows: Vec<Vec<panel::Cell>> = a
                .messages
                .iter()
                .map(|m| {
                    vec![
                        vec![Span::raw(safe(&m.id))],
                        vec![Span::raw(m.session.to_string())],
                        vec![Span::styled(model::label(m.state), t.accent())],
                        vec![Span::styled(m.host.as_deref().unwrap_or("local").to_string(), t.dim())],
                        vec![Span::styled(ago(m.created_at), t.dim())],
                        vec![Span::raw(safe(&m.reason))],
                    ]
                })
                .collect();
            let columns = vec![
                Column::left("Message"),
                Column::right("Session"),
                Column::left("State"),
                Column::left("Host"),
                Column::right("Created"),
                Column::left("Reason"),
            ];
            let table = Table::measure(columns, rows.iter().map(Vec::as_slice));
            let width = line_width(area, table.width().min(overlays::READING).max(panel.chrome_width()).max(60));
            let table = table.fit(width);
            if rows.is_empty() {
                panel.body = note("No idle deliveries. :send --idle opens a captured-session draft.", width, t);
            } else {
                panel.top.push(table.header(width, t));
                panel.body =
                    rows.into_iter().enumerate().map(|(i, cells)| table.row(cells, width, i == sel, t)).collect();
                panel.follow = Some(sel);
            }
            let drafts = app.attention.drafts.values().filter(|d| !d.accepted).count();
            if drafts > 0 {
                panel.body.push(Line::raw(""));
                panel.body.extend(note(
                    &format!("{drafts} retained local drafts · :send --idle N reopens the session's draft"),
                    width,
                    t,
                ));
            }
            show(f, area, width, panel, viewport, t);
        }
        Tab::Supervisor => {
            let state = if s.enabled { Span::styled("On", plain().fg(t.ok)) } else { Span::styled("Off", t.dim()) };
            let pending_style = if s.pending.is_empty() { plain() } else { plain().fg(t.needs) };
            let stats = [
                Tile { value: vec![state], label: "Supervisor".into() },
                Tile { value: vec![Span::raw(s.revision.to_string())], label: "Revision".into() },
                Tile { value: vec![Span::raw(s.rules.to_string())], label: "Rules".into() },
                Tile {
                    value: vec![Span::styled(s.pending.len().to_string(), pending_style)],
                    label: "Pending hosts".into(),
                },
            ];
            let rows: Vec<Vec<panel::Cell>> = s
                .audit
                .iter()
                .rev()
                .take(32)
                .map(|audit| {
                    vec![
                        vec![Span::styled(ago(audit.at), t.dim())],
                        vec![Span::raw(audit.session.to_string())],
                        vec![Span::raw(safe(&audit.category))],
                        vec![Span::raw(safe(audit.rule.as_deref().unwrap_or("—")))],
                    ]
                })
                .collect();
            let columns =
                vec![Column::right("When"), Column::right("Session"), Column::left("Category"), Column::left("Rule")];
            let table = Table::measure(columns, rows.iter().map(Vec::as_slice));
            let width =
                line_width(area, table.width().max(panel::tiles_width(&stats)).max(panel.chrome_width()).max(60));
            let table = table.fit(width);
            panel.top = tiles(&stats, width, t);
            panel.top.push(Line::raw(""));
            let mut body = note(
                "Off by default. Only the provider's proven allow-once prompt and current exact rules can authorize an answer; no model is used.",
                width,
                t,
            );
            if !s.pending.is_empty() {
                body.push(Line::raw(""));
                body.push(section(vec![Span::raw("Pending or offline")], Vec::new(), width, t));
                for host in &s.pending {
                    body.push(overlays::row(
                        vec![Span::styled(safe(host), plain().fg(t.needs))],
                        Vec::new(),
                        width,
                        false,
                        t,
                    ));
                }
            }
            body.push(Line::raw(""));
            body.push(section(vec![Span::raw("Audit")], vec![Span::raw(s.audit.len().to_string())], width, t));
            if rows.is_empty() {
                body.extend(note("Nothing audited yet.", width, t));
            } else {
                body.push(table.header(width, t));
                body.extend(rows.into_iter().map(|cells| table.row(cells, width, false, t)));
            }
            panel.body = body;
            show(f, area, width, panel, viewport, t);
        }
    }
}

/// The message to deliver when the captured session is next idle.
pub(super) fn draft(f: &mut Frame, area: Rect, app: &App, t: &Theme) {
    use panel::{line_width, note, show, Panel};
    let draft = app.attention.active_draft.as_ref().and_then(|k| app.attention.drafts.get(k));
    let mut panel = Panel::new("Idle message");
    let width = line_width(area, 80);
    panel.top = note("Delivered when the session is idle; the core safety checks still decide.", width, t);
    panel.top.push(Line::raw(""));
    if let Some(draft) = draft {
        panel.summary = vec![Span::raw(format!(
            "session {} · {}",
            draft.session,
            if draft.accepted {
                "accepted; draft retained for this receipt"
            } else if draft.request.is_some() {
                "awaiting acknowledgement; Enter will not duplicate"
            } else {
                "private draft"
            }
        ))];
        for line in draft.text.lines() {
            panel.body.extend(overlays::text_rows(&safe(line), plain(), width));
        }
        if let Some(error) = &draft.error {
            panel.body.push(Line::raw(""));
            panel.body.extend(overlays::text_rows(&safe(error), t.bold().fg(t.needs), width));
        }
    }
    let mut viewport = DialogViewport::default();
    show(f, area, width, panel, &mut viewport, t);
}
