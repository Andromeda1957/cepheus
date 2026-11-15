//! Documentation captures from the production ratatui renderer and synthetic data.
//! Ignored during tests: explicitly run with CEPHEUS_SCREENSHOTS_DIR set.
use super::*;
use crate::protocol::{BranchInfo, Conversation, HostInfo, HostStats, ScanInfo, Status, Tool, UsageWindow};
use crate::screen::{Run, ScreenFrame};
use crate::tui::tests::{app_with, session};
use alacritty_terminal::{
    event::VoidListener,
    term::{test::TermSize, Config as TermConfig, Term},
    vte::ansi::{Processor, StdSyncHandler},
};
use ratatui::{
    backend::{Backend, CrosstermBackend, TestBackend},
    buffer::Buffer,
    Terminal,
};
use serde_json::{json, Value};
use std::{fs, path::Path};

const WIDTH: u16 = 160;
const HEIGHT: u16 = 44;
const FOREGROUND: (u8, u8, u8) = (0xb9, 0xbe, 0xc4);
const BACKGROUND: (u8, u8, u8) = (0x13, 0x14, 0x16);
// The neutral dark palette used by the native Foot capture, not an SVG theme.
const BASE: [(u8, u8, u8); 16] = [
    (0x13, 0x14, 0x16),
    (0xdf, 0x65, 0x65),
    (0x82, 0xad, 0x4d),
    (0xb3, 0x93, 0x62),
    (0x36, 0x92, 0xe7),
    (0xb1, 0x8c, 0xf0),
    (0x66, 0xb4, 0xc8),
    (0xb9, 0xbe, 0xc4),
    (0x8f, 0x95, 0x9c),
    (0xef, 0x85, 0x85),
    (0xa2, 0xc5, 0x75),
    (0xd2, 0xb3, 0x85),
    (0x65, 0xb1, 0xf4),
    (0xc8, 0xab, 0xf5),
    (0x8b, 0xcd, 0xdd),
    (0xdf, 0xe3, 0xe8),
];

fn sample() -> App {
    let now = crate::util::now_secs();
    let mut sessions = vec![
        session(1, Tool::Codex, Status::Working, true),
        session(2, Tool::Claude, Status::NeedsYou, true),
        session(3, Tool::Antigravity, Status::Finished, true),
        session(4, Tool::Shell, Status::Idle, true),
        session(5, Tool::Dynamic("dot"), Status::Idle, true),
    ];
    for (s, name) in sessions.iter_mut().zip([
        "Refactor parser",
        "Review API changes",
        "Update documentation",
        "Project terminal",
        "Dot",
    ]) {
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
    app.cfg.sidebar_width = 40;
    app.cfg.sidebar.parts =
        ["title", "resources", "usage", "cloud", "sessions"].into_iter().map(str::to_owned).collect();
    app.state.providers =
        vec![crate::providers::Info { id: "dot".into(), label: "Dot".into(), cloud: true, ..Default::default() }];
    app.dot.profile = Some(crate::pet::Profile { name: Some("Dot".into()), ..Default::default() });
    app.theme = Theme::from_palette(&crate::config::Palette::from_parts(BASE, FOREGROUND, BACKGROUND, None));
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
        label: "wk".into(),
        used_percent: Some(42.0),
        resets_at: Some(now + 172800),
        updated_at: now,
    }];
    app.state.usage.codex = vec![UsageWindow {
        label: "wk".into(),
        used_percent: Some(37.0),
        resets_at: Some(now + 518400),
        updated_at: now,
    }];
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
    app.state.sessions[1].detail = "Approve the next check".into();
    app.frame = Some((1, agent_frame(WIDTH - 41, HEIGHT - 1)));
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

/// Feed colored synthetic agent output through the production terminal emulator.
fn agent_frame(cols: u16, rows: u16) -> ScreenFrame {
    let mut lines = vec![
        "".to_string(),
        "  \x1b[1;97mCodex\x1b[0m \x1b[2m· example project\x1b[0m".into(),
        "  \x1b[2m/workspace/example · feature/parser\x1b[0m".into(),
        "".into(),
        "\x1b[48;2;42;44;46m › Improve the parser diagnostics and check the result.\x1b[K\x1b[0m".into(),
        "".into(),
        "  \x1b[32m•\x1b[0m \x1b[1mExplored\x1b[0m the parser and existing diagnostic types".into(),
        "    \x1b[2mRead\x1b[0m \x1b[94msrc/parser.rs\x1b[0m, \x1b[94msrc/diagnostic.rs\x1b[0m".into(),
        "".into(),
        "  \x1b[32m•\x1b[0m \x1b[1mEdited\x1b[0m \x1b[94msrc/parser.rs\x1b[0m \x1b[32m(+24)\x1b[0m \x1b[31m(-8)\x1b[0m"
            .into(),
        "    \x1b[2m@@ fn parse_expression(&mut self) -> Result<Expr>\x1b[0m".into(),
        "    \x1b[31m- return Err(Error::UnexpectedToken);\x1b[0m".into(),
        "    \x1b[32m+ return Err(Diagnostic::unexpected(token)\x1b[0m".into(),
        "    \x1b[32m+     .with_location(self.source.location()));\x1b[0m".into(),
        "".into(),
        "  \x1b[32m•\x1b[0m \x1b[1mRan\x1b[0m \x1b[96mcargo test parser --lib\x1b[0m".into(),
        "    \x1b[2mrunning 16 tests\x1b[0m".into(),
        "    \x1b[32mtest result: ok. 16 passed; 0 failed\x1b[0m \x1b[2m· 0.42s\x1b[0m".into(),
        "".into(),
        "  \x1b[1mUpdated parser diagnostics\x1b[0m".into(),
        "  Source locations now stay attached to incomplete expressions.".into(),
        "".into(),
        "  \x1b[32m✓\x1b[0m Nested expressions retain their context".into(),
        "  \x1b[32m✓\x1b[0m Missing delimiters point to the opening token".into(),
        "  \x1b[32m✓\x1b[0m Focused checks pass".into(),
    ];
    while lines.len() < usize::from(rows.saturating_sub(4)) {
        lines.push(String::new());
    }
    lines.extend([
        "\x1b[48;2;42;44;46m › Add a regression check for nested calls\x1b[K\x1b[0m".into(),
        "\x1b[48;2;42;44;46m\x1b[K\x1b[0m".into(),
        "  \x1b[96mGPT-6.1 · Codex\x1b[0m \x1b[2m·\x1b[0m \x1b[95mWeekly 63% left\x1b[0m".into(),
    ]);
    let input = format!("\x1b[?25l{}", lines.join("\r\n"));
    let mut term = Term::new(TermConfig::default(), &TermSize::new(usize::from(cols), usize::from(rows)), VoidListener);
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    parser.advance(&mut term, input.as_bytes());
    crate::screen::build_frame(&term)
}

