//! The reader runs in a child copy of this test binary whose standard input
//! is a pseudo-terminal, so it reads fd 0 just as the deck does. The parent
//! types into the terminal and collects the events the child prints.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

const CHILD: &str = "CEPHEUS_INPUT_CHILD";
const READY: &str = "input-reader-ready";
const PREFIX: &str = "input-event: ";
/// How long the child waits without an event before it exits.
const IDLE: Duration = Duration::from_millis(700);
/// The pause between typed chunks that a network can put inside a sequence.
const GAP: Duration = Duration::from_millis(20);

/// The reader under test when `CEPHEUS_INPUT_CHILD` is set; otherwise
/// nothing.
#[test]
fn terminal_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    super::spawn(move |ev| tx.send(ev).is_ok()).unwrap();
    println!("\n{READY}");
    while let Ok(ev) = rx.recv_timeout(IDLE) {
        println!("\n{PREFIX}{ev:?}");
    }
}

fn open_terminal() -> (File, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    let size = libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let opened = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &size) };
    assert_eq!(opened, 0, "openpty: {}", io::Error::last_os_error());
    let mut attrs = unsafe { std::mem::zeroed::<libc::termios>() };
    unsafe {
        assert_eq!(libc::tcgetattr(slave, &mut attrs), 0);
        libc::cfmakeraw(&mut attrs);
        assert_eq!(libc::tcsetattr(slave, libc::TCSANOW, &attrs), 0);
        (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
    }
}

/// Types each chunk into a terminal read by a child running `spawn`, `GAP`
/// apart, then runs `then` on the terminal, and returns the debug form of
/// each event the child read.
fn typed(chunks: &[&[u8]], then: impl FnOnce(&File)) -> Vec<String> {
    let (mut master, slave) = open_terminal();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["input::tests::terminal_child", "--exact", "--nocapture", "--test-threads", "1"])
        .env(CHILD, "1")
        .stdin(Stdio::from(slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // The terminal becomes the child's controlling terminal, so a resize
    // signals it and crossterm's size query reads it.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    drop(cmd);
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok);
    assert!(lines.by_ref().any(|line| line == READY), "the child reader never started");
    // Input that arrives before the reader first waits would hide a split.
    thread::sleep(Duration::from_millis(100));
    for (i, chunk) in chunks.iter().enumerate() {
        if i > 0 {
            thread::sleep(GAP);
        }
        master.write_all(chunk).unwrap();
    }
    then(&master);
    let events = lines.filter_map(|line| line.strip_prefix(PREFIX).map(str::to_owned)).collect();
    child.wait().unwrap();
    events
}

fn debug(events: &[Event]) -> Vec<String> {
    events.iter().map(|event| format!("{event:?}")).collect()
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn wheel(kind: MouseEventKind) -> Event {
    Event::Mouse(MouseEvent { kind, column: 9, row: 4, modifiers: KeyModifiers::NONE })
}

#[test]
fn a_burst_longer_than_one_crossterm_read_arrives_whole() {
    // Over SSH a scroll flood arrives at once; crossterm read 1 KiB of this
    // 1,650-byte burst and left the rest until the next key.
    let burst = b"\x1b[<65;10;5M".repeat(150);
    let events = typed(&[&burst], |_| {});
    assert_eq!(events, debug(&vec![wheel(MouseEventKind::ScrollDown); 150]));
}

#[test]
fn a_sequence_split_after_its_esc_stays_one_event() {
    // crossterm took the ESC as the Esc key and typed the rest as text.
    let events = typed(&[b"\x1b", b"[<64;10;5M"], |_| {});
    assert_eq!(events, debug(&[wheel(MouseEventKind::ScrollUp)]));
}

#[test]
fn a_bare_esc_is_the_esc_key() {
    assert_eq!(typed(&[b"\x1b"], |_| {}), debug(&[key(KeyCode::Esc, KeyModifiers::NONE)]));
}

#[test]
fn two_quick_escs_stay_two() {
    let esc = key(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(typed(&[b"\x1b", b"\x1b"], |_| {}), debug(&[esc.clone(), esc]));
}

#[test]
fn esc_and_then_a_letter_is_not_alt() {
    let events = typed(&[b"\x1b", b"x"], |_| {});
    assert_eq!(events, debug(&[key(KeyCode::Esc, KeyModifiers::NONE), key(KeyCode::Char('x'), KeyModifiers::NONE)]));
}

#[test]
fn alt_and_a_letter_in_one_write_is_alt() {
    assert_eq!(typed(&[b"\x1bx"], |_| {}), debug(&[key(KeyCode::Char('x'), KeyModifiers::ALT)]));
}

#[test]
fn a_paste_longer_than_one_crossterm_read_is_one_paste() {
    let text = "a".repeat(2000);
    let paste = [b"\x1b[200~", text.as_bytes(), b"\x1b[201~"].concat();
    assert_eq!(typed(&[&paste], |_| {}), debug(&[Event::Paste(text)]));
}

#[test]
fn kitty_keys_still_parse() {
    assert_eq!(typed(&[b"\x1b[119;5u"], |_| {}), debug(&[key(KeyCode::Char('w'), KeyModifiers::CONTROL)]));
}

#[test]
fn a_resize_reports_the_new_size() {
    let events = typed(&[], |master| {
        let size = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
        assert_eq!(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) }, 0);
    });
    assert_eq!(events, debug(&[Event::Resize(100, 30)]));
}
