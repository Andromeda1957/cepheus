//! The deck's terminal input reader.
//!
//! crossterm's own reader loses input when it arrives in batches, as it does
//! over SSH. It wakes on edge-triggered epoll but reads at most 1 KiB per
//! wakeup, so the rest of a burst (a scroll flood, any-motion mouse reports)
//! stays unread until the next keystroke, and every later key queues behind
//! it. It also takes a read that ends in a bare ESC as the Esc key, so an
//! escape sequence split across two reads becomes Esc plus typed characters.
//! This reader polls level-triggered and drains everything, holds a trailing
//! bare ESC for `ESC_WAIT`, and leaves the parsing to crossterm's own parser
//! (`input/parse.rs`).

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode};

mod parse;

use parse::{parse_event, InternalEvent};

/// How long a bare ESC at the end of a read waits for the rest of an escape
/// sequence before it counts as the Esc key.
pub const ESC_WAIT: Duration = Duration::from_millis(100);

/// The most bytes one read takes; a longer burst takes several reads.
const READ_SIZE: usize = 8192;

/// Starts the thread that reads the terminal and hands each event to
/// `deliver`, until `deliver` returns false or the terminal closes. A resize
/// arrives as `Event::Resize` with the new size, as crossterm reports it.
///
/// From then on this thread takes every answer the terminal sends, so
/// crossterm's own queries (`cursor::position`, which ratatui's
/// `Terminal::clear` asks, and `supports_keyboard_enhancement`) must come
/// before it: after it they wait two seconds and fail.
pub fn spawn(deliver: impl FnMut(Event) -> bool + Send + 'static) -> io::Result<()> {
    let fd = tty_fd()?;
    let (winch, wake) = UnixStream::pair()?;
    winch.set_nonblocking(true)?;
    signal_hook::low_level::pipe::register(libc::SIGWINCH, wake)?;
    thread::Builder::new().name("deck-input".into()).spawn(move || run(fd, &winch, ESC_WAIT, deliver))?;
    Ok(())
}

/// The terminal, found as crossterm finds it: standard input when that is a
/// terminal, otherwise the controlling terminal, kept open for the process.
fn tty_fd() -> io::Result<RawFd> {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        return Ok(libc::STDIN_FILENO);
    }
    Ok(OpenOptions::new().read(true).write(true).open("/dev/tty")?.into_raw_fd())
}

/// Reads `fd` until it closes or `deliver` refuses an event. Each byte on
/// `winch` reports a resize.
fn run(fd: RawFd, winch: &UnixStream, esc_wait: Duration, mut deliver: impl FnMut(Event) -> bool) {
    let mut parser = Parser::default();
    let mut buf = vec![0; READ_SIZE];
    let mut esc_deadline: Option<Instant> = None;
    loop {
        let mut fds = [
            libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: winch.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let timeout = esc_deadline.map_or(-1, |deadline| {
            let left = deadline.saturating_duration_since(Instant::now());
            i32::try_from(left.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX)
        });
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[1].revents != 0 {
            let mut sink = [0; 64];
            while matches!((&*winch).read(&mut sink), Ok(n) if n > 0) {}
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                parser.events.push_back(InternalEvent::Event(Event::Resize(cols, rows)));
            }
        }
        if fds[0].revents != 0 {
            match read(fd, &mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    parser.feed(&buf[..n]);
                    esc_deadline = parser.holds_esc().then(|| Instant::now() + esc_wait);
                }
                Err(err) if matches!(err.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                Err(_) => return,
            }
        } else if esc_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            parser.finish_esc();
            esc_deadline = None;
        }
        while let Some(event) = parser.pop() {
            if !deliver(event) {
                return;
            }
        }
    }
}

fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    usize::try_from(n).map_err(|_| io::Error::last_os_error())
}

/// Terminal bytes on their way to events. As in crossterm's reader, every
/// byte may end an event, and an unfinished sequence waits for more bytes.
#[derive(Default)]
struct Parser {
    buffer: Vec<u8>,
    events: VecDeque<InternalEvent>,
}

impl Parser {
    /// Parses the next read. A bare ESC held from the read before joins it
    /// only to start a CSI (`[`) or SS3 (`O`) sequence: an ESC that came
    /// before anything else was the Esc key, so two quick Escs stay two and
    /// Esc and then a letter is not Alt and the letter.
    fn feed(&mut self, bytes: &[u8]) {
        if self.holds_esc() && !matches!(bytes.first(), Some(b'[' | b'O')) {
            self.finish_esc();
        }
        for &byte in bytes {
            self.buffer.push(byte);
            match parse_event(&self.buffer, true) {
                Ok(Some(event)) => {
                    self.events.push_back(event);
                    self.buffer.clear();
                }
                Ok(None) => {}
                Err(_) => self.buffer.clear(),
            }
        }
    }

    fn holds_esc(&self) -> bool {
        self.buffer == [0x1B]
    }

    /// Delivers a held bare ESC as the Esc key.
    fn finish_esc(&mut self) {
        self.buffer.clear();
        self.events.push_back(InternalEvent::Event(Event::Key(KeyCode::Esc.into())));
    }

    /// The next event; answers to terminal queries are dropped, as
    /// crossterm's `event::read` drops them.
    fn pop(&mut self) -> Option<Event> {
        while let Some(event) = self.events.pop_front() {
            if let InternalEvent::Event(event) = event {
                return Some(event);
            }
        }
        None
    }
}

#[cfg(test)]
#[path = "tests/input.rs"]
mod tests;
