//! One hosted program: its pseudo-terminal, child process and emulator.
//!
//! The session keeps its own copy of the terminal's master descriptor and
//! runs its reading, writing and waiting on threads that can be stopped and
//! started again. That lets the service hand a running program to its next
//! build (`handover.rs`): the threads stop with the program's unread output
//! still in the kernel, the descriptor survives `exec`, and the new build
//! adopts the program by descriptor and process id.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Osc52, Term};
use alacritty_terminal::vte::ansi::{Processor, Rgb, StdSyncHandler};
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::protocol::ScrollOp;

pub type ColorFormatter = Arc<dyn Fn(Rgb) -> String + Send + Sync>;
pub type SizeFormatter = Arc<dyn Fn(WindowSize) -> String + Send + Sync>;

/// How often a stopped-or-not check interrupts a quiet reader, and how often
/// the waiter looks for the program's exit.
const IO_TICK: Duration = Duration::from_millis(50);

/// Events from session threads to the service loop.
pub enum SessionEvent {
    Output(u64),
    Exited(u64, Option<u32>),
    Title(u64, Option<String>),
    Clipboard(u64, String),
    Bell(u64),
    ColorRequest(u64, usize, ColorFormatter),
    SizeRequest(u64, SizeFormatter),
}

/// What the writer thread is asked to do.
pub enum Input {
    Data(Vec<u8>),
    Guarded {
        data: Vec<u8>,
        deadline: Instant,
        epoch: u64,
        authority: Arc<Mutex<u64>>,
        revision: u64,
        prepare: Box<dyn FnOnce() -> bool + Send>,
        check: Box<dyn FnOnce(&Emu) -> bool + Send>,
        done: Box<dyn FnOnce(bool) + Send>,
    },
    /// Write everything queued before this, then stop and say so.
    Stop(Sender<()>, Arc<Mutex<u8>>),
}

/// Receives emulator callbacks while the reader thread holds the emulator lock.
pub struct Listener {
    id: u64,
    events: Sender<crate::service::Ev>,
    input: Sender<Input>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let ev = match event {
            Event::PtyWrite(text) => {
                let _ = self.input.send(Input::Data(text.into_bytes()));
                return;
            }
            Event::Title(title) => SessionEvent::Title(self.id, Some(title)),
            Event::ResetTitle => SessionEvent::Title(self.id, None),
            Event::ClipboardStore(_, text) => SessionEvent::Clipboard(self.id, text),
            Event::ColorRequest(index, fmt) => SessionEvent::ColorRequest(self.id, index, fmt),
            Event::TextAreaSizeRequest(fmt) => SessionEvent::SizeRequest(self.id, fmt),
            Event::Bell => SessionEvent::Bell(self.id),
            _ => return,
        };
        let _ = self.events.send(crate::service::Ev::Session(ev));
    }
}

pub struct Emu {
    /// Parsed PTY output only: view scrolling/resizing never advances this fence.
    pub output_revision: u64,
    pub term: Term<Listener>,
    pub parser: Processor<StdSyncHandler>,
}

pub struct SpawnSpec<'a> {
    pub id: u64,
    pub argv: &'a [String],
    pub cwd: &'a Path,
    pub env_set: &'a [(String, String)],
    pub env_remove: &'a [String],
    pub cols: u16,
    pub rows: u16,
    pub scrollback: usize,
}

/// A running program handed over by the previous build of the service.
pub struct AdoptSpec {
    pub id: u64,
    /// The terminal's master descriptor, inherited across `exec`.
    pub fd: RawFd,
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
    pub scrollback: usize,
    /// Escape sequences that redraw the screen and scrollback it had
    /// (`screen::replay`).
    pub screen: String,
}

/// The running reader, writer and waiter threads.
struct Io {
    stop: Arc<AtomicBool>,
    reader: JoinHandle<()>,
    writer: JoinHandle<Receiver<Input>>,
    waiter: JoinHandle<()>,
}

