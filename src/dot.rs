//! `cepheus dot`: the optional bridge to an existing cloud ChatGPT Dot.
//!
//! `dot chat` is the session the service runs for the provider `dot`; it
//! keeps `transcript.json` and `activity.json` beside its private owner API.
//! `dot host` serves the signed-in native ChatGPT app,
//! over SSH or in this process for `local`: credentials, tokens and signed
//! URLs stay inside it and never reach arguments, snapshots or logs.
//! `dot setup`, `probe` and `login` manage the registration (`:dot`).

use std::fmt;
use std::io;

#[path = "../plugins/dot/api.rs"]
pub mod api;
pub use api::OwnerRequest;

#[path = "../plugins/dot/bridge.rs"]
mod bridge;
#[path = "../plugins/dot/cli.rs"]
mod cli;
#[path = "../plugins/dot/host.rs"]
mod host;
#[path = "../plugins/dot/live.rs"]
mod live;
#[path = "../plugins/dot/net.rs"]
mod net;
#[path = "../plugins/dot/profile.rs"]
mod profile;
#[path = "../plugins/dot/registration.rs"]
mod registration;
#[path = "../plugins/dot/state.rs"]
mod state;
#[path = "../plugins/dot/terminal.rs"]
mod terminal;

#[cfg(test)]
#[path = "tests/dot.rs"]
mod tests;

pub use cli::run;

/// How a failure reaches the owner.
#[derive(Debug)]
pub enum Error {
    /// Shown in the chat; nothing retries until the owner acts.
    Attention(String),
    /// Invalid input, configuration or private state.
    Invalid(String),
    /// The native connection failed; the chat reconnects without resending.
    Connection(String),
    /// A local file or process operation failed.
    Io(io::Error),
}

impl Error {
    /// What a failure shows. An I/O error shows only its kind: paths and
    /// payloads stay out of the chat feed.
    pub fn detail(&self) -> String {
        match self {
            Error::Attention(text) | Error::Invalid(text) | Error::Connection(text) => text.clone(),
            Error::Io(err) => format!("{} (check dependencies and private state permissions)", io_name(err)),
        }
    }
}

/// Wakes a thread waiting in `poll` from another thread, without a timer.
pub(crate) struct Wake {
    read: std::os::fd::OwnedFd,
    write: std::os::fd::OwnedFd,
}

impl Wake {
    pub fn new() -> io::Result<Wake> {
        use std::os::fd::FromRawFd;
        let mut descriptors = [0; 2];
        // SAFETY: pipe2 fills two new descriptors, which become owned here.
        if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both descriptors are new and have no other owner.
        Ok(unsafe {
            Wake {
                read: std::os::fd::OwnedFd::from_raw_fd(descriptors[0]),
                write: std::os::fd::OwnedFd::from_raw_fd(descriptors[1]),
            }
        })
    }

    /// Make the read end readable; a full pipe already is.
    pub fn notify(&self) {
        use std::os::fd::AsRawFd;
        // SAFETY: writes one byte from a live buffer to an owned descriptor.
        unsafe { libc::write(self.write.as_raw_fd(), b"!".as_ptr().cast(), 1) };
    }

    /// The descriptor to poll for input.
    pub fn descriptor(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.read.as_raw_fd()
    }

    /// Consume every pending wake.
    pub fn drain(&self) {
        use std::os::fd::AsRawFd;
        let mut buffer = [0u8; 64];
        // SAFETY: reads into a live buffer from an owned nonblocking descriptor.
        while unsafe { libc::read(self.read.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) } > 0 {}
    }
}

/// The exception names the bridge reported before it moved into the deck.
fn io_name(err: &io::Error) -> &'static str {
    match err.kind() {
        io::ErrorKind::NotFound => "FileNotFoundError",
        io::ErrorKind::PermissionDenied => "PermissionError",
        io::ErrorKind::AlreadyExists => "FileExistsError",
        io::ErrorKind::BrokenPipe => "BrokenPipeError",
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => "TimeoutError",
        io::ErrorKind::NotADirectory => "NotADirectoryError",
        io::ErrorKind::IsADirectory => "IsADirectoryError",
        _ => "OSError",
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Attention(text) | Error::Invalid(text) | Error::Connection(text) => f.write_str(text),
            Error::Io(err) => f.write_str(io_name(err)),
        }
    }
}

/// Python's truth value of a JSON value, which the delivery journal relies on:
/// `{}` and `false` are no pending delivery for a send, but a relocation
/// still refuses any non-null journal.
fn truthy(value: &serde_json::Value) -> bool {
    use serde_json::Value;
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `str.isspace` for one character.
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// Python's `" ".join(text.split())`: how a user echo is matched to the journal.
fn normalize_space(text: &str) -> String {
    text.split(is_space).filter(|word| !word.is_empty()).collect::<Vec<_>>().join(" ")
}

/// At most `limit` characters of `text`.
fn clip_chars(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}
