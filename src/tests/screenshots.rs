//! Documentation captures from the production ratatui renderer and synthetic data.
//! Ignored during tests: explicitly run with CEPHEUS_SCREENSHOTS_DIR set.
use super::*;
use crate::protocol::{BranchInfo, Conversation, HostInfo, HostStats, ScanInfo, Status, Tool, UsageWindow};
use crate::screen::{Run, ScreenFrame};
use crate::tui::tests::{app_with, session};
use ratatui::{backend::TestBackend, Terminal};
use serde_json::{json, Value};
use std::{fmt::Write as _, fs, path::Path};

const WIDTH: u16 = 144;
const HEIGHT: u16 = 44;
const BASE: [(u8, u8, u8); 16] = [
    (26, 27, 38),
    (247, 118, 142),
    (158, 206, 106),
    (224, 175, 104),
    (122, 162, 247),
    (187, 154, 247),
    (125, 207, 255),
    (169, 177, 214),
    (86, 95, 137),
    (247, 118, 142),
    (158, 206, 106),
    (224, 175, 104),
    (122, 162, 247),
    (187, 154, 247),
    (125, 207, 255),
    (192, 202, 245),
];

fn sample() -> App {
    let now = crate::util::now_secs();
    let mut sessions = vec![
        session(1, Tool::Codex, Status::Working, true),
        session(2, Tool::Claude, Status::NeedsYou, true),
        session(3, Tool::Antigravity, Status::Finished, true),
        session(4, Tool::Shell, Status::Idle, true),
    ];
    for (s, name) in
        sessions.iter_mut().zip(["Refactor parser", "Review API changes", "Update documentation", "Project terminal"])
    {
        s.title = name.into();
        s.cwd = Some("/workspace/example".into());
        s.created_at = now - 1200;
        s.last_active = now - 12;
        s.status_since = now - 45;
    }
    sessions[0].worktree = Some(crate::protocol::Worktree {
        path: "/workspace/example-feature".into(),
        branch: Some("feature/parser".into()),
        task: Some("parser".into()),
        exists: true,
    });
    sessions[0].subagents = vec![crate::protocol::Subagent {
        name: "Check parser edge cases".into(),
        kind: None,
        active: true,
        started_at: now - 60,
        last_active: now - 3,
    }];
    sessions[2].host = Some("buildbox".into());
    let (mut app, _) = app_with(sessions, vec![]);
    app.cfg.host_name = Some("workstation".into());
    app.cfg.sidebar_width = 37;
    app.theme = Theme::from_palette(&crate::config::Palette::from_parts(BASE, (192, 202, 245), (26, 27, 38), None));
    app.state.start_dir = "/workspace/example".into();
    app.state.attention_capable = true;
    app.state.work_queue_capable = true;
    app.state.job_capable = true;
    app.current = Some(Sel::Session(1));
    app.git.now = Some(BranchStatus { branch: "main".into(), ahead: 2, ..Default::default() });
    app.resources.now = Resources {
        cpu: Some(18.0),
        memory: Some(42.0),
        disk: Some(crate::resources::disk_usage(500, 300, 280, 1 << 30)),
    };
    app.state.branches = vec![
        BranchInfo {
            branch: "main".into(),
            path: "/workspace/example".into(),
            head: Some("a1b2c3d".into()),
            subject: Some("Improve diagnostics".into()),
            committed_at: Some(now - 120),
            main: true,
            ahead: Some(0),
            behind: Some(0),
        },
        BranchInfo {
            branch: "feature/parser".into(),
            path: "/workspace/example-feature".into(),
            head: Some("d4e5f6a".into()),
            subject: Some("Handle nested expressions".into()),
            committed_at: Some(now - 60),
            main: false,
            ahead: Some(2),
            behind: Some(0),
        },
    ];
    app.hosts = vec![HostInfo {
        name: "buildbox".into(),
        label: "buildbox".into(),
        online: true,
        start_dir: "/workspace/example".into(),
        stats: Some(HostStats {
            cpu: Some(23.0),
            memory: Some(31.0),
            disk_used: Some(120 << 30),
            disk_available: Some(360 << 30),
            branch: Some("main".into()),
            head: Some("a1b2c3d".into()),
            worktrees: 2,
            ..Default::default()
        }),
        ..Default::default()
    }];
    app.state.hosts = app.hosts.clone();
    app.state.usage.claude = vec![UsageWindow {
        label: "5h".into(),
        used_percent: Some(28.0),
        resets_at: Some(now + 7200),
        updated_at: now,
    }];
    app.state.usage.codex = vec![UsageWindow {
        label: "5h".into(),
        used_percent: Some(36.0),
        resets_at: Some(now + 10800),
        updated_at: now,
    }];
    app.state.errors =
        vec![crate::protocol::LogEntry::new(now - 90, "Example: optional host is offline; reconnect when available")];
    app.state.scans = vec![ScanInfo {
        id: "example-review".into(),
        repo: "/workspace/example".into(),
        started_at: now - 900,
        finished_at: Some(now - 600),
        dir: "/workspace/scans/example-review".into(),
        report: Some("/workspace/scans/example-review/report.md".into()),
        state: "saved".into(),
        ..Default::default()
    }];
    app.state.jobs = vec![crate::job_sessions::Info {
        id: 10,
        title: "Build documentation".into(),
        status: "finished".into(),
        created_at: now - 240,
        started_at: Some(now - 230),
        finished_at: Some(now - 120),
        exit_code: Some(0),
        ..Default::default()
    }];
    app.state.work_queue = serde_json::from_value(json!({
        "enabled":false,"pause_effective":true,"items":[{"kind":"job","id":1,"title":"Run parser checks","state":"held","priority":0,"created_at":now-180,"updated_at":now-60,"reason":"Dispatch is paused","dispatch":null,"dependencies":[],"source":null,"budget":null,"cancelable":true,"provider":null,"model":null,"placement":"auto"}],
        "sources":[{"id":"example-board","owner":"team","repository":"example","host":null,"enabled":true,"updated_at":now-30,"fresh":true,"error":null}],
        "tasks":[{"source_id":"example-board","id":"parser","title":"Improve parser diagnostics","state":"ready","branch":"feature/parser","next_step":"Review the patch"}],
        "chores":[{"owner_live":false,"placement":"local","guarded":false,"provider":null,"id":"daily-checks","owner":"team","repository":"example","title":"Daily checks","registered":true,"enabled":false,"effective_enabled":false,"target":"job","schedule":{"cron":"0 9 * * 1-5","timezone":"UTC"},"next_run":null,"claim":null,"outcome":"Not run"}]
    })).unwrap();
    app.state.attention.messages = vec![crate::attention::MessageInfo {
        host: None,
        id: "example-follow-up".into(),
        session: 1,
        state: crate::attention::DeliveryState::Queued,
        created_at: now - 60,
        updated_at: now - 60,
        reason: "Waiting for the current turn to finish".into(),
    }];
    app.state.attention.queued.insert(1, 1);
    app.frame = Some((
        1,
        frame(&[
            "",
            "  Codex · example project",
            "",
            "  › Improve the parser diagnostics and check the result.",
            "",
            "  • I found the parser entry point and the existing error types.",
            "    The update preserves the source location when an expression is incomplete.",
            "",
            "    src/parser.rs       +24 -8",
            "    src/diagnostic.rs   +12 -3",
            "",
            "  • Running the focused parser checks…",
            "",
            "    ✓ nested expressions",
            "    ✓ missing closing delimiter",
            "    ✓ useful source locations",
            "",
            "  › ",
        ]),
    ));
    app
}

