//! Management commands and the terminal event loop. No service is started here.
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::bridge::{self, Bridge};
use super::registration;
use super::state::{absolute, dot_url, private_dir, write_json, State};
use super::terminal::{self, Input, Terminal};
use super::Error;

const USAGE: &str = "usage: cepheus dot setup|chat|probe|login [--config-dir DIR] [--state-dir DIR] [--url URL] [--native-host HOST]\nThe native chat host needs this Cepheus build and a signed-in ChatGPT Linux app.";

#[derive(Default)]
pub struct Args {
    pub operation: String,
    pub config: PathBuf,
    pub state: Option<PathBuf>,
    pub url: Option<String>,
    pub host: Option<String>,
    source: Option<PathBuf>,
    dry_run: bool,
}

pub fn parse(words: &[String]) -> Result<Args, Error> {
    let mut args = Args { config: crate::paths::config_dir(), ..Args::default() };
    let Some(operation) = words.first() else { return Err(Error::Invalid(USAGE.into())) };
    if !["setup", "chat", "probe", "login", "repo-sources"].contains(&operation.as_str()) {
        return Err(Error::Invalid(USAGE.into()));
    }
    args.operation = operation.clone();
    let mut words = words[1..].iter();
    while let Some(word) = words.next() {
        if word == "--dry-run" {
            args.dry_run = true;
            continue;
        }
        let value = words.next().ok_or_else(|| Error::Invalid(USAGE.into()))?;
        match word.as_str() {
            "--config-dir" => args.config = absolute(Path::new(value)),
            "--state-dir" => args.state = Some(absolute(Path::new(value))),
            "--url" => args.url = Some(dot_url(value)?),
            "--source-root" => args.source = Some(absolute(Path::new(value))),
            "--native-host" if bridge::valid_host(value) => args.host = Some(value.clone()),
            _ => return Err(Error::Invalid(USAGE.into())),
        }
    }
    Ok(args)
}

fn configure(args: &Args, state: &State) -> Result<(), Error> {
    let config = private_dir(&args.config)?;
    let providers = private_dir(&config.join("providers"))?;
    let mut settings = state.settings()?;
    if let Some(url) = &args.url {
        if state.read("pending")?.as_ref().is_some_and(super::truthy) {
            return Err(Error::Invalid("Check uncertain delivery and /resolve before changing Dot setup.".into()));
        }
        settings.insert("url".into(), url.clone().into());
    }
    // The removed browser route's selector; the native route is the only one.
    settings.remove("transport");
    if let Some(host) = &args.host {
        settings.insert("native_host".into(), host.clone().into());
    }
    let provider = registration::provider(&std::env::current_exe()?, &state.directory);
    state.write("settings", &Value::Object(settings))?;
    write_json(&providers.join("dot.json"), &provider)?;
    if args.url.is_some() {
        state.write("selection", &Value::Null)?;
    }
    println!("Dot provider configured. Restart the deck service once, then use :new dot or :dot.");
    Ok(())
}

pub(super) enum Work {
    Send(String),
    Api(super::api::Call),
    Reconnect,
    Resolve,
    /// The host announced a live activity change.
    Live,
    Stop,
}

pub(super) struct Update {
    snapshot: Option<Value>,
    /// The failure the chat shows until the next update, if any.
    failure: Option<String>,
    /// The owner quit through the private API.
    quit: bool,
}

/// The first pause after a failed connection, doubled up to `RETRY_MAX`.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(300);
/// The refresh while Dot works, and the first wait once it is idle.
const REFRESH: Duration = Duration::from_secs(2);
/// The longest idle wait when live activity announces each change, and
/// when it cannot.
const IDLE_ANNOUNCED: Duration = Duration::from_secs(60);
const IDLE_UNANNOUNCED: Duration = Duration::from_secs(8);
const SEND_WINDOW: Duration = Duration::from_secs(120);

enum Link {
    /// Connect at this moment.
    Down(Instant),
    /// Refresh at this moment.
    Open(Instant),
}

