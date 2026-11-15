use super::*;
use crate::protocol::{SessionInfo, Tool};
use std::io::Write;

struct RunningService {
    socket: PathBuf,
    handle: Option<thread::JoinHandle<anyhow::Result<()>>>,
}
impl Drop for RunningService {
    fn drop(&mut self) {
        let _ = crate::testsupport::shutdown_service(&self.socket);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn phone_receives_unfinished_replies_and_reconciles_without_index_changes() {
    for (tool, long) in [(Tool::Claude, false), (Tool::Codex, false), (Tool::Claude, true)] {
        let dir = TempDir::new("remote-live-reply");
        let display = dir.path().join("display");
        fs::write(&display, "ready\n").unwrap();
        let script = format!(
            "stty -echo; while :; do printf '\\033[2J\\033[H'; cat {}; sleep 0.02; done",
            util::shell_quote(&display.to_string_lossy())
        );
        let opts = service_options(&dir, &script);
        let service = RunningService {
            socket: opts.socket.clone(),
            handle: Some(thread::spawn(move || crate::service::run(opts))),
        };
        let (hub, mut ws) = paired_phone(hub_options(&dir, vec![PHONE]));
        send(&mut ws, json!({"t":"new","rid":1,"tool":tool.wire_key()}));
        let id = reply(&mut ws, 1)["id"].as_u64().unwrap();
        expect(&mut ws, "session state", |v| {
            v["t"] == "state" && v["sessions"].as_array().is_some_and(|s| s.iter().any(|s| s["id"] == id))
        });
        let conversation = if tool == Tool::Claude {
            hub.session(id).unwrap().conversation.unwrap()
        } else {
            let conversation = "11111111-1111-4111-8111-111111111111";
            let mut control = UnixStream::connect(&service.socket).unwrap();
            write_msg(&mut control, &ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Hook }).unwrap();
            write_msg(
                &mut control,
                &ClientMsg::Hook {
                    session: id,
                    tool,
                    payload: json!({"type":"agent-turn-complete","thread-id":conversation}),
                },
            )
            .unwrap();
            conversation.to_string()
        };
        let (transcript, user, assistant) = if tool == Tool::Claude {
            (
                dir.path().join(format!("claude/projects/-fixture/{conversation}.jsonl")),
                transcript_line("user", "question"),
                transcript_line("assistant", "First second third."),
            )
        } else {
            let meta = json!({"type":"session_meta","payload":{"id":conversation,"cwd":dir.path(),"timestamp":"2026-11-05T23:00:00Z"}});
            let user = json!({"type":"event_msg","payload":{"type":"user_message","message":"question"}});
            let assistant =
                json!({"type":"event_msg","payload":{"type":"agent_message","message":"First second third."}});
            (
                dir.path().join(format!("codex/sessions/2026/11/05/rollout-x-{conversation}.jsonl")),
                format!("{meta}\n{user}\n"),
                format!("{assistant}\n"),
            )
        };
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, &user).unwrap();
        send(&mut ws, json!({"t":"open","rid":2,"id":id}));
        let mut page = reply(&mut ws, 2);
        for _ in 0..40 {
            if page["source"] == "log" {
                break;
            }
            thread::sleep(Duration::from_millis(50));
            send(&mut ws, json!({"t":"open","rid":2,"id":id}));
            page = reply(&mut ws, 2);
        }
        assert_eq!(page["source"], "log", "{page}");
        assert_eq!(page["total"], 1, "{page}");
        reply_edge_cases(&hub.session(id).unwrap());
        let mut final_text = String::new();
        for text in ["First", "First second", "First second third."] {
            let body = if long {
                format!("{text}\n{}", (0..60).map(|i| format!("  extra line {i}\n")).collect::<String>())
            } else {
                text.to_string()
            };
            final_text = body.trim().to_string();
            let frame = if tool == Tool::Claude {
                format!("❯ question\n\n● {body}\n\n✶ Working… (esc to interrupt)\n────────────────────────\n❯\n────────────────────────\n")
            } else {
                format!("› question\n\n• {body}\n\n• Working (esc to interrupt)\n\n›\n  model · ? for shortcuts\n")
            };
            fs::write(&display, frame).unwrap();
            let update = expect(&mut ws, "unfinished live reply before any assistant record exists", |v| {
                v["t"] == "items" && v["id"] == id && v["stream"].as_str().is_some_and(|s| s.starts_with(text))
            });
            assert_eq!(update["total"], 1, "provisional text must not take a transcript index");
            assert_eq!(update["items"], json!([]));
        }
        let assistant = if long { transcript_line("assistant", &final_text) } else { assistant };
        fs::OpenOptions::new().append(true).open(&transcript).unwrap().write_all(assistant.as_bytes()).unwrap();
        let saved = expect(&mut ws, "canonical reply replaces the stream", |v| v["t"] == "items" && v["total"] == 2);
        assert!(saved["stream"].is_null(), "{saved}");
        assert_eq!(saved["items"].as_array().unwrap().len(), 1);
        assert_eq!(saved["items"][0]["text"], final_text);
        send(&mut ws, json!({"t":"open","rid":3,"id":null}));
        reply(&mut ws, 3);
        drop(ws);
        drop(hub);
        drop(service);
    }
}

