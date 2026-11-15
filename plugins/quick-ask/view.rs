//! F4: quick questions and their answers, newest at the bottom, and the question being typed. Answers are
//! rendered Markdown and the one being written shows as it comes. The window is centred and as tall as what it
//! holds, who answers is said in its header and the question is typed in a box at its foot.
use std::path::PathBuf;

use unicode_width::UnicodeWidthChar;

use super::*;
use crate::config::AskRoute;
use crate::edit::Draft;
use crate::tui::{AskRendered, Dialog};
use crate::view::markdown::{self, clean};
use crate::view::panel::{body_room, head_rows, input_box};
use crate::wrap;

/// The cells of conversation a line holds, for reading; the most rows the
/// question being typed takes.
const READ: usize = 100;
const INPUT_ROWS: usize = 6;
/// Before the question being typed, inside its box.
const PROMPT: &str = "› ";

/// A row of the conversation and the history entry it belongs to.
type Row = (Option<usize>, Line<'static>);

/// The window's rows' width on this screen.
fn window_width(area: Rect) -> usize {
    line_width(area, READ)
}

/// The conversation's width: between the window's insets.
fn text_width(area: Rect) -> usize {
    super::super::panel::text_width(window_width(area)).max(1)
}

/// "agy · gemini-3.8-flash-high", or the program alone with its default.
fn who(route: &AskRoute) -> String {
    if route.model.is_empty() {
        route.tool.clone()
    } else {
        format!("{} · {}", route.tool, route.model)
    }
}

fn conversation(asked: &ask::Asked) -> Option<&str> {
    asked.thread.as_ref().map(|thread| thread.id.as_str())
}

const INDENT: &str = "  ";

/// A question after "›", wrapped under itself, with `said` (who answered
/// and when) at the right of its first row.
#[allow(clippy::too_many_arguments)]
fn question_rows(
    rows: &mut Vec<Row>,
    owner: Option<usize>,
    question: &str,
    said: Vec<Span<'static>>,
    mark: Style,
    style: Style,
    width: usize,
) {
    let said_width = width_of(&said);
    let room = if said_width > 0 { width.saturating_sub(said_width + 3).max(12) } else { width };
    let mut lead = Some(Span::styled("› ", mark));
    let hang = [Span::raw(INDENT)];
    let mut said = Some(said).filter(|s| !s.is_empty());
    for line in question.trim().lines() {
        let first = [lead.take().unwrap_or_else(|| hang[0].clone())];
        let text = [Span::styled(clean(line).trim_end().to_string(), style)];
        for mut row in wrap::spans(&text, &first, &hang, room) {
            if let Some(said) = said.take() {
                row.push(Span::raw(" ".repeat(width.saturating_sub(width_of(&row) + said_width))));
                row.extend(said);
            }
            rows.push((owner, Line::from(row)));
        }
    }
}

/// An answer's rows: its Markdown drawn, or a failure said as one.
fn answer_rows(asked: &ask::Asked, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let indent = [Span::raw(INDENT)];
    if !asked.error {
        return markdown::rows(&asked.answer, &indent, &indent, width, t);
    }
    let style = plain().fg(t.bad);
    let mut rows = Vec::new();
    for line in asked.answer.lines().filter(|line| !line.trim().is_empty()) {
        let text = [Span::styled(clean(line), style)];
        rows.extend(wrap::spans(&text, &indent, &indent, width).into_iter().map(Line::from));
    }
    rows
}

/// A faint rule across the conversation with a word in its middle.
fn divider(label: &str, width: usize, t: &Theme) -> Line<'static> {
    let label = format!(" {label} ");
    let sides = width.saturating_sub(label.width());
    let left = sides / 2;
    Line::from(vec![
        Span::styled("─".repeat(left), t.faint()),
        Span::styled(label, t.dim()),
        Span::styled("─".repeat(sides - left), t.faint()),
    ])
}

