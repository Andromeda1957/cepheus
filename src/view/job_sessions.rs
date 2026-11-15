//! Jobs render as text and shared chat records, never as a terminal emulator.
use super::*;
use crate::remote::chat::Item;

fn status_style(status: &str, t: &Theme) -> Style {
    match status {
        "failed" => t.bold().fg(t.needs),
        "finished" => t.bold().fg(t.finished),
        "working" => plain().fg(t.working),
        _ => t.dim(),
    }
}

/// Match other free-text surfaces' control stripping before any text reaches
/// ratatui. Newlines remain rows; escape/OSC/BEL sequences never reach stdout.
fn text_lines(text: &str, style: Style, out: &mut Vec<Line<'static>>) {
    for line in text.lines() {
        out.push(Line::styled(util::one_line(line), style));
    }
}

fn body(app: &App, id: u64, t: &Theme) -> Vec<Line<'static>> {
    let Some(job) = app.state.jobs.iter().find(|j| j.id == id) else {
        return vec![Line::raw("Job no longer listed.")];
    };
    let age = |at: u64| format!("{} ago", short_duration(util::now_secs().saturating_sub(at)));
    let mut lines = vec![
        Line::styled(
            util::one_line(&format!(
                "{} · {} · {}",
                job.status,
                app.host_label(job.host.as_deref()),
                job.provider.map_or("command", |p| p.label())
            )),
            status_style(&job.status, t),
        ),
        Line::raw(format!(
            "Created {} · started {} · finished {}",
            age(job.created_at),
            job.started_at.map_or_else(|| "—".into(), age),
            job.finished_at.map_or_else(|| "—".into(), age)
        )),
    ];
    if let Some(exit) = job.exit_code {
        lines.push(Line::raw(format!("Exit code: {exit}")));
    }
    if let Some(signal) = job.signal {
        lines.push(Line::raw(format!("Signal: {signal}")));
    }
    if !job.reason.is_empty() {
        text_lines(&format!("Reason: {}", job.reason), status_style(&job.status, t), &mut lines);
    }
    lines.push(Line::raw(""));
    if let Some(error) = &app.jobs.problem {
        text_lines(error, t.bold().fg(t.needs), &mut lines);
    }
    if app.jobs.raw {
        lines.push(Line::styled(
            format!(
                "Raw bytes {}–{} · {}",
                app.jobs.page_start,
                app.jobs.next_offset,
                if app.jobs.eof { "end of log" } else { "more / live · :job next" }
            ),
            t.dim(),
        ));
        text_lines(&app.jobs.text, plain(), &mut lines);
    } else {
        if app.jobs.items_truncated {
            lines.push(Line::styled(
                "Chat truncated to the bounded snapshot. :job raw and :job next read the full log.",
                t.bold().fg(t.needs),
            ));
        }
        if app.jobs.items.is_empty() {
            lines.push(Line::styled(
                if app.jobs.loading {
                    "Loading job output…"
                } else {
                    "No structured chat records. :job raw shows its output."
                },
                t.dim(),
            ));
        }
        for item in &app.jobs.items {
            let (label, text, style) = match item {
                Item::User { text, .. } => ("User", text.as_str(), t.accent()),
                Item::Assistant { text, .. } => ("Assistant", text.as_str(), plain()),
                Item::Thinking { text } => ("Thinking", text.as_str(), t.dim()),
                Item::Tool { name, summary, input, .. } => {
                    text_lines(&format!("Tool · {name} · {summary}"), t.accent(), &mut lines);
                    text_lines(input, t.dim(), &mut lines);
                    lines.push(Line::raw(""));
                    continue;
                }
                Item::Result { text, error, .. } => (
                    if *error { "Tool error" } else { "Tool result" },
                    text.as_str(),
                    if *error { t.bold().fg(t.needs) } else { t.dim() },
                ),
                Item::Notice { text } => ("Notice", text.as_str(), t.dim()),
                Item::Screen { text } => ("Output", text.as_str(), plain()),
            };
            lines.push(Line::styled(label, t.bold()));
            text_lines(text, style, &mut lines);
            lines.push(Line::raw(""));
        }
    }
    lines
}

pub(super) fn scroll_limit(app: &App) -> usize {
    let width = app.jobs.width.saturating_sub(2).max(1);
    let height = app.jobs.height.saturating_sub(3) as usize;
    Paragraph::new(body(app, app.jobs.id.unwrap_or(0), &app.theme))
        .wrap(Wrap { trim: false })
        .line_count(width)
        .saturating_sub(height)
}

pub(super) fn pane(f: &mut Frame, pane: Rect, app: &App, id: u64, t: &Theme) {
    let job = app.state.jobs.iter().find(|j| j.id == id);
    let title = job.map(|j| util::one_line(&j.title)).unwrap_or_default();
    // A signal-only receipt can still leave process custody unresolved.
    let stop_available = job.is_some_and(|job| {
        matches!(job.status.as_str(), "starting" | "working") || (job.status == "failed" && job.exit_code.is_none())
    });
    f.render_widget(Paragraph::new(format!(" Job #{id} · {title}")).style(t.bold()), Rect { height: 1, ..pane });
    if pane.height < 4 {
        return;
    }
    let output = Rect { x: pane.x + 1, y: pane.y + 1, width: pane.width.saturating_sub(2), height: pane.height - 3 };
    let paragraph = Paragraph::new(body(app, id, t)).wrap(Wrap { trim: false });
    let limit = paragraph.line_count(output.width).saturating_sub(output.height as usize);
    f.render_widget(paragraph.scroll((app.jobs.scroll.min(limit).min(u16::MAX as usize) as u16, 0)), output);
    let controls = vec![
        Line::raw(format!(
            " :job chat / raw · :job status{} · :job dismiss",
            if stop_available { " · :job stop" } else { "" }
        )),
        Line::styled(" Read-only · PgUp/PgDn scroll · :job next / prev page raw bytes · Enter refreshes", t.dim()),
    ];
    f.render_widget(Paragraph::new(controls), Rect { y: pane.bottom() - 2, height: 2, ..pane });
}