pub struct Hosted {
    pub id: u64,
    /// An owned incarnation fence, renewed even when a reopened slot reuses
    /// its id, launch timestamp or PID. Views discard snapshots across adoption.
    pub history_epoch: u64,
    pub emu: Arc<Mutex<Emu>>,
    pub generation: Arc<AtomicU64>,
    input_epoch: Arc<Mutex<u64>>,
    notified: Arc<AtomicBool>,
    master: OwnedFd,
    input: Sender<Input>,
    events: Sender<crate::service::Ev>,
    io: Option<Io>,
    /// The writer's queue while paused, kept for `resume`.
    paused: Option<Receiver<Input>>,
    pub pid: Option<u32>,
    /// The command line this service started the program with; none for a
    /// program adopted from an earlier run of the service.
    pub argv: Option<Vec<String>>,
    pub cols: u16,
    pub rows: u16,
}

/// A close-on-exec duplicate of `fd`.
fn dup_cloexec(fd: RawFd) -> std::io::Result<OwnedFd> {
    // SAFETY: fcntl only reads `fd`; a valid result is a new descriptor we own.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if new < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `new` was just returned by fcntl and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

/// Set or clear close-on-exec on `fd`.
pub fn set_cloexec(fd: RawFd, on: bool) -> std::io::Result<()> {
    // SAFETY: F_GETFD/F_SETFD only change the flags of `fd`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC };
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Whether `fd` has input (or hung up) within `timeout`.
pub fn readable(fd: RawFd, timeout: Duration) -> std::io::Result<bool> {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: poll reads and writes the one pollfd we pass.
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
    match rc {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        _ => {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(err)
            }
        }
    }
}

/// The exit code of `pid` if it has ended: `Ok(Some(code))` once reaped,
/// `Ok(None)` while it runs, `Err` when it is not this process's child.
/// A program killed by a signal reports 1, as portable-pty did.
pub fn try_wait(pid: u32) -> std::io::Result<Option<u32>> {
    let mut status: libc::c_int = 0;
    // SAFETY: waitpid writes the status of our own child into `status`.
    let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    if rc == 0 {
        return Ok(None);
    }
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Some(if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) as u32 } else { 1 }))
}