/// The history with `answers` drawn, the answer being written and the
/// questions waiting, as rows `width` cells wide.
fn transcript(app: &App, answers: &[Vec<Line<'static>>], width: usize, t: &Theme) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let gap = |rows: &mut Vec<Row>| {
        if !rows.is_empty() {
            rows.push((None, Line::raw("")));
        }
    };
    let new_conversation = |rows: &mut Vec<Row>| {
        rows.push((None, divider("new conversation", width, t)));
        rows.push((None, Line::raw("")));
    };
    let now = util::now_secs();
    let start = app.ask_history.len().saturating_sub(ask::HISTORY_LEN);
    let history = &app.ask_history[start..];
    for (n, asked) in history.iter().enumerate() {
        let owner = Some(start + n);
        gap(&mut rows);
        if n > 0 && conversation(asked) != conversation(&history[n - 1]) {
            new_conversation(&mut rows);
        }
        let mut said = format!("{} · {} ago", asked.who, short_duration(now.saturating_sub(asked.at)));
        if asked.stopped {
            said.push_str(" · stopped");
        }
        question_rows(
            &mut rows,
            owner,
            &asked.question,
            vec![Span::styled(said, t.dim())],
            t.accent(),
            t.bold(),
            width,
        );
        rows.push((owner, Line::raw("")));
        rows.extend(answers.get(n).into_iter().flatten().map(|line| (owner, line.clone())));
    }
    if let Some(asking) = app.asking.as_ref().filter(|asking| !asking.discard) {
        gap(&mut rows);
        if !history.is_empty() && asking.continues.as_deref() != history.last().and_then(conversation) {
            new_conversation(&mut rows);
        }
        let said = vec![
            Span::styled(format!("{} ", spinner(app)), plain().fg(t.working)),
            Span::styled(format!("{} · {}s", asking.who, asking.since.elapsed().as_secs()), t.dim()),
        ];
        question_rows(&mut rows, None, &asking.question, said, t.accent(), t.bold(), width);
        rows.push((None, Line::raw("")));
        if let Some(activity) = &asking.activity {
            rows.push((None, Line::from(fit(vec![Span::raw(INDENT), Span::styled(activity.clone(), t.dim())], width))));
        }
        let indent = [Span::raw(INDENT)];
        rows.extend(markdown::rows(&asking.text, &indent, &indent, width, t).into_iter().map(|line| (None, line)));
    }
    for (question, _) in &app.ask_queue {
        gap(&mut rows);
        question_rows(&mut rows, None, question, vec![Span::styled("waiting", t.faint())], t.faint(), t.dim(), width);
    }
    rows
}

/// Before anything is asked: who answers what, from the config.
fn empty_rows(app: &App, width: usize, t: &Theme) -> Vec<Row> {
    let ask = &app.cfg.ask;
    let mut rows: Vec<Row> = vec![(None, Line::styled("Ask anything.", t.bold())), (None, Line::raw(""))];
    let routes = [
        ("general", who(&ask.general), "questions"),
        ("web", who(&ask.web), "lookups on the web"),
        ("code", who(&ask.code), "questions about the code, in the start folder"),
    ];
    let label = routes.iter().map(|(name, ..)| name.width()).max().unwrap_or(0);
    let model = routes.iter().map(|(_, who, _)| who.width()).max().unwrap_or(0);
    for (name, answers, what) in routes {
        let spans = vec![
            Span::styled(format!("{name:<label$}   "), t.accent()),
            Span::styled(format!("{answers:<model$}   "), plain()),
            Span::styled(what.to_string(), t.dim()),
        ];
        rows.push((None, Line::from(fit(spans, width))));
    }
    rows.push((None, Line::raw("")));
    let text = "A follow-up continues the conversation and /clear starts over; /general, /web, /code, /agy, /claude or /codex first chooses who answers.";
    rows.extend(wrap_words(text, width).into_iter().map(|line| (None, Line::styled(line, t.dim()))));
    rows
}

/// Draw the answers once per change and width rather than every frame, and
/// keep a reader who scrolled up on the same rows as an answer comes in.
pub fn prepare_ask(app: &mut App, area: Rect) {
    if !matches!(app.dialog, Some(Dialog::Ask { .. })) {
        return;
    }
    let width = text_width(area);
    let fresh = app
        .ask_rendered
        .as_ref()
        .is_some_and(|r| r.generation == app.ask_generation && r.width == width && r.theme == app.theme);
    let before = app.ask_rendered.as_ref().filter(|r| r.width == width).map(|r| r.rows.len());
    let answers = match app.ask_rendered.take() {
        Some(rendered) if fresh => rendered.answers,
        _ => {
            let start = app.ask_history.len().saturating_sub(ask::HISTORY_LEN);
            app.ask_history[start..].iter().map(|asked| answer_rows(asked, width, &app.theme)).collect()
        }
    };
    let rows = transcript(app, &answers, width, &app.theme);
    if let (Some(before), Some(Dialog::Ask { scroll, .. })) = (before, &mut app.dialog) {
        if *scroll > 0 {
            *scroll = (*scroll + rows.len()).saturating_sub(before);
        }
    }
    app.ask_rendered =
        Some(AskRendered { generation: app.ask_generation, width, theme: app.theme.clone(), answers, rows });
}

/// The question being typed in rows of at most `width` cells, broken
/// anywhere, and the row and column of the cursor.
fn draft_rows(text: &str, cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let mut rows = vec![String::new()];
    let mut used = 0;
    let mut at = (0, 0);
    for (i, c) in text.char_indices() {
        if c == '\n' {
            if i == cursor {
                at = (rows.len() - 1, used);
            }
            rows.push(String::new());
            used = 0;
            continue;
        }
        let w = c.width().unwrap_or(0);
        if used > 0 && used + w > width {
            rows.push(String::new());
            used = 0;
        }
        if i == cursor {
            at = (rows.len() - 1, used);
        }
        rows.last_mut().expect("a row").push(c);
        used += w;
    }
    if cursor >= text.len() {
        at = (rows.len() - 1, used);
    }
    (rows, at)
}

