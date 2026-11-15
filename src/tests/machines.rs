//! Script machines (`deck.ui.machine`) in the stats table.
use super::*;
use ratatui::{backend::TestBackend, Terminal};

fn screen(app: &mut App) -> String {
    let (width, height) = (120, 30);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| {
            view::render(f, app);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height).map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>() + "\n").collect()
}

fn machine(id: &str, label: &str, sidebar: bool) -> crate::lua::Machine {
    crate::lua::Machine {
        id: id.into(),
        label: label.into(),
        cpu: Some(12.0),
        memory: Some(30.0),
        disk_used: Some(70 << 30),
        disk_available: Some(45 << 30),
        status: None,
        sidebar,
    }
}

// Owner, 2026-11-12: "i dont need to see every vps 100% of the time, but
// production is too important not to see 100% of the time".
#[test]
fn a_sidebar_machine_keeps_its_row_and_machines_lists_the_rest() {
    let (mut app, _rx) = tests::app_with(vec![], vec![]);
    let mut mail = machine("vps-mail", "Mail", false);
    mail.status = Some("stale · 7m".into());
    app.lua_machines = vec![machine("vps-production", "Production", true), mail];
    let sidebar = screen(&mut app);
    let row = sidebar.lines().find(|line| line.contains("Production")).expect("Production in the sidebar");
    assert!(row.contains("12%") && row.contains("30%") && row.contains("70G"), "{row}");
    assert!(!sidebar.contains("Mail"), "{sidebar}");
    app.run_command("machines");
    let overlay = screen(&mut app);
    let row = overlay.lines().find(|line| line.contains("Mail")).expect("Mail in :machines");
    assert!(row.contains("stale · 7m"), "{row}");
    tests::press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.dialog.is_none());
}
