//! Dot's conversation, drawn by the deck the way the local agents draw theirs: the owner's messages on a shaded
//! band after "›", Dot's replies after a bullet in its colour with rendered Markdown, failures as plain
//! failures, word-wrapped scrollback and a composer that grows with its text. Dot is always on in ChatGPT, so
//! nothing here says ready, working or done.
use std::borrow::Cow;

use super::markdown::{clean, markdown};
use super::*;
use crate::remote::chat::Item;
use crate::tui::dot_chat::Rendered;
use crate::wrap;
use ratatui::widgets::{Block, BorderType, Borders};

/// The composer grows with its text up to this many rows.
const COMPOSER_LINES: usize = 6;
/// A refused send shows in at most this many rows above the composer.
const PROBLEM_LINES: usize = 3;

/// A row on the owner's band, filled to the column's width.
fn banded(mut row: Vec<Span<'static>>, width: usize, band: Color) -> Line<'static> {
    let used: usize = row.iter().map(Span::width).sum();
    if used < width {
        row.push(Span::styled(" ".repeat(width - used), plain().bg(band)));
    }
    Line::from(row)
}

/// The owner's message, as the local agents show a prompt: "›" on a shaded
/// band. A message still being sent stays dim.
fn owner(rows: &mut Vec<Line<'static>>, text: &str, images: &[&str], sending: bool, width: usize, t: &Theme) {
    let band = t.selected_bg;
    let style = if sending { t.dim() } else { plain() }.bg(band);
    let mut lead = Some(Span::styled("› ", if sending { t.dim() } else { t.bold() }.bg(band)));
    let hang = [Span::styled("  ", style)];
    let mut lines: Vec<Span<'static>> =
        text.trim_end().lines().map(|line| Span::styled(clean(line).trim_end().to_string(), style)).collect();
    lines.extend(images.iter().map(|name| Span::styled(format!("▧ {}", clean(name)), t.dim().bg(band))));
    if lines.is_empty() {
        lines.push(Span::styled("", style));
    }
    for line in lines {
        let first = [lead.take().unwrap_or_else(|| hang[0].clone())];
        rows.extend(wrap::spans(&[line], &first, &hang, width).into_iter().map(|row| banded(row, width, band)));
    }
}

/// Dot's reply after a bullet in its colour, its Markdown rendered.
fn reply(rows: &mut Vec<Line<'static>>, text: &str, width: usize, t: &Theme) {
    let mark = Span::styled("● ", plain().fg(t.tool(Tool::Dynamic("dot"))));
    let indent = Span::raw("  ");
    let lines = markdown(text, t);
    if lines.is_empty() {
        rows.push(Line::from(mark));
        return;
    }
    for (n, line) in lines.into_iter().enumerate() {
        let mut lead = vec![if n == 0 { mark.clone() } else { indent.clone() }];
        lead.extend(line.lead);
        let mut hang = vec![indent.clone()];
        hang.extend(line.hang);
        rows.extend(wrap::spans(&line.spans, &lead, &hang, width).into_iter().map(Line::from));
    }
}

/// Something that failed, said as a failure.
fn failure(text: &str, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let lead = [Span::styled("✗ ", t.bold().fg(t.bad))];
    let hang = [Span::raw("  ")];
    let text = [Span::styled(util::one_line(text), plain().fg(t.bad))];
    wrap::spans(&text, &lead, &hang, width).into_iter().map(Line::from).collect()
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Today's date (`YYYY-MM-DD`) in this machine's time zone.
fn today() -> String {
    crate::work_queue::schedule::calendar(util::now_secs(), "local").map(|c| c.day).unwrap_or_default()
}

/// When a message was sent, as the phone writes it under each message: the time alone today, else the short
/// date before it, in this machine's time zone.
pub(super) fn sent_label(at_ms: u64, today: &str) -> Option<String> {
    let at = crate::work_queue::schedule::calendar(at_ms / 1000, "local").ok()?;
    let hour = match at.hour % 12 {
        0 => 12,
        hour => hour,
    };
    let time = format!("{hour}:{:02} {}", at.minute, if at.hour < 12 { "AM" } else { "PM" });
    if at.day == today {
        return Some(time);
    }
    let month = MONTHS.get(at.month.checked_sub(1)? as usize)?;
    let year = at.day.get(..4)?;
    if Some(year) == today.get(..4) {
        Some(format!("{month} {}, {time}", at.month_day))
    } else {
        Some(format!("{month} {}, {year}, {time}", at.month_day))
    }
}

/// A message's time on a dim row of its own under it, in line with its text.
fn sent(rows: &mut Vec<Line<'static>>, at: Option<u64>, today: &str, t: &Theme) {
    if let Some(label) = at.filter(|&at| at > 0).and_then(|at| sent_label(at, today)) {
        rows.push(Line::styled(format!("  {label}"), t.dim()));
    }
}

/// The conversation as rows `width` cells wide.
pub(super) fn body(app: &App, width: usize, t: &Theme, today: &str) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    let gap = |rows: &mut Vec<Line<'static>>| {
        if !rows.is_empty() {
            rows.push(Line::raw(""));
        }
    };
    for item in &app.dot.items {
        match item {
            Item::User { text, at, .. } => {
                gap(&mut rows);
                owner(&mut rows, text, &[], false, width, t);
                sent(&mut rows, *at, today, t);
            }
            Item::Assistant { text, at, .. } => {
                gap(&mut rows);
                reply(&mut rows, text, width, t);
                sent(&mut rows, *at, today, t);
            }
            Item::Notice { text } => {
                gap(&mut rows);
                rows.extend(failure(text, width, t));
            }
            _ => {}
        }
    }
    for submitted in &app.dot.submitted {
        gap(&mut rows);
        let images: Vec<&str> = submitted.images.iter().map(|image| image.name.as_str()).collect();
        owner(&mut rows, &submitted.text, &images, false, width, t);
    }
    if let Some(sending) = &app.dot.sending {
        gap(&mut rows);
        let images: Vec<&str> = sending.images.iter().map(|image| image.name.as_str()).collect();
        owner(&mut rows, &sending.text, &images, true, width, t);
    }
    rows
}

