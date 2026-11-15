//! Metadata-only queue/source/chore overlays. No reads or mutations are launched here.
use super::*;
use crate::tui::work_queue::Tab;

fn safe(text: impl AsRef<str>) -> String {
    util::one_line(text.as_ref())
}

/// Whether queued work is dispatched, and its style.
fn policy_status(queue: &crate::work_queue::Snapshot, t: &Theme) -> Span<'static> {
    if queue.enabled {
        Span::styled("dispatch on", plain().fg(t.ok))
    } else if queue.pause_effective {
        Span::styled("paused · :queue enable", plain().fg(t.warn))
    } else if queue.pause_pending.is_empty() {
        Span::styled("pausing · awaiting executor confirmation", plain().fg(t.warn))
    } else {
        Span::styled(format!("pausing · awaiting {}", queue.pause_pending.join(", ")), plain().fg(t.warn))
    }
}

/// "5m ago", or "in 3h" for a time still to come.
fn when(at: u64, now: u64) -> String {
    if at > now {
        format!("in {}", short_duration(at - now))
    } else {
        format!("{} ago", short_duration(now - at))
    }
}

/// `:queue`: the work queue, the task sources and the chores, as pages; the
/// selected row's details under it.
pub(super) fn overlay(
    f: &mut Frame,
    area: Rect,
    app: &App,
    tab: Tab,
    sel: usize,
    t: &Theme,
    viewport: &mut DialogViewport,
) {
    use panel::{line_width, note, section, show, Column, Panel, Table};
    let queue = &app.state.work_queue;
    let now = util::now_secs();
    let tasks = app.source_rows();
    let mut panel = Panel::new("Work queue");
    panel.summary = vec![policy_status(queue, t)];
    panel.tabs = vec![
        panel::Tab::new("Queue", queue.items.len().to_string()),
        panel::Tab::new("Tasks", tasks.len().to_string()),
        panel::Tab::new("Chores", queue.chores.len().to_string()),
    ];
    panel.tab = match tab {
        Tab::Queue => 0,
        Tab::Tasks => 1,
        Tab::Chores => 2,
    };
    let mut details = Vec::<String>::new();
    let (columns, rows): (Vec<Column>, Vec<Vec<panel::Cell>>) = match tab {
        Tab::Queue => {
            let rows = queue
                .items
                .iter()
                .map(|item| {
                    vec![
                        vec![Span::styled(format!("#{}", item.id), t.dim())],
                        vec![Span::styled(safe(&item.state), t.accent())],
                        vec![Span::raw(safe(&item.title))],
                        vec![Span::raw(format!(
                            "{}{}",
                            item.provider.map_or("command", |p| p.label()),
                            item.model.as_ref().map_or(String::new(), |m| format!(" · {m}"))
                        ))],
                        vec![Span::raw(item.priority.to_string())],
                        vec![Span::styled(safe(&item.placement), t.dim())],
                        vec![Span::styled(safe(&item.reason), t.dim())],
                    ]
                })
                .collect();
            if let Some(item) = queue.items.get(sel) {
                let cancel = if item.cancelable { "cancelable" } else { "not cancelable" };
                details.push(format!(
                    "Created {} · updated {} · {cancel}",
                    when(item.created_at, now),
                    when(item.updated_at, now)
                ));
                if let Some(budget) = item.budget {
                    details.push(format!("Budget ${budget:.2} USD · informational"));
                }
                if let Some(source) = &item.source {
                    details.push(format!("Source {} / {}", source.source_id, source.item_id));
                }
                if let Some(dispatch) = &item.dispatch {
                    details.push(format!("Dispatched {} #{} · Enter opens it", dispatch.kind, dispatch.id));
                }
                for dependency in &item.dependencies {
                    details.push(format!(
                        "After {} → {}",
                        dependency.queue.map_or_else(
                            || format!(
                                "{} / {}",
                                dependency.source.as_deref().unwrap_or("?"),
                                dependency.task.as_deref().unwrap_or("?")
                            ),
                            |id| format!("queue #{id}")
                        ),
                        dependency.condition
                    ));
                }
            }
            let columns = vec![
                Column::right("#"),
                Column::left("State"),
                Column::left("Title"),
                Column::left("Program"),
                Column::right("Priority"),
                Column::left("Placement"),
                Column::left("Reason"),
            ];
            (columns, rows)
        }
        Tab::Tasks => {
            let rows = tasks
                .iter()
                .map(|task| {
                    let fresh =
                        queue.sources.iter().find(|s| s.id == task.source_id).is_some_and(|s| s.fresh && s.enabled);
                    vec![
                        vec![Span::styled(safe(&task.source_id), t.dim())],
                        vec![Span::raw(safe(&task.id))],
                        vec![Span::styled(safe(&task.state), t.accent())],
                        vec![Span::raw(safe(&task.title))],
                        if fresh {
                            vec![Span::styled("fresh", plain().fg(t.ok))]
                        } else {
                            vec![Span::styled("stale", plain().fg(t.warn))]
                        },
                    ]
                })
                .collect();
            if let Some(task) = tasks.get(sel) {
                if let Some(next) = task.next_step.as_deref().filter(|n| !n.is_empty()) {
                    details.push(format!("Next: {next}"));
                }
                details.push(format!(
                    "Worktree {} · branch {}",
                    task.worktree.as_deref().unwrap_or("—"),
                    task.branch.as_deref().unwrap_or("—")
                ));
                details.push("a opens an explicit executable spec; the title and next step are never executed.".into());
            }
            let columns = vec![
                Column::left("Source"),
                Column::left("Task"),
                Column::left("State"),
                Column::left("Title"),
                Column::left("Listing"),
            ];
            (columns, rows)
        }
        Tab::Chores => {
            let rows = queue
                .chores
                .iter()
                .map(|chore| {
                    vec![
                        vec![Span::raw(safe(&chore.id))],
                        if chore.effective_enabled {
                            vec![Span::styled("enabled", plain().fg(t.ok))]
                        } else {
                            vec![Span::styled("disabled", t.dim())]
                        },
                        vec![Span::raw(safe(&chore.title))],
                        vec![Span::styled(safe(&chore.target), t.dim())],
                        vec![Span::raw(format!("{} {}", chore.schedule.cron, chore.schedule.timezone))],
                        vec![Span::styled(chore.next_run.map_or_else(|| "—".into(), |at| when(at, now)), t.dim())],
                    ]
                })
                .collect();
            if let Some(chore) = queue.chores.get(sel) {
                details.push(format!(
                    "Owner {} · repository {} · registered {} · configured {}",
                    chore.owner,
                    chore.repository,
                    chore.registered,
                    if chore.enabled { "enabled" } else { "disabled" }
                ));
                if let Some(claim) = &chore.claim {
                    details.push(format!(
                        "Claim {} generation {} · session {} · claimed {} · stale {}",
                        claim.day,
                        claim.generation,
                        claim.session,
                        when(claim.claimed_at, now),
                        when(claim.stale_at, now)
                    ));
                }
                if let Some(outcome) = &chore.outcome {
                    details.push(format!("Outcome: {outcome}"));
                }
            }
            let columns = vec![
                Column::left("Chore"),
                Column::left("State"),
                Column::left("Title"),
                Column::left("Target"),
                Column::left("Schedule"),
                Column::right("Next"),
            ];
            (columns, rows)
        }
    };
    let table = Table::measure(columns, rows.iter().map(Vec::as_slice));
    let width = line_width(area, table.width().min(overlays::READING).max(panel.chrome_width()).max(72));
    let table = table.fit(width);
    let mut body = Vec::new();
    let mut selected_line = 0;
    if rows.is_empty() {
        body.extend(note(
            match tab {
                Tab::Queue => "No queued work. a prepares a new item.",
                Tab::Tasks => "No source tasks listed. :queue refresh SOURCE updates a source.",
                Tab::Chores => "No registered chores. a prepares a disabled registration.",
            },
            width,
            t,
        ));
    } else {
        panel.top.push(table.header(width, t));
    }
    for (i, cells) in rows.into_iter().enumerate() {
        if i == sel {
            selected_line = body.len();
        }
        body.push(table.row(cells, width, i == sel, t));
        if i == sel {
            for line in &details {
                for text in wrap_words(&safe(line), panel::text_width(width).saturating_sub(4).max(1)) {
                    body.push(Line::from(vec![
                        Span::raw(" ".repeat(overlays::INSET + 4)),
                        Span::styled(text, t.dim()),
                    ]));
                }
            }
        }
    }
    if tab == Tab::Tasks && !queue.sources.is_empty() {
        body.push(Line::raw(""));
        body.push(section(vec![Span::raw("Sources")], vec![Span::raw(queue.sources.len().to_string())], width, t));
        for source in &queue.sources {
            let state = if source.fresh && source.enabled {
                Span::styled("fresh", plain().fg(t.ok))
            } else if source.enabled {
                Span::styled("stale", plain().fg(t.warn))
            } else {
                Span::styled("disabled", t.dim())
            };
            let left = vec![
                Span::styled(safe(&source.id), t.bold()),
                Span::styled(format!("  {} · {}", safe(&source.owner), safe(&source.repository)), t.dim()),
            ];
            body.push(overlays::row(left, vec![state], width, false, t));
            if let Some(error) = &source.error {
                body.extend(overlays::text_rows(&format!("    {}", safe(error)), plain().fg(t.needs), width));
            }
        }
    }
    panel.body = body;
    panel.follow = Some(selected_line);
    show(f, area, width, panel, viewport, t);
}