/// What the chat shows for a failure: its own words, or that the
/// connection is down and the bridge retries without resending.
fn failure(error: &Error) -> String {
    match error {
        Error::Attention(text) | Error::Invalid(text) => text.clone(),
        _ => "Cannot connect to Dot; retrying. Nothing is resent.".into(),
    }
}

struct Worker<'a> {
    bridge: Bridge,
    output: &'a dyn Fn(Update),
    link: Link,
    retry: Duration,
    idle: Duration,
    /// A failure after connecting was already checked by connecting again.
    rechecked: bool,
    /// What the chat shows now.
    messages: Value,
    failure: Option<String>,
    /// Owner requests (messages, `/reconnect`, `/resolve`, API calls) queued
    /// for the worker and not yet answered; a newer build takes over only
    /// when there are none.
    queued: Arc<AtomicUsize>,
}

impl Worker<'_> {
    fn show(&mut self, snapshot: Option<Value>, failure: Option<String>) {
        if let Some(snapshot) = &snapshot {
            self.messages = snapshot["messages"].clone();
        }
        self.failure = failure.clone();
        (self.output)(Update { snapshot, failure, quit: false });
    }

    /// After a request: its failure, if any, until the conversation's own
    /// refresh soon after.
    fn note(&mut self, failure: Option<String>) {
        if let Link::Open(_) = self.link {
            self.idle = REFRESH;
            self.link = Link::Open(Instant::now() + REFRESH);
        }
        self.show(None, failure);
    }

    /// Open the room and fetch the card exactly as before.
    fn connect(&mut self) -> Result<Value, Error> {
        let _ = self.bridge.state.activity(None);
        let snapshot = self.bridge.open()?;
        let have = super::profile::have(&self.bridge.state, true);
        match self.bridge.rpc("profile", json!({"have": have})) {
            Ok(profile) => {
                let _ = super::profile::record(&self.bridge.state, &profile);
            }
            Err(error) if !self.bridge.ready => return Err(error),
            Err(_) => {}
        }
        self.retry = RETRY_FIRST;
        self.idle = REFRESH;
        Ok(snapshot)
    }

    /// Show a snapshot when it changed anything, and plan the next refresh:
    /// soon while Dot works, later the longer nothing changes.
    fn opened(&mut self, snapshot: Value) {
        let _ = self.bridge.state.activity(Some(&snapshot));
        let failure = if self.bridge.state.read("pending").ok().flatten().as_ref().is_some_and(super::truthy) {
            Some("Delivery uncertain; check ChatGPT, then /resolve.".into())
        } else {
            bridge::activity_failure(&snapshot)
        };
        let changed = snapshot["messages"] != self.messages || failure != self.failure;
        let announced = snapshot.get("activity_known") == Some(&Value::Bool(true));
        self.idle = if changed {
            REFRESH
        } else {
            (self.idle * 2).min(if announced { IDLE_ANNOUNCED } else { IDLE_UNANNOUNCED })
        };
        let working = snapshot.get("working") == Some(&Value::Bool(true));
        self.link = Link::Open(Instant::now() + if working { REFRESH } else { self.idle });
        if changed {
            self.show(Some(snapshot), failure);
        }
    }

    /// After a failed connection or request. The host has closed the room:
    /// a problem found after connecting is checked again by connecting at
    /// once; anything else connects again after a growing pause. Nothing
    /// waits for the owner. A signed-out app, a login another host renewed or
    /// an older host build is put right outside the chat, so each attempt
    /// checks again, with every room and identity check, and resends nothing.
    /// Until 2026-11-12 a problem found while connecting stopped the chat
    /// until a send or `/reconnect`: "Native ChatGPT login is unavailable."
    /// stayed after the renewed login had reached every host.
    fn failed(&mut self, error: Error, connecting: bool) {
        let _ = self.bridge.state.activity(None);
        self.bridge.close();
        let problem = matches!(error, Error::Attention(_) | Error::Invalid(_));
        self.link = if problem && !connecting && !self.rechecked {
            self.rechecked = true;
            Link::Down(Instant::now())
        } else {
            let pause = self.retry.max(self.bridge.retry_delay);
            self.retry = (pause * 2).min(RETRY_MAX);
            Link::Down(Instant::now() + pause)
        };
        self.bridge.retry_delay = RETRY_FIRST;
        let shown = failure(&error);
        self.show(None, Some(if problem { format!("{shown} Retrying automatically.") } else { shown }));
    }

    /// The timer: connect or refresh.
    fn due(&mut self) {
        match self.link {
            Link::Down(_) => match self.connect() {
                Ok(snapshot) => self.opened(snapshot),
                Err(error) => self.failed(error, true),
            },
            Link::Open(_) => match self.bridge.snapshot() {
                Ok(snapshot) => {
                    self.rechecked = false;
                    self.opened(snapshot);
                }
                Err(error) => self.failed(error, false),
            },
        }
    }

    /// Deliver one message, connecting first when needed. The answer says
    /// whether it was submitted, not sent, or is unconfirmed.
    fn deliver(&mut self, message: &super::api::Message, deadline: Instant) -> Result<Value, String> {
        if !self.bridge.ready {
            // A send is the owner acting: connect now instead of waiting.
            match self.connect() {
                Ok(snapshot) => self.opened(snapshot),
                Err(error) => {
                    let detail = format!("Not sent: {}", error.detail());
                    self.failed(error, true);
                    return Err(detail);
                }
            }
        }
        match self.bridge.send_message(message, deadline) {
            Ok(ack) => {
                if self.bridge.ready {
                    self.note(None);
                } else {
                    self.failed(Error::Connection("Native host response unavailable".into()), false);
                }
                Ok(ack)
            }
            Err(error) => {
                let detail = error.detail();
                if self.bridge.ready {
                    self.note(Some(failure(&error)));
                } else {
                    self.failed(error, false);
                }
                Err(detail)
            }
        }
    }

    fn resolve(&mut self) -> Result<(), Error> {
        let result = self.bridge.state.write("pending", &Value::Null);
        match &result {
            Ok(()) => self.note(None),
            Err(error) => self.note(Some(failure(error))),
        }
        result
    }

    fn reconnect(&mut self) {
        self.bridge.interrupt.clear();
        self.bridge.close();
        let _ = self.bridge.state.activity(None);
        self.retry = RETRY_FIRST;
        self.link = Link::Down(Instant::now());
        self.show(None, None);
    }

    /// One private API call; false after `/quit`.
    fn call(&mut self, call: super::api::Call) -> bool {
        use super::api::Control;
        let control = call.message.control();
        if Instant::now() >= call.deadline {
            let text = "Dot request expired before submission; the draft and images are kept.";
            let _ = call.reply.send(Err(text.into()));
            self.note(Some(text.into()));
            return true;
        }
        let result = match control {
            Some(Control::Resolve) => self.resolve().map(|()| json!({"control":control})).map_err(|e| e.detail()),
            Some(Control::Reconnect) => {
                self.reconnect();
                Ok(json!({"control":control}))
            }
            Some(Control::Quit) => Ok(json!({"control":control})),
            None => self.deliver(&call.message, call.deadline),
        };
        let _ = call.reply.send(result);
        if control == Some(Control::Quit) {
            (self.output)(Update { snapshot: None, failure: None, quit: true });
            return false;
        }
        true
    }
}