/// Regressions from reviewing native snapshots against delayed saved records.
fn reply_edge_cases(info: &SessionInfo) {
    use crate::remote::{chat::Item, live::ReplyState};
    let user = |text: &str| Item::User { client_id: None, text: text.into(), images: vec![], at: None };
    let assistant = |text: &str| Item::Assistant { text: text.into(), questions: vec![], at: None };
    let frame = |prompt: Option<&str>, text: &str| -> Vec<String> {
        let prompt = prompt
            .map(|p| format!("{} {p}\n\n", if info.tool == Tool::Claude { '❯' } else { '›' }))
            .unwrap_or_default();
        let value = if info.tool == Tool::Claude {
            format!("{prompt}● {text}\n\n✶ Working… (esc to interrupt)\n────────────────────────\n❯\n────────────────────────\n")
        } else {
            format!("{prompt}• {text}\n\n• Working (esc to interrupt)\n\n›\n  model · ? for shortcuts\n")
        };
        value.lines().map(String::from).collect()
    };
    let items = vec![user("question")];
    for stale in ["old task", "Image resizing task"] {
        let mut state = ReplyState::default();
        for text in ["Old answer", "Old answer grows"] {
            assert_eq!(state.reply(info, &frame(Some(stale), text), 0, 0, &items), None);
        }
    }
    // Repeated answers belong to the current user turn, never a previous one.
    let repeated = vec![user("old task"), assistant("Added support."), user("question")];
    let mut state = ReplyState::default();
    assert_eq!(
        state.reply(info, &frame(Some("question"), "Added support."), 0, 0, &repeated).as_deref(),
        Some("Added support.")
    );
    // A delayed earlier commentary record does not persist the live block.
    let mut delayed = items.clone();
    let live = frame(Some("question"), "Read the next section.");
    let mut state = ReplyState::default();
    assert!(state.reply(info, &live, 0, 0, &delayed).is_some());
    delayed.push(assistant("An earlier commentary."));
    assert!(state.reply(info, &live, 0, 0, &delayed).is_some());
    delayed.push(assistant("Read the next section."));
    assert_eq!(state.reply(info, &live, 0, 0, &delayed), None);
    // A distinct new block may extend the words of completed commentary.
    delayed.push(assistant("Done."));
    assert!(state.reply(info, &frame(Some("question"), "Done. The changes are ready."), 0, 0, &delayed).is_some());
    for prose in [
        "Ran the focused checks successfully.",
        "Called the service and confirmed the result.",
        "Updated plan details are below.",
    ] {
        let mut state = ReplyState::default();
        assert!(state.reply(info, &frame(Some("question"), prose), 0, 0, &items).is_some(), "{prose}");
    }
    // Once the prompt scrolls out, observed growth anchors the current block.
    let mut state = ReplyState::default();
    assert_eq!(state.reply(info, &frame(None, "Working with"), 0, 0, &items), None);
    assert!(state.reply(info, &frame(None, "Working with streaming output."), 0, 0, &items).is_some());
    // Preserve the accepted prefix if even its opening bullet leaves the tail.
    let mut clipped = vec!["  continuation".to_string(); 512];
    let footer = frame(None, "");
    let end = footer.iter().position(|l| l.contains("esc to interrupt")).unwrap();
    let footer = &footer[end..];
    let start = clipped.len() - footer.len();
    clipped[start..].clone_from_slice(footer);
    assert!(state.reply(info, &clipped, 0, 0, &items).is_some());
    // Rewriting a turn resets the prior anchor.
    assert_eq!(state.reply(info, &frame(Some("question"), "Old text"), 1, 0, &[user("new task")]), None);
    for tool in ["Bash(cargo test)", "Ran cargo test\n  └ passed", "Explored\n  └ files"] {
        let mut state = ReplyState::default();
        assert_eq!(state.reply(info, &frame(Some("question"), tool), 0, 0, &items), None);
    }
}

#[test]
fn one_gateway_holds_the_lock() {
    let dir = TempDir::new("remote-lock");
    let held = take_lock(dir.path()).unwrap().expect("the first gateway takes the lock");
    assert!(running(dir.path()));
    assert!(take_lock(dir.path()).unwrap().is_none(), "a second gateway does not start");
    drop(held);
    assert!(!running(dir.path()));
    assert_eq!(fs::metadata(dir.path().join(store::LOCK)).unwrap().permissions().mode() & 0o777, 0o600);
}