/// The draft as composer rows: "›" then the text, wrapped at words, with
/// one cell left for the cursor.
fn draft_rows(app: &App, width: usize, t: &Theme) -> Vec<Vec<Span<'static>>> {
    let typing = app.mode == Mode::Insert;
    let mut lead = Some(Span::styled("› ", if typing { t.accent() } else { t.dim() }));
    let hang = [Span::raw("  ")];
    let width = width.saturating_sub(1).max(1);
    if app.dot.draft.is_empty() {
        let placeholder = Span::styled(format!("Message {}", app.dot_name()), t.faint());
        return wrap::spans(&[placeholder], &[lead.take().unwrap()], &hang, width).into_iter().take(1).collect();
    }
    let mut rows = Vec::new();
    for line in app.dot.draft.split('\n') {
        let first = [lead.take().unwrap_or_else(|| hang[0].clone())];
        rows.extend(wrap::spans(&[Span::raw(clean(line))], &first, &hang, width));
    }
    rows
}

/// The conversation's column: the whole pane, one cell in from each side.
fn column(pane: Rect) -> (u16, u16) {
    (pane.x + 1, pane.width.saturating_sub(2))
}

struct Layout {
    body: Rect,
    problem: Rect,
    composer: Rect,
}

fn layout(pane: Rect, app: &App) -> Layout {
    let (x, width) = column(pane);
    let text = Rect { x: x + 2, width: width.saturating_sub(4).max(1), ..pane };
    let draft = draft_rows(app, text.width as usize, &app.theme).len().clamp(1, COMPOSER_LINES) as u16;
    let composer_h = (draft + 2 + u16::from(!app.dot.images.is_empty())).min(pane.height.saturating_sub(2));
    let problem = app.dot.problem.as_ref().map_or(0, |p| failure(p, text.width as usize, &app.theme).len());
    let problem_h = (problem.min(PROBLEM_LINES) as u16).min(pane.height.saturating_sub(composer_h + 2));
    let composer = Rect { x, y: pane.bottom().saturating_sub(composer_h), width, height: composer_h };
    let problem = Rect { y: composer.y.saturating_sub(problem_h), height: problem_h, ..text };
    let body = Rect { y: pane.y, height: problem.y.saturating_sub(pane.y + 1), ..text };
    Layout { body, problem, composer }
}

fn fresh(rendered: &Rendered, app: &App, width: u16, today: &str) -> bool {
    rendered.generation == app.dot.generation
        && rendered.width == width
        && rendered.theme == app.theme
        && rendered.day == today
}