impl<'a> Worker<'a> {
    fn new(bridge: Bridge, output: &'a dyn Fn(Update)) -> Self {
        let link = if bridge.ready { Link::Open(Instant::now()) } else { Link::Down(Instant::now()) };
        Worker {
            bridge,
            output,
            link,
            retry: RETRY_FIRST,
            idle: REFRESH,
            rechecked: false,
            messages: Value::Null,
            failure: None,
            queued: Arc::default(),
        }
    }

    /// A queued request was answered.
    fn answered(&self) {
        let _ = self.queued.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
    }

    /// Until `/quit`, `Stop` or the end of the input.
    fn run(mut self, input: mpsc::Receiver<Work>, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::Relaxed) {
            let (Link::Down(at) | Link::Open(at)) = self.link;
            let work = input.recv_timeout(at.saturating_duration_since(Instant::now()));
            match work {
                Err(mpsc::RecvTimeoutError::Disconnected) | Ok(Work::Stop) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => self.due(),
                // Announcements in a row refresh once, after the queue is empty.
                Ok(Work::Live) => {
                    if let Link::Open(_) = self.link {
                        self.link = Link::Open(Instant::now());
                    }
                }
                Ok(Work::Reconnect) => {
                    self.reconnect();
                    self.answered();
                }
                Ok(Work::Resolve) => {
                    let _ = self.resolve();
                    self.answered();
                }
                Ok(Work::Send(text)) => {
                    let message = super::api::Message { text, images: Vec::new() };
                    let _ = self.deliver(&message, Instant::now() + SEND_WINDOW);
                    self.answered();
                }
                Ok(Work::Api(call)) => {
                    let more = self.call(call);
                    self.answered();
                    if !more {
                        break;
                    }
                }
            }
        }
        self.bridge.close();
        // Calls still queued were never started.
        for work in input.try_iter() {
            if let Work::Api(call) = work {
                let _ = call.reply.send(Err("Dot is unavailable; nothing was sent.".into()));
            }
        }
    }
}

