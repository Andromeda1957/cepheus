use super::tests::{options, screen_of, wait_for_screen};
use super::*;
use crate::testsupport::TempDir;

// The actual phone path pastes into the hosted editor, which may already hold
// a draft. This PTY editor retains multiline input and implements Codex's
// one-Ctrl+C draft reset. It records what Enter submitted, not just key bytes.
fn editor(dir: &TempDir, draft: &str) -> (Service, u64, Receiver<Ev>, PathBuf) {
    let source = r#"
import json, os, sys, tty
from pathlib import Path
tty.setraw(0)
text = sys.argv[2]
record = Path(sys.argv[1])
pasting = False
escape = b''
def draw():
    rows = text.split('\n') if text else ['Ask Codex to do anything']
    view = '\x1b[2J\x1b[H\x1b[?2004hready\r\n\r\n'
    view += '› ' + '\r\n  '.join(rows)
    view += '\r\n\r\n  GPT-6.1-Sol xhigh · Test conversation\r\n  ? for shortcuts'
    view += '\x1b[%d;%dH' % (3 + len(rows) - 1, 3 + len(rows[-1]) if text else 3)
    os.write(1, view.encode())
draw()
while True:
    key = os.read(0, 1)
    if not key: break
    if escape or key == b'\x1b':
        escape += key
        if escape == b'\x1b[200~': pasting = True; escape = b''
        elif escape == b'\x1b[201~': pasting = False; escape = b''; draw()
        continue
    if key == b'\x03' and not pasting:
        if not text: sys.exit(3)
        text = ''; draw()
    elif key == b'\r' and not pasting:
        with record.open('a') as out: out.write(json.dumps(text) + '\n')
        if text.strip() in ('/clear', '/new'):
            text = ''
            os.write(1, b'\x1b[2J\x1b[H  >_ OpenAI Codex (v0.166.0)\r\n  permissions: YOLO mode\r\n\r\n\xe2\x80\xba ')
        else:
            draw()
    else:
        text += key.decode(errors='replace')
        if not pasting: draw()
"#;
    dir.write("editor.py", source);
    let record = dir.path().join("submitted.jsonl");
    let mut opts = options(dir, "");
    opts.config.codex_command = vec![
        "python3".into(),
        dir.path().join("editor.py").to_string_lossy().into_owned(),
        record.to_string_lossy().into_owned(),
        draft.into(),
    ];
    let (tx, rx) = mpsc::channel();
    let mut service = Service::new(opts, tx);
    let id = service.create(Tool::Codex, None).unwrap();
    wait_for_screen(&service, id, "Test conversation");
    service.sessions.get_mut(&id).unwrap().conversation = Some("saved-conversation".into());
    (service, id, rx, record)
}

#[test]
fn phone_clear_replaces_stale_single_and_multiline_drafts_before_submitting() {
    for draft in ["", "/clearunfinished message", "unfinished message\n\n/synthetic/image.jpg"] {
        let dir = TempDir::new("phone-clear-draft");
        let (mut svc, id, rx, record) = editor(&dir, draft);
        // Rapid repeated taps must not send a second Ctrl+C to an empty
        // composer or append the command before the first delayed Enter.
        for _ in 0..3 {
            svc.broadcast_message(0, &[id], "/clear");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            while let Ok(event) = rx.try_recv() {
                svc.handle(event);
            }
            svc.submit_due(Instant::now() + SUBMIT_DELAY);
            svc.refresh_status(Instant::now());
            if svc.sessions[&id].conversation.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let submitted = fs::read_to_string(&record).unwrap_or_default();
        let observed = (svc.sessions[&id].conversation.clone(), screen_of(&svc, id));
        svc.sessions[&id].hosted.as_ref().unwrap().signal(libc::SIGKILL);
        assert_eq!(submitted, "\"/clear\"\n", "draft {draft:?}: submitted the exact command once");
        assert_eq!(observed.0, None, "Clear resets phone conversation identity: {}", observed.1);
    }
}