/// Wrap the conversation once per change rather than on every frame, and
/// keep a reader who scrolled up on the same lines as rows arrive below.
pub(super) fn prepare(app: &mut App) {
    let width = layout(app.pane_rect(), app).body.width;
    let today = today();
    if app.dot.rendered.as_ref().is_some_and(|r| fresh(r, app, width, &today)) {
        return;
    }
    let rows = body(app, width as usize, &app.theme, &today);
    let before = app.dot.rendered.as_ref().filter(|old| old.width == width).map(|old| old.rows.len());
    if let Some(before) = before.filter(|_| app.dot.scroll > 0) {
        app.dot.scroll = (app.dot.scroll + rows.len()).saturating_sub(before);
    }
    let (generation, theme) = (app.dot.generation, app.theme.clone());
    app.dot.rendered = Some(Rendered { generation, width, theme, day: today, rows });
}

fn rows(app: &App, width: u16) -> Cow<'_, [Line<'static>]> {
    let today = today();
    match &app.dot.rendered {
        Some(rendered) if fresh(rendered, app, width, &today) => Cow::Borrowed(&rendered.rows),
        _ => Cow::Owned(body(app, width as usize, &app.theme, &today)),
    }
}

pub(super) fn scroll_limit(app: &App) -> usize {
    let area = layout(app.pane_rect(), app).body;
    rows(app, area.width).len().saturating_sub(area.height as usize)
}

pub(super) fn page(app: &App) -> usize {
    (layout(app.pane_rect(), app).body.height as usize).saturating_sub(2).max(1)
}

pub(super) fn pane(f: &mut Frame, pane: Rect, app: &App, t: &Theme) -> Option<(u16, u16, u8)> {
    if pane.height < 5 || pane.width < 12 {
        return None;
    }
    let area = layout(pane, app);
    let rows = rows(app, area.body.width);
    let limit = rows.len().saturating_sub(area.body.height as usize);
    let up = app.dot.scroll.min(limit);
    let top = limit - up;
    let shown = rows[top..rows.len().min(top + area.body.height as usize)].to_vec();
    f.render_widget(Paragraph::new(shown), area.body);
    if up > 0 {
        // As a scrolled session says it.
        let text = format!(" ↑ {up} / {} lines · End live ", rows.len());
        let w = (text.width() as u16).min(pane.width);
        let rect = Rect { x: pane.right() - w, y: pane.y, width: w, height: 1 };
        f.render_widget(Paragraph::new(text).style(t.tag(t.accent)), rect);
    }
    if let Some(problem) = app.dot.problem.as_ref().filter(|_| area.problem.height > 0) {
        f.render_widget(Paragraph::new(failure(problem, area.problem.width as usize, t)), area.problem);
    }
    let typing = app.mode == Mode::Insert;
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(if typing {
        t.accent()
    } else {
        t.dim()
    });
    let inner = block.inner(area.composer);
    let mut inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
    f.render_widget(block, area.composer);
    if !app.dot.images.is_empty() && inner.height > 1 {
        let images = app.dot.images.iter().map(|image| format!("▧ {}", clean(&image.name))).collect::<Vec<_>>();
        let images = truncate_width(&images.join("  "), inner.width as usize);
        f.render_widget(Paragraph::new(images).style(t.accent()), Rect { height: 1, ..inner });
        inner.y += 1;
        inner.height -= 1;
    }
    let draft = draft_rows(app, inner.width as usize, t);
    let visible = &draft[draft.len().saturating_sub((inner.height as usize).max(1))..];
    f.render_widget(Paragraph::new(visible.iter().cloned().map(Line::from).collect::<Vec<_>>()), inner);
    if !typing {
        return None;
    }
    let row = visible.len().saturating_sub(1) as u16;
    let col = if app.dot.draft.is_empty() {
        2
    } else {
        visible.last().map_or(0, |spans| spans.iter().map(Span::width).sum::<usize>()) as u16
    };
    Some((inner.x + col, inner.y + row, crate::screen::CURSOR_BEAM))
        .filter(|(x, y, _)| *x < inner.right().max(inner.x + 1) && *y < inner.bottom())
}

#[cfg(test)]
#[path = "../tests/dot_times.rs"]
mod times_tests;