/// The chat's connection, driven by requests, announcements and one timer.
/// `queued` counts the owner requests sent to `input` and not yet answered.
fn worker(
    bridge: Bridge,
    input: mpsc::Receiver<Work>,
    output: &dyn Fn(Update),
    stop: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
) {
    let mut worker = Worker::new(bridge, output);
    worker.queued = queued;
    worker.run(input, stop);
}

fn draw(terminal: &Terminal, state: &State, (columns, rows): (usize, usize)) -> Result<(), Error> {
    if state.set_notice(terminal.failure.clone()) {
        state.publish(None)?;
    }
    let mut out = io::stdout().lock();
    out.write_all(terminal.render(columns, rows).as_bytes())?;
    out.flush()?;
    Ok(())
}

/// How often, in milliseconds, an idle chat looks for a newer build of itself.
const UPDATE_CHECK_MS: libc::c_int = 2_000;

/// The newer build that replaced the program file `link` names, once it
/// shows it runs Dot. One that does not is declined until another replaces it.
fn newer_build(link: &Path, declined: &mut Option<crate::handover::BinaryId>) -> Option<PathBuf> {
    let (exe, id) = crate::handover::replacement_of(link)?;
    if *declined == Some(id) {
        return None;
    }
    let runs = Command::new(&exe)
        .args(["dot", "--version"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout.starts_with(b"cepheus dot "));
    if runs {
        return Some(exe);
    }
    *declined = Some(id);
    None
}

/// The chat until the owner quits. When a newer build is installed, it takes
/// over this process once the composer is empty and every message has its
/// answer: the same terminal, deck session and controller identity, with
/// nothing to restart.
fn chat(state: Arc<State>, host: String) -> Result<i32, Error> {
    let raw = terminal::Raw::enter()?;
    let mut terminal = Terminal::default();
    if let Some(Value::Array(messages)) = state.read("transcript")? {
        terminal.messages = messages
            .iter()
            .filter_map(|m| {
                let role = m.get("k")?.as_str()?;
                matches!(role, "user" | "assistant")
                    .then(|| (role.into(), m.get("text").and_then(Value::as_str).unwrap_or("").into()))
            })
            .collect();
    }
    let mut size = terminal::size();
    draw(&terminal, &state, size)?;
    let stop = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(super::Wake::new()?);
    let (commands, input) = mpsc::channel();
    let mut bridge = Bridge::new(Arc::clone(&state), host, Arc::clone(&stop))?;
    let announced = commands.clone();
    bridge.events = Some(Arc::new(move || {
        let _ = announced.send(Work::Live);
    }));
    let interrupt = bridge.interrupt.clone();
    let api_interrupt = interrupt.clone();
    let api_commands = commands.clone();
    let queued = Arc::new(AtomicUsize::new(0));
    let api_queued = Arc::clone(&queued);
    let api = super::api::Server::start(&state.directory, Arc::clone(&stop), move |call| {
        if matches!(call.message.control(), Some(super::api::Control::Reconnect | super::api::Control::Quit)) {
            api_interrupt.raise();
        }
        api_queued.fetch_add(1, Ordering::SeqCst);
        let sent = api_commands.send(Work::Api(call)).is_ok();
        if !sent {
            api_queued.fetch_sub(1, Ordering::SeqCst);
        }
        sent
    })
    .map_err(Error::Invalid)?;
    let (output, updates) = mpsc::channel();
    let worker_stop = Arc::clone(&stop);
    let worker_queued = Arc::clone(&queued);
    let woken = Arc::clone(&wake);
    let thread = std::thread::Builder::new().name("dot-worker".into()).spawn(move || {
        let output = move |update: Update| {
            if output.send(update).is_ok() {
                woken.notify();
            }
        };
        worker(bridge, input, &output, worker_stop, worker_queued)
    })?;
    let mut declined = None;
    let result = (|| -> Result<Option<PathBuf>, Error> {
        let mut decoder = terminal::Decoder::default();
        let mut data = [0u8; 8192];
        loop {
            let mut ready = [
                libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: wake.descriptor(), events: libc::POLLIN, revents: 0 },
            ];
            // SAFETY: two initialized pollfds borrowed for one wait, which ends
            // on owner input, a worker update or the next look for a newer build.
            if unsafe { libc::poll(ready.as_mut_ptr(), 2, UPDATE_CHECK_MS) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            // Repaint only what changed: the worker reports every poll.
            let mut changed = false;
            if ready[1].revents != 0 {
                wake.drain();
                while let Ok(update) = updates.try_recv() {
                    if update.quit {
                        return Ok(None);
                    }
                    if terminal.failure != update.failure {
                        terminal.failure = update.failure;
                        changed = true;
                    }
                    if let Some(snapshot) = update.snapshot {
                        let messages: Vec<(String, String)> = snapshot["messages"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|m| (m["role"].as_str().unwrap_or("").into(), m["text"].as_str().unwrap_or("").into()))
                            .collect();
                        if terminal.messages != messages {
                            terminal.messages = messages;
                            changed = true;
                        }
                    }
                }
            }
            let resized = terminal::size();
            if resized != size {
                size = resized;
                changed = true;
            }
            if ready[0].revents != 0 {
                let count = io::stdin().read(&mut data)?;
                if count == 0 {
                    return Ok(None);
                }
                for input in terminal.feed(&decoder.decode(&data[..count])) {
                    let work = match input {
                        Input::Quit => return Ok(None),
                        Input::Line(text) if text == "/quit" => return Ok(None),
                        Input::Line(text) if text == "/reconnect" => {
                            interrupt.raise();
                            Work::Reconnect
                        }
                        Input::Line(text) if text == "/resolve" => Work::Resolve,
                        Input::Line(text) => Work::Send(text),
                    };
                    queued.fetch_add(1, Ordering::SeqCst);
                    if commands.send(work).is_err() {
                        return Err(Error::Connection("Native worker stopped".into()));
                    }
                }
                changed = true;
            }
            if changed {
                draw(&terminal, &state, size)?;
            }
            if !terminal.composing() && queued.load(Ordering::SeqCst) == 0 {
                let link = std::fs::read_link("/proc/self/exe");
                if let Some(exe) = link.ok().and_then(|link| newer_build(&link, &mut declined)) {
                    return Ok(Some(exe));
                }
            }
        }
    })();
    // The worker answers or refuses every call it took before the API
    // closes, so no API caller waits on a stopped controller.
    stop.store(true, Ordering::Relaxed);
    interrupt.raise();
    let _ = commands.send(Work::Stop);
    let _ = thread.join();
    drop(api);
    let Some(exe) = result? else { return Ok(0) };
    // The connection, the private API and the controller lock closed with
    // this build; the terminal stays as it is for the newer one.
    let mut command = Command::new(&exe);
    command.args(std::env::args_os().skip(1));
    raw.pass_to(&mut command);
    let error = command.exec();
    Err(Error::Invalid(format!("Dot could not switch to the newer build at {}: {error}.", exe.display())))
}

fn execute(args: Args) -> Result<i32, Error> {
    if args.operation == "repo-sources" {
        let source =
            args.source.as_ref().ok_or_else(|| Error::Invalid("Repository source root is required.".into()))?;
        registration::source_mode(&args.config, source, args.dry_run)?;
        println!(
            "Repository plugin sources: {}. Reopen the deck; service plugins need a service update or handover.",
            source.display()
        );
        return Ok(0);
    }
    if args.source.is_some() || args.dry_run {
        return Err(Error::Invalid(USAGE.into()));
    }
    let registered = registration::read(&args.config, args.state.as_deref(), false)?;
    let directory = registered
        .as_ref()
        .map(|r| r.state.clone())
        .or_else(|| args.state.clone())
        .unwrap_or_else(|| crate::paths::state_dir().join("dot"));
    if args.operation == "setup" {
        if registered.is_some() {
            let held = registration::hold(&args.config, args.state.as_deref())?
                .ok_or_else(|| Error::Invalid("Dot registration changed; retry.".into()))?;
            let state = State::new(&held.record.state)?;
            if args.url.is_none() && args.host.is_none() {
                registration::relocate(&held, &std::env::current_exe()?)?;
                println!("Dot helper metadata is configured; private state and service were preserved.");
                return Ok(0);
            }
            configure(&args, &state)?;
            return Ok(0);
        }
        let state = State::new(&directory)?;
        state.acquire()?;
        configure(&args, &state)?;
        return Ok(0);
    }
    let state = Arc::new(State::new(&directory)?);
    state.acquire()?;
    let result = (|| {
        if let Some(url) = &args.url {
            if state.settings()?.get("url").and_then(Value::as_str) != Some(url) {
                return Err(Error::Invalid("Use setup to change the selected Dot URL.".into()));
            }
        }
        state.activity(None)?;
        let settings = state.settings()?;
        let host = args
            .host
            .as_deref()
            .or_else(|| settings.get("native_host").and_then(Value::as_str))
            .unwrap_or("local")
            .to_string();
        if args.operation == "login" {
            println!("Sign into the ChatGPT app on the configured native host, then run probe.");
            return Ok(0);
        }
        if args.operation == "chat" {
            return chat(Arc::clone(&state), host);
        }
        let mut bridge = Bridge::new(Arc::clone(&state), host, Arc::new(AtomicBool::new(false)))?;
        let snapshot = bridge.open()?;
        println!("Dot native connected; loaded messages: {}", snapshot["messages"].as_array().map_or(0, Vec::len));
        println!("Composer available: true");
        Ok(0)
    })();
    if state.holds_lock() {
        let _ = state.activity(None);
        if let Err(error) = &result {
            state.set_notice(Some(error.detail()));
            let _ = state.publish(None);
        }
        state.close();
    }
    result
}

pub fn run(words: &[String]) -> i32 {
    if words == ["host"] {
        return super::host::main();
    }
    if words == ["--version"] {
        println!("cepheus dot {}", super::host::VERSION);
        return 0;
    }
    if words.is_empty() || words == ["--help"] {
        println!("{USAGE}");
        return 0;
    }
    // SAFETY: restrict files this command creates; no preconditions.
    unsafe {
        libc::umask(0o077);
    }
    match parse(words).and_then(execute) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Dot needs attention: {}", error.detail());
            2
        }
    }
}

#[cfg(test)]
#[path = "../../src/tests/dot_cli.rs"]
mod tests;