/// Who will answer the question being typed, or why it cannot be asked.
fn route_line(app: &App, input: &str, route: Option<Route>, t: &Theme) -> (String, Style) {
    let warn = plain().fg(t.warn);
    match ask::typed(input) {
        ask::Typed::Unknown(word) => (ask::unknown_command(word), warn),
        ask::Typed::Clear("") => ("Enter starts a new conversation and empties the history".into(), t.dim()),
        ask::Typed::Clear(_) => ("Enter starts a new conversation with this question".into(), t.dim()),
        ask::Typed::Question => {
            let start = PathBuf::from(&app.state.start_dir);
            let state = app.ask_dir.clone().unwrap_or_else(crate::paths::state_dir);
            match ask::plan(&app.cfg.ask, input, route, app.ask_thread.as_ref(), &start, &state) {
                Ok(plan) if plan.resume.is_some() => {
                    (format!("continues {} · /clear starts over", plan.who()), t.dim())
                }
                Ok(plan) => {
                    let words = format!("{} → {}", plan.route.label(), who(&plan.settings));
                    let words = match (plan.automatic, app.ask_thread.is_some()) {
                        (true, _) => format!("auto · {words}"),
                        (false, true) => format!("new conversation · {words}"),
                        (false, false) => words,
                    };
                    (words, t.dim())
                }
                Err(e) => (format!("{e:#}"), warn),
            }
        }
    }
}

/// The question being typed, as the rows of its box: the prompt before its
/// first row, and a faint invitation while it is empty.
fn question_box(shown: &[String], first: bool, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let rows = shown
        .iter()
        .enumerate()
        .map(|(n, row)| {
            let lead = if first && n == 0 {
                Span::styled(PROMPT, t.bold().fg(t.accent))
            } else {
                Span::raw(" ".repeat(PROMPT.width()))
            };
            let text = if row.is_empty() && first && shown.len() == 1 {
                Span::styled("Ask a question", t.faint())
            } else {
                Span::styled(row.clone(), t.bold())
            };
            vec![lead, text]
        })
        .collect();
    input_box(rows, width, t)
}

/// The window, as tall as its rows up to the screen; returns where the
/// terminal's cursor goes.
#[allow(clippy::too_many_arguments)]
pub fn ask_view(
    f: &mut Frame,
    area: Rect,
    draft: &Draft,
    route: Option<Route>,
    scroll: usize,
    app: &App,
    t: &Theme,
    viewport: &mut DialogViewport,
) -> Option<(u16, u16, u8)> {
    let width = window_width(area);
    let text = text_width(area);
    if area.height < 10 || text < 20 {
        return None;
    }
    let owned: Vec<Row>;
    let mut rows: &[Row] = match &app.ask_rendered {
        Some(rendered) if rendered.width == text => &rendered.rows,
        _ => {
            let start = app.ask_history.len().saturating_sub(ask::HISTORY_LEN);
            let answers: Vec<_> = app.ask_history[start..].iter().map(|asked| answer_rows(asked, text, t)).collect();
            owned = transcript(app, &answers, text, t);
            &owned
        }
    };
    let empty: Vec<Row>;
    if rows.is_empty() {
        empty = empty_rows(app, text, t);
        rows = &empty;
    }
    let (lines, (row, col)) = draft_rows(draft.text(), draft.cursor(), text.saturating_sub(4 + PROMPT.width()).max(1));
    let top = (row + 1).saturating_sub(INPUT_ROWS);
    let shown = &lines[top..lines.len().min(top + INPUT_ROWS)];
    let mut panel = Panel::new("Ask");
    let (words, style) = route_line(app, draft.text(), route, t);
    panel.summary = vec![Span::styled(words, style)];
    panel.bottom = vec![Line::raw("")];
    panel.bottom.extend(question_box(shown, top == 0, text, t));
    // The conversation keeps its end in view unless the reader scrolled up.
    let room = body_room(area, head_rows(&panel) + panel.bottom.len() + 1);
    let body = rows.len().clamp(1, room);
    let limit = rows.len().saturating_sub(body);
    let up = scroll.min(limit);
    let first = limit - up;
    // Ctrl-y copies the answer the last row shown belongs to.
    viewport.pick = if up > 0 { rows[first..first + body].iter().rev().find_map(|(owner, _)| *owner) } else { None };
    panel.body = rows
        .iter()
        .map(|(owner, line)| {
            let marked = up > 0 && owner.is_some() && *owner == viewport.pick;
            let mut spans = vec![if marked { Span::styled("▌ ", t.accent()) } else { Span::raw("  ") }];
            spans.extend(line.spans.iter().cloned());
            Line::from(spans).style(line.style)
        })
        .collect();
    panel.scroll = Some(first);
    let placed = show(f, area, width, panel, viewport, t);
    // The box starts a row below the body; its text after "│ › ".
    let x = placed.bottom.x + (INSET + 2 + PROMPT.width() + col) as u16;
    let y = placed.bottom.y + (2 + row - top) as u16;
    Some((x, y, crate::screen::CURSOR_BEAM))
        .filter(|&(x, y, _)| x < placed.rect.right() - 1 && y < placed.bottom.bottom().min(placed.rect.bottom() - 1))
}
