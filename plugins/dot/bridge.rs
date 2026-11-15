//! The chat controller. Only this side writes the delivery journal; the host
//! keeps authentication private and sends back room-scoped data.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::host;
use super::state::State;
use super::{clip_chars, normalize_space, truthy, Error};

const RESPONSE_MAX: usize = 8 << 20;
/// The longest one request to the host may take.
const REQUEST_WINDOW: Duration = Duration::from_secs(120);

/// What the host's output gives a waiting request.
enum Reply {
    Line(Vec<u8>),
    /// The output ended or exceeded its limit.
    Closed,
    Interrupted,
}

/// Hears the host announce a live activity change.
pub type Events = Arc<dyn Fn() + Send + Sync>;

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Ends a waiting request at once for a reconnect or shutdown.
#[derive(Clone, Default)]
pub struct Interrupt(Arc<Interruption>);

#[derive(Default)]
struct Interruption {
    raised: AtomicBool,
    waiting: Mutex<Option<Sender<Reply>>>,
}

impl Interrupt {
    pub fn raise(&self) {
        self.0.raised.store(true, Ordering::SeqCst);
        if let Some(waiting) = guard(&self.0.waiting).as_ref() {
            let _ = waiting.send(Reply::Interrupted);
        }
    }

    pub fn clear(&self) {
        self.0.raised.store(false, Ordering::SeqCst);
    }

    pub fn raised(&self) -> bool {
        self.0.raised.load(Ordering::SeqCst)
    }

    fn attach(&self, replies: Option<Sender<Reply>>) {
        *guard(&self.0.waiting) = replies;
    }
}

/// Read the host's lines on a thread: an announcement goes to `events`,
/// everything else to the returned channel, which ends with `Closed`.
fn read_host<R: Read + Send + 'static>(output: R, events: Option<Events>) -> (Sender<Reply>, Receiver<Reply>) {
    let (replies, received) = mpsc::channel();
    let sender = replies.clone();
    let _ = std::thread::Builder::new().name("dot-host-output".into()).spawn(move || {
        let mut reader = BufReader::with_capacity(64 * 1024, output);
        loop {
            let mut line = Vec::new();
            match (&mut reader).take(RESPONSE_MAX as u64 + 1).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() > RESPONSE_MAX => break,
                Ok(_) => {}
            }
            let ended = line.last() == Some(&b'\n');
            if ended {
                line.pop();
            }
            if line.starts_with(b"{\"event\":") {
                let event: Option<Value> = serde_json::from_slice(&line).ok();
                if event.is_some_and(|e| e.get("event").and_then(Value::as_str) == Some("live")) {
                    if let Some(events) = &events {
                        events();
                    }
                }
            } else if sender.send(Reply::Line(line)).is_err() {
                return;
            }
            if !ended {
                break;
            }
        }
        let _ = sender.send(Reply::Closed);
    });
    (replies, received)
}

trait Input: Write + AsRawFd + Send {}
impl<T: Write + AsRawFd + Send> Input for T {}