fn frame(lines: &[&str]) -> ScreenFrame {
    ScreenFrame {
        cols: WIDTH - 38,
        rows: HEIGHT - 1,
        lines: lines
            .iter()
            .map(|s| vec![Run { c: 0, w: 1, fg: 0, bg: 0, fl: 0, t: s.to_string(), g: false }])
            .collect(),
        ..Default::default()
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn color(c: Color, background: bool) -> String {
    let (r, g, b) = match c {
        Color::Reset => {
            if background {
                (26, 27, 38)
            } else {
                (192, 202, 245)
            }
        }
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i) => {
            crate::config::Palette::from_parts(BASE, (192, 202, 245), (26, 27, 38), None).get(i.into())
        }
        Color::Black => BASE[0],
        Color::Red => BASE[1],
        Color::Green => BASE[2],
        Color::Yellow => BASE[3],
        Color::Blue => BASE[4],
        Color::Magenta => BASE[5],
        Color::Cyan => BASE[6],
        Color::Gray => BASE[7],
        Color::DarkGray => BASE[8],
        Color::LightRed => BASE[9],
        Color::LightGreen => BASE[10],
        Color::LightYellow => BASE[11],
        Color::LightBlue => BASE[12],
        Color::LightMagenta => BASE[13],
        Color::LightCyan => BASE[14],
        Color::White => BASE[15],
    };
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn capture(root: &Path, entries: &mut Vec<Value>, id: &str, title: &str, app: &mut App) {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
    terminal
        .draw(|f| {
            render(f, app);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut svg = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\"><title>{}</title><rect width=\"100%\" height=\"100%\" fill=\"#1a1b26\"/>", WIDTH*10+32, HEIGHT*20+32, WIDTH*10+32, HEIGHT*20+32, xml(title));
    let mut text = String::new();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let cell = &buffer[(x, y)];
            let mut fg = cell.fg;
            let mut bg = cell.bg;
            if cell.modifier.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if bg != Color::Reset {
                write!(
                    svg,
                    "<rect x=\"{}\" y=\"{}\" width=\"10\" height=\"20\" fill=\"{}\"/>",
                    16 + x * 10,
                    16 + y * 20,
                    color(bg, true)
                )
                .unwrap();
            }
            text.push_str(cell.symbol());
        }
        text.push('\n');
    }
    svg.push_str(
        "<g font-family=\"JetBrains Mono,DejaVu Sans Mono,monospace\" font-size=\"15\" xml:space=\"preserve\">",
    );
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let cell = &buffer[(x, y)];
            if cell.symbol().trim().is_empty() {
                continue;
            }
            let fg = if cell.modifier.contains(Modifier::REVERSED) { cell.bg } else { cell.fg };
            let bold = if cell.modifier.contains(Modifier::BOLD) { "bold" } else { "normal" };
            write!(
                svg,
                "<text x=\"{}\" y=\"{}\" fill=\"{}\" font-weight=\"{}\">{}</text>",
                16 + x * 10,
                31 + y * 20,
                color(fg, false),
                bold,
                xml(cell.symbol())
            )
            .unwrap();
        }
    }
    svg.push_str("</g></svg>\n");
    fs::write(root.join(format!("{id}.svg")), svg).unwrap();
    fs::write(root.join(format!("{id}.txt")), text).unwrap();
    entries.push(json!({"id":id,"title":title,"svg":format!("{id}.svg"),"image":format!("{id}.png")}));
}

