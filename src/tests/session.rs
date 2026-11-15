use super::*;
use crate::service::Ev;
use std::path::PathBuf;
use std::time::Instant;

fn argv(script: &str) -> Vec<String> {
    vec!["sh".to_string(), "-c".to_string(), script.to_string()]
}

fn spawn(id: u64, script: &str, events: Sender<Ev>) -> Hosted {
    let argv = argv(script);
    let cwd = PathBuf::from("/");
    let spec =
        SpawnSpec { id, argv: &argv, cwd: &cwd, env_set: &[], env_remove: &[], cols: 40, rows: 6, scrollback: 100 };
    Hosted::spawn(spec, events).expect("spawn sh")
}

fn text(hosted: &Hosted) -> String {
    crate::screen::screen_text(&hosted.lock().term).join("\n")
}

/// Wait until the screen shows `want`, answering output wake-ups as the
/// service does. Returns an exit event seen on the way.
fn wait_for_text(hosted: &Hosted, events: &Receiver<Ev>, want: &str) -> Option<Option<u32>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = None;
    while !text(hosted).contains(want) {
        assert!(Instant::now() < deadline, "screen never showed {want:?}:\n{}", text(hosted));
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(Ev::Session(SessionEvent::Output(_))) => hosted.clear_notified(),
            Ok(Ev::Session(SessionEvent::Exited(_, code))) => exited = Some(code),
            _ => {}
        }
    }
    exited
}

fn wait_for_exit(hosted: &Hosted, events: &Receiver<Ev>) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "the program never exited:\n{}", text(hosted));
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(Ev::Session(SessionEvent::Output(_))) => hosted.clear_notified(),
            Ok(Ev::Session(SessionEvent::Exited(id, code))) => {
                assert_eq!(id, hosted.id);
                return code;
            }
            _ => {}
        }
    }
}

/// Bytes the program wrote that nobody has read yet.
fn unread(fd: RawFd) -> usize {
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD writes the count into `n`.
    unsafe { libc::ioctl(fd, libc::FIONREAD, &mut n) };
    n.max(0) as usize
}

#[test]
fn a_paused_session_is_adopted_with_its_screen_unread_output_input_and_exit() {
    let (tx, rx) = mpsc::channel();
    let script = "printf 'one\\n'; read a; printf 'two:%s\\n' \"$a\"; read b; exit 7";
    let mut old = spawn(1, script, tx);
    wait_for_text(&old, &rx, "one");

    let fd = old.pause().expect("pause");
    // While nothing reads the terminal the program answers input; its
    // output must wait in the kernel for the next build.
    // SAFETY: writing a byte slice to the descriptor we own.
    let sent = unsafe { libc::write(fd, b"x\n".as_ptr().cast(), 2) };
    assert_eq!(sent, 2);
    let deadline = Instant::now() + Duration::from_secs(10);
    // The echo of "x" and "two:x", each ending in CR LF.
    while unread(fd) < "x\r\ntwo:x\r\n".len() {
        assert!(Instant::now() < deadline, "the program did not answer while paused");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!text(&old).contains("two:x"), "a paused session reads nothing");

    let screen = crate::screen::replay(&old.lock().term);
    let (pid, cols, rows) = (old.pid.expect("pid"), old.cols, old.rows);
    let fd = old.release();
    drop(rx);

    let (tx, rx) = mpsc::channel();
    let adopted = Hosted::adopt(AdoptSpec { id: 1, fd, pid, cols, rows, scrollback: 100, screen }, tx).expect("adopt");
    assert_eq!(adopted.pid, Some(pid));
    wait_for_text(&adopted, &rx, "two:x");
    assert!(text(&adopted).starts_with("one\nx\ntwo:x"), "the old screen comes first:\n{}", text(&adopted));
    adopted.write(b"y\n".to_vec());
    assert_eq!(wait_for_exit(&adopted, &rx), Some(7), "the adopted program's exit code is reported");
}

#[test]
fn a_session_resumed_after_a_failed_handover_keeps_input_sent_meanwhile() {
    let (tx, rx) = mpsc::channel();
    let mut hosted = spawn(2, "printf 'ready\\n'; read a; printf 'got:%s\\n' \"$a\"; exit 3", tx);
    wait_for_text(&hosted, &rx, "ready");
    let fd = hosted.pause().expect("pause");
    // The reader can stop with the end of "ready" still unread.
    let before = unread(fd);
    hosted.write(b"held\n".to_vec());
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(unread(fd), before, "input sent while paused waits, so neither its echo nor an answer arrives");
    hosted.resume().expect("resume");
    let early_exit = wait_for_text(&hosted, &rx, "got:held");
    let code = early_exit.unwrap_or_else(|| wait_for_exit(&hosted, &rx));
    assert_eq!(code, Some(3));
}