/// Only the owned host stdin is changed; the child keeps its blocking read end.
fn nonblocking_input<T: AsRawFd>(input: T) -> io::Result<T> {
    // SAFETY: input owns a valid descriptor; fcntl changes only its status flags.
    let configured = unsafe {
        let flags = libc::fcntl(input.as_raw_fd(), libc::F_GETFL);
        flags >= 0 && libc::fcntl(input.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0
    };
    if configured {
        Ok(input)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Complete a frame on unbuffered, nonblocking host stdin. A stalled image
/// cannot hide shutdown/reconnect behind a blocking pipe write.
fn write_request(
    input: &mut dyn Input,
    mut bytes: &[u8],
    deadline: Instant,
    stop: &AtomicBool,
    interrupt: &AtomicBool,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if stop.load(Ordering::Relaxed) || interrupt.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match input.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let mut ready = libc::pollfd { fd: input.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                let wait = left.min(Duration::from_millis(20)).as_millis().max(1) as libc::c_int;
                // SAFETY: input owns this descriptor; one initialized pollfd
                // is borrowed only for the bounded wait.
                if unsafe { libc::poll(&mut ready, 1, wait) } < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.as_bytes()[0].is_ascii_alphanumeric()
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

pub fn host_command(host: &str) -> Result<Command, Error> {
    if !valid_host(host) {
        return Err(Error::Invalid("Configure a native ChatGPT host with setup --native-host HOST.".into()));
    }
    if host == "local" {
        if cfg!(test) {
            // The test binary must never start itself as a native host.
            return Err(Error::Invalid("Tests cannot start the local native host.".into()));
        }
        let mut command = Command::new(std::env::current_exe()?);
        command.args(["dot", "host"]);
        host::owned(&mut command);
        Ok(command)
    } else {
        let mut command = Command::new("ssh");
        // Noninteractive SSH does not load the login profile on every host.
        // Keep an explicit PATH installation, then try the fleet install path.
        // This remote program is fixed; the alias cannot add flags and all
        // URLs, messages and request IDs travel only over standard input.
        const REMOTE: &str = "if command -v cepheus >/dev/null 2>&1; then exec cepheus dot host; else exec \"$HOME/.local/bin/cepheus\" dot host; fi";
        command.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--", host, REMOTE]);
        host::owned(&mut command);
        Ok(command)
    }
}

pub struct Bridge {
    pub state: Arc<State>,
    host: String,
    process: Option<Child>,
    input: Option<Box<dyn Input>>,
    output: Option<Receiver<Reply>>,
    sequence: u64,
    pub ready: bool,
    images: bool,
    identity: Option<(String, String)>,
    pub retry_delay: Duration,
    stop: Arc<AtomicBool>,
    pub interrupt: Interrupt,
    /// Hears the host announce a live activity change while connected.
    pub events: Option<Events>,
}

/// A failure before the journal: the message never reached the host.
fn not_sent(error: Error) -> Error {
    reword(error, |text| format!("Not sent: {text}"))
}

/// A failure after the journal: the host may have posted the message.
fn unconfirmed(error: Error) -> Error {
    reword(error, |text| format!("Delivery unconfirmed: {text} Check ChatGPT; nothing is resent."))
}

fn reword(error: Error, words: impl Fn(&str) -> String) -> Error {
    match error {
        Error::Attention(text) => Error::Attention(words(&text)),
        Error::Invalid(text) => Error::Invalid(words(&text)),
        Error::Connection(text) => Error::Connection(words(&text)),
        Error::Io(error) => Error::Connection(words(&Error::Io(error).to_string())),
    }
}

impl Bridge {
    pub fn new(state: Arc<State>, host: String, stop: Arc<AtomicBool>) -> Result<Self, Error> {
        if !valid_host(&host) {
            return Err(Error::Invalid("Configure a native ChatGPT host with setup --native-host HOST.".into()));
        }
        Ok(Self {
            state,
            host,
            process: None,
            input: None,
            output: None,
            sequence: 0,
            ready: false,
            images: false,
            identity: None,
            retry_delay: Duration::from_secs(5),
            stop,
            interrupt: Interrupt::default(),
            events: None,
        })
    }

    /// Tests only: a bridge past its handshake with the host at `stream`.
    #[cfg(test)]
    pub fn over(state: Arc<State>, stream: std::os::unix::net::UnixStream, events: Option<Events>) -> Bridge {
        let mut bridge = Bridge::new(state, "local".into(), Arc::new(AtomicBool::new(false))).unwrap();
        bridge.events = events;
        bridge.input = Some(Box::new(stream.try_clone().unwrap()));
        bridge.attach(stream);
        bridge.ready = true;
        bridge.images = true;
        bridge.identity = Some(("fixture".into(), "room".into()));
        bridge
    }

    fn interrupted(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.interrupt.raised()
    }

    /// Read answers from the host's output from now on.
    fn attach<R: Read + Send + 'static>(&mut self, output: R) {
        let (replies, received) = read_host(output, self.events.clone());
        self.interrupt.attach(Some(replies));
        self.output = Some(received);
    }

    pub fn rpc(&mut self, action: &str, values: Value) -> Result<Value, Error> {
        if self.interrupted() {
            return Err(Error::Connection("Native request interrupted".into()));
        }
        let input = self.input.as_mut().ok_or_else(|| Error::Connection("Native host disconnected".into()))?;
        self.sequence += 1;
        let sequence = self.sequence;
        let mut request = values.as_object().cloned().unwrap_or_default();
        request.insert("id".into(), sequence.into());
        request.insert("action".into(), action.into());
        let deadline = Instant::now() + REQUEST_WINDOW;
        let mut bytes = Value::Object(request).to_string().into_bytes();
        bytes.push(b'\n');
        if let Err(error) = write_request(&mut **input, &bytes, deadline, &self.stop, &self.interrupt.0.raised) {
            self.ready = false;
            return Err(Error::Connection(
                if error.kind() == io::ErrorKind::Interrupted {
                    "Native request interrupted"
                } else {
                    "Native host unavailable"
                }
                .into(),
            ));
        }
        let raw = loop {
            let output = self.output.as_ref().ok_or_else(|| Error::Connection("Native host disconnected".into()))?;
            // One wait: the answer, the end of the output, an interrupt or the deadline.
            match output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Reply::Line(data)) => break data,
                // An interrupt already cleared for a new connection is stale.
                Ok(Reply::Interrupted) if !self.interrupted() => {}
                Ok(Reply::Interrupted) => {
                    self.ready = false;
                    return Err(Error::Connection("Native request interrupted".into()));
                }
                Ok(Reply::Closed) | Err(_) => {
                    self.ready = false;
                    return Err(Error::Connection("Native host response unavailable".into()));
                }
            }
        };
        let response: Value =
            serde_json::from_slice(&raw).map_err(|_| Error::Connection("Native host response is invalid".into()))?;
        if response.get("id") != Some(&json!(sequence)) {
            self.ready = false;
            return Err(Error::Connection("Native host response identity mismatch".into()));
        }
        if response.get("ok") != Some(&Value::Bool(true)) {
            self.ready = false;
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .filter(|text| text.chars().count() <= 512)
                .unwrap_or("Native connection unavailable.");
            if response.get("retry") == Some(&Value::Bool(true)) {
                self.retry_delay =
                    Duration::from_secs(response.get("retry_after").and_then(Value::as_u64).unwrap_or(5).clamp(5, 300));
                return Err(Error::Connection(error.into()));
            }
            return Err(Error::Attention(error.into()));
        }
        response.get("result").cloned().ok_or_else(|| Error::Connection("Native host response is invalid".into()))
    }

    pub fn open(&mut self) -> Result<Value, Error> {
        self.close();
        let url = self
            .state
            .settings()?
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Attention("Configure the existing Dot URL with setup first.".into()))?;
        super::state::dot_url(&url)?;
        let mut process =
            host_command(&self.host)?.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let input = process.stdin.take();
        if let Some(output) = process.stdout.take() {
            self.attach(output);
        }
        self.process = Some(process);
        self.input = match input.map(nonblocking_input).transpose() {
            Ok(input) => input.map(|input| Box::new(input) as Box<dyn Input>),
            Err(error) => {
                self.close();
                return Err(error.into());
            }
        };
        self.handshake(&url)
    }

    fn handshake(&mut self, url: &str) -> Result<Value, Error> {
        let hello = match self.rpc("hello", json!({})) {
            Ok(hello) => hello,
            Err(error) => {
                let status = self
                    .process
                    .as_mut()
                    .and_then(|child| child.try_wait().ok().flatten())
                    .and_then(|status| status.code());
                if matches!(status, Some(2 | 127)) {
                    self.close();
                    return Err(Error::Attention(
                        "The native chat host needs a Cepheus build with the Rust Dot bridge. No message was sent."
                            .into(),
                    ));
                }
                return Err(error);
            }
        };
        if hello.get("protocol").and_then(Value::as_str) != Some(host::PROTOCOL)
            || hello.get("version").and_then(Value::as_str) != Some(host::VERSION)
        {
            self.close();
            return Err(Error::Attention(
                "The native chat host needs the matching Cepheus build. No message was sent.".into(),
            ));
        }
        self.images = hello.get("capabilities").and_then(Value::as_array).is_some_and(|a| a.contains(&json!("images")));
        let snapshot = self.rpc("open", json!({"url": url}))?;
        let dot = snapshot.get("dot_id").and_then(Value::as_str).filter(|id| !id.is_empty());
        let room = snapshot.get("room_id").and_then(Value::as_str).filter(|id| !id.is_empty());
        let (Some(dot), Some(room)) = (dot, room) else {
            return Err(Error::Attention("Native response does not match the selected Dot room.".into()));
        };
        self.identity = Some((dot.into(), room.into()));
        let selection = json!({"url": url, "dot_id": dot, "room_id": room});
        if let Some(pinned) = self.state.read("selection")?.filter(truthy) {
            if pinned != selection {
                self.close();
                return Err(Error::Attention(
                    "Dot room changed. Check delivery and explicitly set up the intended Dot URL.".into(),
                ));
            }
        } else {
            self.state.write("selection", &selection)?;
        }
        let snapshot = self.publish(snapshot)?;
        self.ready = true;
        Ok(snapshot)
    }

    pub fn publish(&mut self, mut snapshot: Value) -> Result<Value, Error> {
        let identity = self.identity.as_ref().ok_or_else(|| Error::Attention("Dot is not connected.".into()))?;
        if snapshot.get("dot_id").and_then(Value::as_str) != Some(identity.0.as_str())
            || snapshot.get("room_id").and_then(Value::as_str) != Some(identity.1.as_str())
        {
            self.ready = false;
            return Err(Error::Attention("Native response does not match the selected Dot room.".into()));
        }
        let messages = snapshot
            .get("messages")
            .and_then(Value::as_array)
            .filter(|items| items.len() <= 500)
            .ok_or_else(|| Error::Attention("Native message response is invalid.".into()))?;
        let mut cleaned = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut budget = 4_000_000usize;
        for message in messages {
            let role = message.get("role").and_then(Value::as_str);
            let id = message.get("id").and_then(Value::as_str);
            let text = message.get("text").and_then(Value::as_str);
            let (Some("user" | "assistant"), Some(id), Some(text)) = (role, id, text) else {
                return Err(Error::Attention("Native message identity is invalid.".into()));
            };
            if !seen.insert(id) {
                return Err(Error::Attention("Native message identity is invalid.".into()));
            }
            let text = clip_chars(text, 24000);
            let Some(left) = budget.checked_sub(text.len()) else { break };
            budget = left;
            let mut message = message.clone();
            message["text"] = text.into();
            cleaned.push(message);
        }
        if let Some(pending) = self.state.read("pending")?.filter(truthy) {
            let before = pending.get("before").and_then(Value::as_array);
            let matched = pending.get("room_id").and_then(Value::as_str) == Some(identity.1.as_str())
                && messages.iter().any(|message| {
                    message.get("role").and_then(Value::as_str) == Some("user")
                        && !before.is_some_and(|ids| ids.contains(&message["id"]))
                        && pending.get("ack").is_none_or(|ack| !truthy(ack) || Some(ack) == message.get("id"))
                        && pending.get("files").and_then(Value::as_array).is_none_or(|files| {
                            let echoed: Vec<Value> = message
                                .get("attachments")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .map(|a| a["id"].clone())
                                .collect();
                            files == &echoed
                        })
                        && message
                            .get("text")
                            .and_then(Value::as_str)
                            .zip(pending.get("text").and_then(Value::as_str))
                            .is_some_and(|(a, b)| normalize_space(a) == normalize_space(b))
                });
            if matched {
                self.state.write("pending", &Value::Null)?;
            }
        }
        // The server returns a sliding recent window. Update by stable message
        // identity rather than replacing scrollback or duplicating streaming edits.
        let mut feed = self.state.read("transcript")?.and_then(|v| v.as_array().cloned()).unwrap_or_default();
        let legacy = cleaned.is_empty() && feed.iter().all(|item| item.get("id").is_none());
        feed.retain(|m| {
            (legacy || m.get("id").and_then(Value::as_str).is_some())
                && !snapshot.get("deleted").and_then(Value::as_array).is_some_and(|ids| ids.contains(&m["id"]))
        });
        for message in &cleaned {
            let mut text = message["text"].as_str().unwrap_or("").to_string();
            for file in message.get("attachments").and_then(Value::as_array).into_iter().flatten().take(10) {
                let name = clip_chars(file.get("name").and_then(Value::as_str).unwrap_or("image"), 100);
                text.push_str(&format!("\n[Image: {name}]"));
            }
            let item =
                json!({"id":message["id"], "k":message["role"], "text":text, "at":message_time(message.get("at"))});
            if let Some(old) = feed.iter_mut().find(|m| m["id"] == item["id"]) {
                *old = item;
            } else {
                feed.push(item);
            }
        }
        // Bound encoded JSON too: escaped text must remain readable under the
        // private-state file limit on the next publication.
        let mut sizes: std::collections::VecDeque<usize> = feed.iter().map(|item| item.to_string().len() + 1).collect();
        let mut size: usize = sizes.iter().sum::<usize>() + 1;
        while feed.len() > 500 || size > 4_000_000 {
            feed.remove(0);
            size = size.saturating_sub(sizes.pop_front().unwrap_or(0));
        }
        self.state.publish(Some(feed))?;
        snapshot["messages"] = Value::Array(cleaned);
        snapshot["composer"] = true.into();
        Ok(snapshot)
    }

    pub fn snapshot(&mut self) -> Result<Value, Error> {
        let snapshot = self.rpc("snapshot", json!({}))?;
        self.publish(snapshot)
    }

    pub fn send(&mut self, text: &str) -> Result<(), Error> {
        self.send_message(
            &super::api::Message { text: text.into(), images: Vec::new() },
            Instant::now() + Duration::from_secs(120),
        )
        .map(|_| ())
    }

    pub fn send_message(&mut self, message: &super::api::Message, deadline: Instant) -> Result<Value, Error> {
        let timely = || {
            if Instant::now() < deadline {
                Ok(())
            } else {
                Err(Error::Attention(
                    "Dot request expired before message submission; the draft and images are kept.".into(),
                ))
            }
        };
        timely()?;
        message.validate().map_err(Error::Attention)?;
        let text = &message.text;
        if self.state.read("pending")?.as_ref().is_some_and(truthy) {
            return Err(Error::Attention("Delivery is uncertain. Check ChatGPT, then /resolve.".into()));
        }
        let snapshot = self.snapshot().map_err(not_sent)?;
        if !self.ready {
            return Err(Error::Attention("Native Dot is disconnected. Use /reconnect.".into()));
        }
        if !message.images.is_empty() && !self.images {
            return Err(Error::Attention(
                "The native ChatGPT host needs the image-capable Dot bridge; the draft and images are kept.".into(),
            ));
        }
        let mut files = Vec::new();
        for image in &message.images {
            timely()?;
            let uploaded = self.rpc("upload", json!({"image":image})).map_err(not_sent)?;
            let id = uploaded["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| Error::Attention("Image upload was not acknowledged; no message was sent.".into()))?;
            files.push(id.to_string());
        }
        timely()?;
        let mut pending = json!({"text": text, "before": snapshot["messages"].as_array().unwrap().iter().map(|m| m["id"].clone()).collect::<Vec<_>>(),
            "url": self.state.settings()?.get("url"), "room_id": self.identity.as_ref().unwrap().1,
            "files":files, "request_id": crate::util::uuid_v4()?, "at": crate::util::now_ms() as f64 / 1000.0});
        // Durable before POST. A lost reply never triggers a resend.
        self.state.write("pending", &pending).map_err(not_sent)?;
        let answer = self
            .rpc("send", json!({"text": text, "files":pending["files"], "request_id": pending["request_id"]}))
            .map_err(unconfirmed)?;
        let ack = answer
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .filter(|id| !pending["before"].as_array().unwrap().contains(&json!(id)))
            .ok_or_else(|| Error::Attention("Delivery acknowledgement is uncertain. Check ChatGPT.".into()))?;
        // Submitted. Nothing after this is the message's failure: without the
        // recorded acknowledgement its echo still clears the journal, and a
        // failed refresh leaves `ready` false so the chat reconnects.
        pending["ack"] = ack.into();
        let _ = self.state.write("pending", &pending);
        let _ = self.snapshot();
        Ok(json!({"id":ack}))
    }

    pub fn close(&mut self) {
        self.ready = false;
        self.interrupt.attach(None);
        self.input.take();
        if let Some(mut child) = self.process.take() {
            let deadline = Instant::now() + Duration::from_secs(15);
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        self.output.take();
    }
}

#[cfg(test)]
#[path = "../../src/tests/dot_native.rs"]
mod tests;

impl Drop for Bridge {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn message_time(value: Option<&Value>) -> Option<u64> {
    let parsed = time::OffsetDateTime::parse(value?.as_str()?, &time::format_description::well_known::Rfc3339).ok()?;
    u64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
}

/// A failure the live activity reports. Working, waiting and paused are
/// not failures, so the chat shows nothing for them.
pub fn activity_failure(snapshot: &Value) -> Option<String> {
    (snapshot.get("activity_label").and_then(Value::as_str) == Some("Turn failed"))
        .then(|| "Turn failed in ChatGPT.".into())
}