fn capture(root: &Path, entries: &mut Vec<Value>, id: &str, title: &str, app: &mut App) {
    // The README hero is a genuinely smaller production viewport, so its type
    // remains readable when GitHub fits the image to the page's content width.
    let (width, height, sidebar) = if id == "01-workspace" { (120, 34, 30) } else { (WIDTH, HEIGHT, 40) };
    app.cfg.sidebar_width = sidebar;
    if let Some((session, screen)) = &mut app.frame {
        if *session == 1 {
            *screen = agent_frame(width - sidebar - 1, height - 1);
        } else {
            screen.cols = width - sidebar - 1;
            screen.rows = height - 1;
        }
    }
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut cursor = None;
    terminal
        .draw(|f| {
            cursor = render(f, app);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut ansi = b"\x1b[?1049h\x1b[0m\x1b[2J\x1b[H\x1b[?25l".to_vec();
    {
        // Use ratatui's actual output backend, including its SGR handling and
        // wide-cell diffing. Foot draws these bytes; no SVG approximates it.
        let mut backend = CrosstermBackend::new(&mut ansi);
        let empty = Buffer::empty(buffer.area);
        backend.draw(empty.diff(buffer).into_iter()).unwrap();
    }
    ansi.extend_from_slice(b"\x1b[0m");
    if let Some((x, y, _)) = cursor {
        ansi.extend_from_slice(format!("\x1b[{};{}H\x1b[?25h", y + 1, x + 1).as_bytes());
    }
    let text =
        (0..height).map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>() + "\n").collect::<String>();
    fs::write(root.join(format!("{id}.ansi")), ansi).unwrap();
    fs::write(root.join(format!("{id}.txt")), text).unwrap();
    entries.push(json!({"id":id,"title":title,"ansi":format!("{id}.ansi"),"image":format!("{id}.png"),"columns":width,"rows":height,"sidebar_columns":sidebar}));
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
        if id == "07-errors" {
            app.state.errors = vec![crate::protocol::LogEntry::new(
                crate::util::now_secs() - 90,
                "Example: optional host is offline; reconnect when available",
            )];
        }
        app.dialog = Some(dialog);
        capture(root, &mut entries, id, title, &mut app);
        app.state.errors.clear();
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
    app.current = Some(Sel::Session(5));
    app.frame = None;
    app.dot.profile = Some(crate::pet::Profile { name: Some("Dot".into()), ..Default::default() });
    app.dot.items=vec![crate::remote::chat::Item::User {text:"Summarize the parser changes.".into(),client_id:None,images:vec![],at:None},crate::remote::chat::Item::Assistant {text:"## Parser diagnostics\n\nThe update keeps **source locations** attached to syntax errors.\n\n- Nested expressions retain their context.\n- Missing delimiters point to the opening token.\n- The focused checks pass.\n\n```rust\nlet report = parser.diagnostics();\n```".into(),questions:vec![],at:None}];
    app.dot.draft = "Which edge cases are covered?".into();
    app.mode = Mode::Insert;
    capture(root, &mut entries, "38-cloud-chat", "Native cloud conversation", &mut app);
    crate::providers::install(&[]).unwrap();
    fs::write(root.join("manifest.json"),serde_json::to_string_pretty(&json!({"schema":1,"renderer":"production ratatui view::render","columns":WIDTH,"rows":HEIGHT,"terminal":{"font":"JetBrainsMono Nerd Font","font_size_points":12,"foreground":FOREGROUND,"background":BACKGROUND,"palette":BASE},"data":"synthetic; no owner sessions or credentials","screens":entries})).unwrap()+"\n").unwrap();
    println!("Captured {} desktop screens in {}", entries.len(), root.display());
}
