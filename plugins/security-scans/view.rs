use super::*;

/// `:scans`: the security scans, newest first, in a table.
pub fn scans_view(f: &mut Frame, area: Rect, sel: usize, app: &App, t: &Theme, viewport: &mut DialogViewport) {
    let now = util::now_secs();
    let scans = &app.state.scans;
    let running = scans.iter().filter(|s| matches!(s.state.as_str(), "running" | "preparing")).count();
    let mut panel = Panel::new("Security scans");
    panel.summary = if running > 0 {
        vec![Span::styled(format!("{} {running} running", spinner(app)), plain().fg(t.working))]
    } else {
        vec![Span::raw(if scans.len() == 1 { "1 scan".to_string() } else { format!("{} scans", scans.len()) })]
    };
    let rows: Vec<Vec<Cell>> = scans
        .iter()
        .map(|scan| {
            let state_style = match scan.state.as_str() {
                "running" | "preparing" => plain().fg(t.working),
                "saved" => plain().fg(t.ok),
                _ => plain().fg(t.needs),
            };
            let repo = std::path::Path::new(&scan.repo)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            vec![
                vec![Span::raw(scan.id.clone())],
                vec![Span::styled(scan.state.clone(), state_style)],
                vec![Span::styled(format!("{} ago", short_duration(now.saturating_sub(scan.started_at))), t.dim())],
                vec![Span::styled(app.host_label(scan.host.as_deref()), t.dim())],
                vec![Span::raw(repo)],
                if scan.report.is_some() { vec![Span::styled("✓ report", plain().fg(t.ok))] } else { Vec::new() },
            ]
        })
        .collect();
    let columns = vec![
        Column::left("Scan"),
        Column::left("State"),
        Column::right("Started"),
        Column::left("Host"),
        Column::left("Repository"),
        Column::left("Report"),
    ];
    let table = Table::measure(columns, rows.iter().map(Vec::as_slice));
    let width = line_width(area, table.width().max(panel.chrome_width()));
    let table = table.fit(width);
    if rows.is_empty() {
        panel.body = note("No scans yet: s starts one, or :scan [host].", width, t);
    } else {
        panel.top.push(table.header(width, t));
        panel.body = rows
            .into_iter()
            .enumerate()
            .map(|(i, mut cells)| {
                if i == sel {
                    cells[0] = vec![Span::styled(scans[i].id.clone(), t.bold())];
                }
                table.row(cells, width, i == sel, t)
            })
            .collect();
        panel.follow = Some(sel);
    }
    show(f, area, width, panel, viewport, t);
}
