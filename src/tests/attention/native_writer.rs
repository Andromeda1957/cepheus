//! Real private PTY checks: full buffers never hold actor-visible locks or replay Enter.
use super::*;
use crate::testsupport::TempDir;
fn start(d: &TempDir, bytes: usize) -> (Hosted, Receiver<crate::service::Ev>) {
    let code = format!(
        r#"import os,tty,time,json
from pathlib import Path
tty.setraw(0)
import signal
signal.signal(signal.SIGHUP,signal.SIG_IGN)
os.write(1,b'writer-ready')
while not Path('release').exists(): time.sleep(.01)
data=b''
while len(data)<{bytes}:
 data+=os.read(0,min(65536,{bytes}-len(data)))
Path('received.json').write_text(json.dumps({{'length':len(data),'cr':data.count(b'\r')}}))
"#
    );
    let argv = vec!["python3".into(), "-u".into(), "-c".into(), code];
    let (tx, rx) = mpsc::channel();
    let h = Hosted::spawn(
        SpawnSpec {
            id: 91,
            argv: &argv,
            cwd: d.path(),
            env_set: &[],
            env_remove: &[],
            cols: 80,
            rows: 12,
            scrollback: 100,
        },
        tx,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !crate::screen::screen_text(&h.lock().term).join("\n").contains("writer-ready") {
        assert!(Instant::now() < deadline);
        let _ = rx.recv_timeout(Duration::from_millis(20));
        h.clear_notified();
    }
    (h, rx)
}
struct Owned(Hosted);
impl Drop for Owned {
    fn drop(&mut self) {
        self.0.signal(libc::SIGKILL);
    }
}
#[test]
fn full_manual_input_keeps_authority_and_emu_responsive_and_aborted_handover_preserves_tail() {
    let d = TempDir::new("attention-full-pty");
    let (h, _rx) = start(&d, 262144);
    let mut owned = Owned(h);
    let h = &mut owned.0;
    h.write(vec![b'x'; 262144]);
    let authority = Arc::new(Mutex::new(4));
    let (done, answer) = mpsc::channel();
    h.write_guarded(
        b"\r".to_vec(),
        Instant::now() + Duration::from_millis(10),
        h.input_epoch(),
        authority.clone(),
        4,
        |_| true,
        move |ok| {
            let _ = done.send(ok);
        },
    )
    .unwrap();
    // These are real actor-visible operations while the kernel input queue is full.
    {
        let mut rev = authority.lock().unwrap();
        *rev = 5;
    }
    let _ = h.lock().output_revision;
    h.revoke_guarded_input();
    assert!(h.pause().is_err(), "handover must refuse to discard the queued manual tail");
    h.resume().unwrap();
    std::fs::write(d.path().join("release"), b"go").unwrap();
    assert!(
        !answer.recv_timeout(Duration::from_secs(15)).unwrap(),
        "expired/revoked Enter must not be accepted after the manual tail drains"
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let receipt = loop {
        if let Ok(bytes) = std::fs::read(d.path().join("received.json")) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                break v;
            }
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(receipt["length"], 262144);
    assert_eq!(receipt["cr"], 0);
}
#[test]
fn guarded_short_write_is_unknown_and_never_replays_its_unwritten_tail() {
    let d = TempDir::new("attention-short-pty");
    let (h, _rx) = start(&d, 1);
    let owned = Owned(h);
    let h = &owned.0;
    let epoch = h.input_epoch();
    let (done, answer) = mpsc::channel();
    h.write_guarded(
        vec![b'x'; 262144],
        Instant::now() + Duration::from_secs(1),
        epoch,
        Arc::new(Mutex::new(1)),
        1,
        |_| true,
        move |ok| {
            let _ = done.send(ok);
        },
    )
    .unwrap();
    assert!(!answer.recv_timeout(Duration::from_secs(15)).unwrap());
    assert!(h.input_epoch() > epoch, "partial actual acceptance changes the input fence");
    // Kernel bytes are the sole accepted prefix. There is no automatic remainder job.
    let input = unsafe { libc::fcntl(h.master.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(input & libc::O_NONBLOCK, 0);
    std::fs::write(d.path().join("release"), b"go").unwrap();
}

#[test]
fn owner_signal_revokes_a_prepared_permission_before_final_native_acceptance() {
    let d = TempDir::new("attention-close-revoke");
    let (h, _rx) = start(&d, 1);
    let owned = Owned(h);
    let h = &owned.0;
    let (prepared, entered) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let (done, answer) = mpsc::channel();
    h.write_guarded_prepared(
        b"\r".to_vec(),
        Instant::now() + Duration::from_secs(10),
        h.input_epoch(),
        Arc::new(Mutex::new(1)),
        1,
        move || {
            prepared.send(()).unwrap();
            wait.recv().unwrap();
            true
        },
        |_| true,
        move |ok| {
            let _ = done.send(ok);
        },
    )
    .unwrap();
    entered.recv_timeout(Duration::from_secs(15)).unwrap();
    h.signal(libc::SIGHUP);
    let _ = h.lock().output_revision;
    release.send(()).unwrap();
    assert!(!answer.recv_timeout(Duration::from_secs(15)).unwrap());
    std::fs::write(d.path().join("release"), b"go").unwrap();
    h.write(b"x".to_vec());
}
#[test]
fn caller_expired_deadline_is_never_extended_by_native_queueing() {
    let d = TempDir::new("attention-absolute-deadline");
    let (h, _rx) = start(&d, 1);
    let owned = Owned(h);
    let h = &owned.0;
    let (done, answer) = mpsc::channel();
    h.write_guarded(
        b"\r".to_vec(),
        Instant::now() - Duration::from_millis(1),
        h.input_epoch(),
        Arc::new(Mutex::new(1)),
        1,
        |_| true,
        move |ok| {
            let _ = done.send(ok);
        },
    )
    .unwrap();
    assert!(!answer.recv_timeout(Duration::from_secs(15)).unwrap());
    std::fs::write(d.path().join("release"), b"go").unwrap();
    h.write(b"x".to_vec());
}