#[test]
#[ignore = "explicit documentation screenshot capture"]
fn capture_all_screens() {
    let root = std::env::var_os("CEPHEUS_SCREENSHOTS_DIR")
        .expect("set CEPHEUS_SCREENSHOTS_DIR to a disposable output directory");
    let root = Path::new(&root);
    fs::create_dir_all(root).unwrap();
    let mut entries = Vec::new();
    let mut app = sample();
    capture(root, &mut entries, "01-workspace", "Workspace and hosted agent", &mut app);
    app.mode = Mode::Insert;
    capture(root, &mut entries, "02-insert", "Typing in a hosted session", &mut app);
    app.mode = Mode::Command;
    app.command.set("new codex");
    capture(root, &mut entries, "03-command", "Command line", &mut app);
    app.mode = Mode::Normal;
    app.command.clear();
    app.current = None;
    app.frame = None;
    capture(root, &mut entries, "04-empty-pane", "Session selection", &mut app);
    app = sample();
    let dialogs = vec![
        ("05-new-session", "New session", Dialog::New { sel: 0, model: 0, effort: 0 }),
        ("06-help", "Help and key bindings", Dialog::Help),
        ("07-errors", "Errors", Dialog::Errors { scroll: 0 }),
        ("08-machines", "Machine resources", Dialog::Machines),
        ("09-usage", "Provider usage", Dialog::Usage),
        ("10-branches", "Branches and worktrees", Dialog::Branches { sel: 0, filter: String::new() }),
        ("11-scans", "Security scans", Dialog::Scans { sel: 0 }),
        (
            "12-broadcast",
            "Broadcast",
            Dialog::Broadcast {
                text: "Please report progress after your current check.".into(),
                targets: vec![(1, true), (2, true), (3, false)],
                sel: 0,
            },
        ),
        (
            "13-confirm",
            "Service confirmation",
            Dialog::Confirm {
                text: "Restart the service and preserve running sessions?".into(),
                action: crate::tui::Action::Restart,
            },
        ),
        (
            "14-resume",
            "Resume a saved conversation",
            Dialog::Picker {
                items: vec![Conversation {
                    tool: Tool::Codex,
                    id: "sample-conversation".into(),
                    title: "Parser diagnostics".into(),
                    created_at: crate::util::now_secs() - 3600,
                    updated_at: crate::util::now_secs() - 300,
                    open_in: None,
                    host: None,
                    search_words: vec![],
                }],
                sel: 0,
                filter: String::new(),
                loading: false,
                search_due: None,
                search_request: None,
            },
        ),
        ("15-queue", "Work queue", Dialog::WorkQueue { tab: crate::tui::work_queue::Tab::Queue, sel: 0 }),
        ("16-task-sources", "Task sources", Dialog::WorkQueue { tab: crate::tui::work_queue::Tab::Tasks, sel: 0 }),
        ("17-chores", "Scheduled chores", Dialog::WorkQueue { tab: crate::tui::work_queue::Tab::Chores, sel: 0 }),
        ("18-attention", "Idle deliveries", Dialog::Attention { tab: crate::tui::attention::Tab::Messages, sel: 0 }),
        ("19-supervisor", "Supervisor", Dialog::Attention { tab: crate::tui::attention::Tab::Supervisor, sel: 0 }),
        (
            "20-hosts",
            "Hosts report",
            Dialog::Report {
                title: "Hosts".into(),
                lines: vec![
                    "workstation · local · online".into(),
                    "buildbox · agents · online".into(),
                    "".into(),
                    "Each host keeps its native sessions and process ownership.".into(),
                ],
                scroll: 0,
                confirm: None,
                waiting: 0,
            },
        ),
        (
            "21-install-preview",
            "Extension installation preview",
            Dialog::Report {
                title: "Extension preview".into(),
                lines: vec![
                    "Source: /workspace/example-extension".into(),
                    "Target: workstation".into(),
                    "".into(),
                    "1 Lua plugin".into(),
                    "1 agent skill".into(),
                    "".into(),
                    "Review this preview before installation.".into(),
                ],
                scroll: 0,
                confirm: Some(vec![]),
                waiting: 0,
            },
        ),
    ];
    for (id, title, dialog) in dialogs {
        app.dialog = Some(dialog);
        capture(root, &mut entries, id, title, &mut app);
    }
    let mut draft = crate::edit::Draft::default();
    draft.insert("How do I inspect a session on another host?");
    app.ask_history = vec![crate::ask::Asked {
        question: "How do I start a terminal?".into(),
        who: "Codex".into(),
        answer:
            "Use `:term`, or choose Shell in the New session dialog.\n\nThe terminal stays hosted when the view closes."
                .into(),
        at: crate::util::now_secs() - 120,
        ..Default::default()
    }];
    app.dialog = Some(Dialog::Ask { draft, route: Some(crate::ask::Route::Code), scroll: 0 });
    capture(root, &mut entries, "22-ask", "Quick questions and answers", &mut app);
    let key = "example-draft".to_string();
    app.attention.active_draft = Some(key.clone());
    app.attention.drafts.insert(
        key,
        crate::tui::attention::Draft {
            session: 1,
            namespace: 0,
            conversation: Some("sample-conversation".into()),
            pid: Some(1001),
            spawned_ms: 1001,
            host: None,
            text: "When this turn finishes, summarize the parser changes.".into(),
            request: None,
            error: None,
            accepted: false,
            attempted: false,
            local_only: true,
        },
    );
    app.dialog = Some(Dialog::AttentionSend);
    capture(root, &mut entries, "23-idle-message", "Idle message draft", &mut app);
    let temp = crate::testsupport::TempDir::new("documentation-pairing");
    let mut pairing =
        crate::pairing::Pairing::open(temp.path(), &crate::config::Config::default(), crate::util::now_secs());
    pairing.offer = Ok(crate::pairing::Offer {
        code: "DEMO CODE".into(),
        address: "example.invalid:7443".into(),
        fingerprint: "00".repeat(32),
        expires: crate::util::now_secs() + 300,
        modules: crate::pairing::qr_modules("cepheus://pair?host=example.invalid&port=7443&fp=demo&code=DEMO").unwrap(),
    });
    app.dialog = Some(Dialog::Pair(Box::new(pairing)));
    capture(root, &mut entries, "24-pairing", "Pair an Android device (inert example)", &mut app);
    let pick = crate::lua::Pick::new(
        "Example plugin picker",
        vec![
            crate::lua::PickItem::choice(vec![crate::lua::Segment {
                text: "Open project documentation".into(),
                style: "accent".into(),
            }]),
            crate::lua::PickItem::choice(vec![crate::lua::Segment {
                text: "Inspect task metadata".into(),
                style: String::new(),
            }]),
        ],
    );
    app.dialog = Some(Dialog::Choose { pick: Box::new(pick), sel: 0, filter: String::new(), callback: 0 });
    capture(root, &mut entries, "25-plugin-picker", "Lua plugin picker", &mut app);
    app.dialog = None;
    app.current = Some(Sel::Job(10));
    app.jobs.id = Some(10);
    app.jobs.eof = true;
    app.jobs.items = vec![crate::remote::chat::Item::Notice {
        text: "Documentation built successfully.\n42 pages generated with no warnings.".into(),
    }];
    capture(root, &mut entries, "26-job-chat", "Headless job output", &mut app);
    app.jobs.raw = true;
    app.jobs.text = "$ cargo doc --no-deps\n Documenting example v0.1.0\n Finished documentation in 2.4s\n".into();
    capture(root, &mut entries, "27-job-raw", "Headless job raw log", &mut app);
    app = sample();
    app.current = Some(Sel::Session(4));
    app.frame = Some((
        4,
        frame(&[
            "$ git status --short",
            " M src/parser.rs",
            " M src/diagnostic.rs",
            "",
            "$ cargo check",
            "    Checking example v0.1.0",
            "    Finished dev profile in 1.2s",
            "",
            "$ ",
        ]),
    ));
    capture(root, &mut entries, "28-terminal", "Hosted shell", &mut app);
    app = sample();
    app.dialog = Some(Dialog::Attention { tab: crate::tui::attention::Tab::Messages, sel: 0 });
    app.attention.private=Some(("Delivery receipt".into(),"Synthetic example receipt.\nThe message is queued for the captured conversation.\nNo message was sent for this screenshot.".into()));
    app.attention.reader = Some(crate::tui::attention::PrivateReader {
        identity: "example-follow-up".into(),
        list_viewport: DialogViewport::default(),
    });
    capture(root, &mut entries, "29-private-response", "Explicitly opened delivery response", &mut app);
    app = sample();
    app.dialog = Some(Dialog::Help);
    capture(root, &mut entries, "30-help-sessions", "Help: sessions and hotkeys", &mut app);
    app.dialog_viewport.offset = app.dialog_viewport.page;
    capture(root, &mut entries, "31-help-windows", "Help: windows and jobs", &mut app);
    app.dialog_viewport.offset = app.dialog_viewport.page * 2;
    capture(root, &mut entries, "32-help-commands", "Help: attention and commands", &mut app);
    app.dialog = None;
    app.run_command("slots");
    capture(root, &mut entries, "33-fleet-slots", "Fleet scheduler slots", &mut app);
    app.dialog = None;
    app.run_command("help commands");
    capture(root, &mut entries, "34-command-catalog", "Command catalog", &mut app);
    let settings = crate::testsupport::TempDir::new("documentation-lua");
    settings.write("config/init.lua", "local deck = require('deck')\ndeck.opt.sidebar_width = 37\ndeck.opt.sidebar = {'title','resources','usage','sessions'}\n");
    app.load_lua(
        crate::lua::Places { config_dir: settings.path().join("config"), start_dir: settings.path().into() },
        std::sync::Arc::new(|_| {}),
    );
    app.dialog = None;
    app.run_command("set");
    capture(root, &mut entries, "35-options", "Lua options", &mut app);
    app.dialog = Some(Dialog::Report {
        title: "Skills".into(),
        lines: vec![
            "Cepheus: operate sessions and native peer messaging".into(),
            "Cepheus plugin: create Lua commands, pickers and integrations".into(),
            "".into(),
            "Agent links are installed only when explicitly requested.".into(),
        ],
        scroll: 0,
        confirm: None,
        waiting: 0,
    });
    capture(root, &mut entries, "36-skills", "Skills report", &mut app);
    app = sample();
    app.dialog = Some(Dialog::Branches { sel: 0, filter: "parser".into() });
    app.dialog_viewport.begin(app.dialog.as_ref().unwrap());
    app.dialog_viewport.search = Some("parser".into());
    capture(root, &mut entries, "37-search", "Filter a window", &mut app);
    let provider: crate::providers::Provider = serde_json::from_value(json!({"id":"dot","label":"Dot","command":["cepheus","dot","chat"],"cloud":true,"chat_snapshot":settings.path().join("transcript.json")})).unwrap();
    crate::providers::install(&[provider]).unwrap();
    app = sample();
    app.state.sessions[0].tool = Tool::Dynamic("dot");
    app.state.sessions[0].title = "Cloud conversation".into();
    app.dot.profile = Some(crate::pet::Profile { name: Some("Dot".into()), ..Default::default() });
    app.dot.items=vec![crate::remote::chat::Item::User {text:"Summarize the parser changes.".into(),client_id:None,images:vec![],at:None},crate::remote::chat::Item::Assistant {text:"## Parser diagnostics\n\nThe update keeps **source locations** attached to syntax errors.\n\n- Nested expressions retain their context.\n- Missing delimiters point to the opening token.\n- The focused checks pass.\n\n```rust\nlet report = parser.diagnostics();\n```".into(),questions:vec![],at:None}];
    app.dot.draft = "Which edge cases are covered?".into();
    app.mode = Mode::Insert;
    capture(root, &mut entries, "38-cloud-chat", "Native cloud conversation", &mut app);
    crate::providers::install(&[]).unwrap();
    fs::write(root.join("manifest.json"),serde_json::to_string_pretty(&json!({"schema":1,"renderer":"production ratatui view::render","columns":WIDTH,"rows":HEIGHT,"data":"synthetic; no owner sessions or credentials","screens":entries})).unwrap()+"\n").unwrap();
    println!("Captured {} desktop screens in {}", entries.len(), root.display());
}