impl Hosted {
    pub fn spawn(spec: SpawnSpec<'_>, events: Sender<crate::service::Ev>) -> Result<Hosted> {
        let argv = spec.argv;
        anyhow::ensure!(!argv.is_empty(), "empty command");
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize { rows: spec.rows.max(2), cols: spec.cols.max(10), pixel_width: 0, pixel_height: 0 })
            .context("opening a pseudo-terminal")?;
        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.cwd(spec.cwd);
        for key in spec.env_remove {
            cmd.env_remove(key);
        }
        for (key, value) in spec.env_set {
            cmd.env(key, value);
        }
        let child = pair.slave.spawn_command(cmd).with_context(|| format!("starting {}", argv[0]))?;
        drop(pair.slave);
        let pid = child.process_id();
        // Keep a descriptor of our own: portable-pty's master closes its copy
        // when dropped, and the child is reaped with waitpid by pid, which
        // works the same for a program adopted after an update.
        let raw = pair.master.as_raw_fd().context("the pseudo-terminal has no descriptor")?;
        let master = dup_cloexec(raw).context("copying the terminal descriptor")?;
        drop(pair.master);
        drop(child);
        let cols = spec.cols.max(10);
        let rows = spec.rows.max(2);
        let mut hosted = Hosted::start(spec.id, master, pid, cols, rows, spec.scrollback, "", events)?;
        hosted.argv = Some(argv.to_vec());
        Ok(hosted)
    }

    /// Take over a program the previous build of the service was hosting.
    pub fn adopt(spec: AdoptSpec, events: Sender<crate::service::Ev>) -> Result<Hosted> {
        anyhow::ensure!(spec.fd > 2, "descriptor {} is not a terminal of a session", spec.fd);
        // SAFETY: the previous build passed this descriptor to us for this
        // session alone; nothing else in this process owns it.
        let master = unsafe { OwnedFd::from_raw_fd(spec.fd) };
        set_cloexec(master.as_raw_fd(), true)?;
        Hosted::start(
            spec.id,
            master,
            Some(spec.pid),
            spec.cols.max(10),
            spec.rows.max(2),
            spec.scrollback,
            &spec.screen,
            events,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        id: u64,
        master: OwnedFd,
        pid: Option<u32>,
        cols: u16,
        rows: u16,
        scrollback: usize,
        screen: &str,
        events: Sender<crate::service::Ev>,
    ) -> Result<Hosted> {
        let (input_tx, input_rx) = mpsc::channel::<Input>();
        let listener = Listener { id, events: events.clone(), input: input_tx.clone() };
        let config = TermConfig {
            scrolling_history: scrollback,
            kitty_keyboard: false,
            osc52: Osc52::OnlyCopy,
            ..TermConfig::default()
        };
        let size = TermSize::new(cols as usize, rows as usize);
        let term = Term::new(config, &size, listener);
        let mut emu = Emu { output_revision: 0, term, parser: Processor::new() };
        if !screen.is_empty() {
            let Emu { term, parser, .. } = &mut emu;
            parser.advance(term, screen.as_bytes());
        }
        let mut hosted = Hosted {
            id,
            history_epoch: {
                static NEXT: std::sync::OnceLock<AtomicU64> = std::sync::OnceLock::new();
                NEXT.get_or_init(|| {
                    AtomicU64::new(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64,
                    )
                })
                .fetch_add(1, Ordering::SeqCst)
            },
            emu: Arc::new(Mutex::new(emu)),
            generation: Arc::new(AtomicU64::new(1)),
            input_epoch: Arc::new(Mutex::new(0)),
            notified: Arc::new(AtomicBool::new(false)),
            master,
            input: input_tx,
            events,
            io: None,
            paused: None,
            pid,
            argv: None,
            cols,
            rows,
        };
        hosted.start_io(input_rx)?;
        Ok(hosted)
    }

    /// Start reading the program's output, writing its input and waiting
    /// for it to exit.
    fn start_io(&mut self, input_rx: Receiver<Input>) -> Result<()> {
        let id = self.id;
        let stop = Arc::new(AtomicBool::new(false));
        let flags = unsafe { libc::fcntl(self.master.as_raw_fd(), libc::F_GETFL) };
        anyhow::ensure!(
            flags >= 0 && unsafe { libc::fcntl(self.master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
            "terminal nonblocking mode unavailable"
        );
        let mut reader = File::from(self.master.try_clone().context("copying the terminal reader")?);
        let mut writer = File::from(self.master.try_clone().context("copying the terminal writer")?);

        let write_emu = self.emu.clone();
        let write_epoch = self.input_epoch.clone();
        let writer = thread::Builder::new().name(format!("pty-write-{id}")).spawn(move || {
            while let Ok(msg) = input_rx.recv() {
                match msg {
                    Input::Data(data) => {
                        let mut offset = 0;
                        while offset < data.len() {
                            match writer.write(&data[offset..]) {
                                Ok(0) => break,
                                Ok(n) => offset += n,
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    let mut poll =
                                        libc::pollfd { fd: writer.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                                    unsafe {
                                        libc::poll(&mut poll, 1, 10);
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    Input::Guarded { data, deadline, epoch, authority, revision, prepare, check, done } => {
                        // Wait before locks; the final attempt is one nonblocking
                        // syscall. Short/zero writes are uncertain and never replayed.
                        let fd = writer.as_raw_fd();
                        while Instant::now() < deadline {
                            let mut p = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
                            if unsafe { libc::poll(&mut p, 1, 5) } > 0 {
                                break;
                            }
                        }
                        let prepared = Instant::now() < deadline && prepare();
                        let accepted = {
                            let emu = write_emu.lock().unwrap_or_else(|e| e.into_inner());
                            let mut current = write_epoch.lock().unwrap_or_else(|e| e.into_inner());
                            let auth = authority.lock().unwrap_or_else(|e| e.into_inner());
                            if !prepared
                                || Instant::now() >= deadline
                                || *current != epoch
                                || *auth != revision
                                || readable(fd, Duration::ZERO).unwrap_or(true)
                                || !check(&emu)
                            {
                                false
                            } else {
                                let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
                                if n > 0 {
                                    *current = current.saturating_add(1);
                                }
                                n == data.len() as isize
                            }
                        };
                        done(accepted);
                    }
                    Input::Stop(done, state) => {
                        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                        if *state == 0 {
                            *state = 2;
                            let _ = done.send(());
                            break;
                        }
                    }
                }
            }
            input_rx
        })?;

        let reader = {
            let emu = self.emu.clone();
            let generation = self.generation.clone();
            let notified = self.notified.clone();
            let events = self.events.clone();
            let stop = stop.clone();
            thread::Builder::new().name(format!("pty-read-{id}")).spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                let fd = reader.as_raw_fd();
                while !stop.load(Ordering::SeqCst) {
                    // Wait in poll, not read, so a stop leaves unread output
                    // in the kernel for whoever reads next.
                    match readable(fd, IO_TICK) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(_) => break,
                    }
                    let mut guard = emu.lock().unwrap_or_else(|e| e.into_inner());
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            {
                                let Emu { term, parser, output_revision } = &mut *guard;
                                parser.advance(term, &buf[..n]);
                                *output_revision = output_revision.saturating_add(1);
                            }
                            generation.fetch_add(1, Ordering::SeqCst);
                            if !notified.swap(true, Ordering::SeqCst) {
                                let _ = events.send(crate::service::Ev::Session(SessionEvent::Output(id)));
                            }
                        }
                        Err(e)
                            if matches!(e.kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock) =>
                        {
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            })?
        };

        let waiter = {
            let events = self.events.clone();
            let stop = stop.clone();
            let pid = self.pid;
            thread::Builder::new().name(format!("pty-wait-{id}")).spawn(move || {
                let Some(pid) = pid else { return };
                while !stop.load(Ordering::SeqCst) {
                    match try_wait(pid) {
                        Ok(None) => thread::sleep(IO_TICK),
                        Ok(Some(code)) => {
                            let _ = events.send(crate::service::Ev::Session(SessionEvent::Exited(id, Some(code))));
                            return;
                        }
                        Err(_) => {
                            let _ = events.send(crate::service::Ev::Session(SessionEvent::Exited(id, None)));
                            return;
                        }
                    }
                }
            })?
        };
        self.io = Some(Io { stop, reader, writer, waiter });
        Ok(())
    }

    /// Stop reading, writing and waiting so the next build can take over the
    /// terminal: input already queued is written first, and output the
    /// reader has not taken stays unread in the kernel. Input sent while
    /// paused waits for `resume`. Returns the master descriptor to hand over.
    pub fn pause(&mut self) -> Result<RawFd> {
        self.revoke_guarded_input();
        if let Some(io) = self.io.as_ref() {
            let (done_tx, done_rx) = mpsc::channel();
            let state = Arc::new(Mutex::new(0u8));
            self.input
                .send(Input::Stop(done_tx, state.clone()))
                .map_err(|_| anyhow::anyhow!("terminal writer unavailable"))?;
            if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                if *state == 0 {
                    *state = 1;
                    anyhow::bail!("manual terminal input has not drained; handover held")
                }
            }
            io.stop.store(true, Ordering::SeqCst);
            let io = self.io.take().unwrap();
            let _ = io.reader.join();
            let _ = io.waiter.join();
            let queue = io.writer.join().map_err(|_| anyhow::anyhow!("terminal writer failed"))?;
            self.paused = Some(queue);
        }
        Ok(self.master.as_raw_fd())
    }

    /// Start the threads again after `pause`, when the handover did not happen.
    pub fn resume(&mut self) -> Result<()> {
        match self.paused.take() {
            Some(queue) => self.start_io(queue),
            None => Ok(()),
        }
    }

    /// Give up the paused terminal without closing it, as the next build
    /// does when it takes over in the same process in tests.
    #[cfg(test)]
    pub fn release(self) -> RawFd {
        use std::os::fd::IntoRawFd;
        self.master.into_raw_fd()
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, Emu> {
        match self.emu.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Allow the reader thread to send the next output wake-up.
    pub fn clear_notified(&self) {
        self.notified.store(false, Ordering::SeqCst);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn bump(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn write(&self, data: Vec<u8>) {
        let mut epoch = self.input_epoch.lock().unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.saturating_add(1);
        let _ = self.input.send(Input::Data(data));
    }
    pub fn revoke_guarded_input(&self) {
        let mut e = self.input_epoch.lock().unwrap_or_else(|e| e.into_inner());
        *e = e.saturating_add(1);
    }
    pub(crate) fn input_fence(&self) -> Arc<Mutex<u64>> {
        self.input_epoch.clone()
    }
    pub fn input_epoch(&self) -> u64 {
        *self.input_epoch.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn write_guarded(
        &self,
        data: Vec<u8>,
        deadline: Instant,
        epoch: u64,
        authority: Arc<Mutex<u64>>,
        revision: u64,
        check: impl FnOnce(&Emu) -> bool + Send + 'static,
        done: impl FnOnce(bool) + Send + 'static,
    ) -> Result<()> {
        self.write_guarded_prepared(data, deadline, epoch, authority, revision, || true, check, done)
    }
    pub fn write_guarded_prepared(
        &self,
        data: Vec<u8>,
        deadline: Instant,
        epoch: u64,
        authority: Arc<Mutex<u64>>,
        revision: u64,
        prepare: impl FnOnce() -> bool + Send + 'static,
        check: impl FnOnce(&Emu) -> bool + Send + 'static,
        done: impl FnOnce(bool) + Send + 'static,
    ) -> Result<()> {
        self.input
            .send(Input::Guarded {
                data,
                deadline: deadline.min(Instant::now() + Duration::from_millis(150)),
                epoch,
                authority,
                revision,
                prepare: Box::new(prepare),
                check: Box::new(check),
                done: Box::new(done),
            })
            .map_err(|_| anyhow::anyhow!("terminal writer unavailable"))
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(10);
        let rows = rows.max(2);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: TIOCSWINSZ reads the winsize we pass; the program gets SIGWINCH.
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size);
        }
        self.lock().term.resize(TermSize::new(cols as usize, rows as usize));
        self.bump();
    }

    pub fn scroll(&self, op: ScrollOp) {
        let scroll = match op {
            ScrollOp::Lines(n) => Scroll::Delta(n),
            ScrollOp::PageUp => Scroll::PageUp,
            ScrollOp::PageDown => Scroll::PageDown,
            ScrollOp::Top => Scroll::Top,
            ScrollOp::Bottom => Scroll::Bottom,
        };
        self.lock().term.scroll_display(scroll);
        self.bump();
    }

    /// Leave scrollback when the user types, as terminals do.
    pub fn follow_output(&self) {
        let mut emu = self.lock();
        if emu.term.grid().display_offset() != 0 {
            emu.term.scroll_display(Scroll::Bottom);
            drop(emu);
            self.bump();
        }
    }

    /// End a pending synchronized update whose timeout passed.
    pub fn check_sync_timeout(&self, now: std::time::Instant) {
        let mut guard = self.lock();
        let due = guard.parser.sync_timeout().sync_timeout().is_some_and(|t| now >= t);
        if due {
            let Emu { term, parser, .. } = &mut *guard;
            parser.stop_sync(term);
            drop(guard);
            self.bump();
        }
    }

    /// Send a signal to the child's process group.
    pub fn signal(&self, sig: libc::c_int) {
        self.revoke_guarded_input();
        if let Some(pid) = self.pid {
            // SAFETY: kill has no memory-safety preconditions; a negative pid
            // addresses the process group the pty spawner created with setsid.
            unsafe {
                libc::kill(-(pid as libc::pid_t), sig);
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/session.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/attention/native_writer.rs"]
mod attention_writer_tests;
