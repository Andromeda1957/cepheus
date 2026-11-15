//! `cepheus dot host`: the native host's side of the bridge. It runs on the
//! machine with the signed-in ChatGPT Linux app, over SSH or inside the chat
//! for `local`, and answers one JSON request per line on standard input.
//!
//! An owned headless app-server supplies the app's own login through its
//! account RPC; the room APIs are called like the app calls them. The token
//! stays in this process: it never crosses SSH or reaches an argument,
//! snapshot or log, and no login file, browser profile or keychain is read.
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::live::{self, Activity, Status};
use super::profile;
use super::state::{quote, split_url, unquote};

pub const VERSION: &str = "0.3.0";
/// v2: a timed-out request is temporary, and the host announces a live
/// activity change with an unsolicited `{"event":"live"}` line.
pub const PROTOCOL: &str = "cepheus-dot-rust-v2";
const API: &str = "https://chatgpt.com/backend-api";
const BODY_MAX: u64 = 8 << 20;
const REQUEST_MAX: usize = super::api::REQUEST_MAX;
const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(35);
const LABELS: [&str; 9] = [
    "Live activity unavailable",
    "Turn failed",
    "Ready",
    "Working",
    "Searching",
    "Creating",
    "Needs input",
    "Paused",
    "Thinking",
];

/// A bridge/helper belongs to its spawning controller. Even if that controller
/// is killed before Rust destructors run, its app-server must not be orphaned.
pub fn owned(command: &mut Command) -> &mut Command {
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: the child hook uses only async-signal-safe system calls, changes
    // only this child's death signal, and rejects the fork/parent-exit race.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            Ok(())
        });
    }
    command
}

/// How a request failed, as the chat should treat it.
#[derive(Debug)]
pub enum HostError {
    /// Shown to the owner in its own words; the chat connects again with a
    /// growing pause, checking everything again.
    Attention(String),
    /// Temporary: the chat reconnects after the given seconds.
    Transient(String, u64),
    /// Unexpected, reported by kind only; `true` when a reconnect may help.
    Other(&'static str, bool),
}

use HostError::{Attention, Other, Transient};

fn attention(text: &str) -> HostError {
    Attention(text.into())
}

fn os_error(err: io::Error) -> HostError {
    Other(
        match err.kind() {
            io::ErrorKind::NotFound => "FileNotFoundError",
            io::ErrorKind::PermissionDenied => "PermissionError",
            io::ErrorKind::BrokenPipe => "BrokenPipeError",
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => "TimeoutError",
            _ => "OSError",
        },
        true,
    )
}

/// How long one API request may take. A slow answer is not a stalled one:
/// only a connection, read or write that makes no progress fails early, and
/// the whole exchange stays inside the chat's 120-second request window.
#[derive(Clone, Copy)]
pub struct Limits {
    pub connect: Duration,
    /// One read or write without progress.
    pub stall: Duration,
    /// The wait for the response head after the request is sent.
    pub response: Duration,
    /// Everything, from the lookup to the end of the body.
    pub total: Duration,
}

impl Limits {
    pub fn production() -> Limits {
        Limits {
            connect: Duration::from_secs(20),
            stall: Duration::from_secs(30),
            response: Duration::from_secs(60),
            total: Duration::from_secs(100),
        }
    }
}

/// What the host reaches and how.
pub struct HostEnv {
    pub api: String,
    pub codex_home: PathBuf,
    pub account: Arc<dyn Fn() -> Result<AccountClient, HostError> + Send + Sync>,
    pub live: Option<live::Settings>,
    pub limits: Limits,
    /// Tests only: pictures from a plain local server.
    pub loopback: bool,
}

impl HostEnv {
    pub fn production() -> HostEnv {
        HostEnv {
            api: API.into(),
            codex_home: crate::paths::codex_dir(),
            account: Arc::new(AccountClient::spawn),
            live: Some(live::Settings::production(API)),
            limits: Limits::production(),
            loopback: false,
        }
    }
}

/// One line read from a child or the bridge.
pub enum Line {
    Data(Vec<u8>),
    TooLong,
}

/// Read lines of at most `limit` bytes on a thread; the channel closes at the end.
pub fn lines<R: Read + Send + 'static>(reader: R, limit: usize) -> Receiver<Line> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("dot-lines".into()).spawn(move || {
        let mut reader = BufReader::with_capacity(64 * 1024, reader);
        loop {
            let mut line = Vec::new();
            match (&mut reader).take(limit as u64 + 1).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() > limit => {
                    let _ = tx.send(Line::TooLong);
                    break;
                }
                Ok(_) => {
                    let ended = line.last() == Some(&b'\n');
                    if ended {
                        line.pop();
                    }
                    if tx.send(Line::Data(line)).is_err() || !ended {
                        break;
                    }
                }
            }
        }
    });
    rx
}

/// Wait for `child` to exit, then terminate it, then kill it.
pub fn reap(child: &mut Child, grace: Duration, term: Duration) {
    let wait = |child: &mut Child, limit: Duration| {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            if child.try_wait().ok().flatten().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    };
    if wait(child, grace) {
        return;
    }
    // SAFETY: signals the child this caller owns and has not reaped.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    if !wait(child, term) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn identifier(value: Option<&Value>) -> Result<String, HostError> {
    match value.and_then(Value::as_str) {
        Some(text)
            if !text.is_empty()
                && text.chars().count() <= 256
                && !text.chars().any(|c| (c as u32) < 33 || c == '\x7f') =>
        {
            Ok(text.to_string())
        }
        _ => Err(attention("Native Dot identity is unavailable.")),
    }
}

/// The Dot's ID from its `https://chatgpt.com/dots/ID` URL.
pub fn target_id(url: Option<&Value>) -> Result<String, HostError> {
    let Some(text) = url.and_then(Value::as_str) else { return Err(Other("TypeError", false)) };
    let parts = split_url(text);
    let words: Vec<&str> = parts.path.trim_matches('/').split('/').collect();
    if parts.scheme != "https"
        || parts.netloc != "chatgpt.com"
        || !parts.query.is_empty()
        || !parts.fragment.is_empty()
        || words.len() != 2
        || words[0] != "dots"
    {
        return Err(attention("Configure the existing Dot URL."));
    }
    let value = unquote(words[1]);
    if value.contains(['/', '\\']) || value.chars().any(|c| (c as u32) < 33) {
        return Err(attention("Invalid Dot URL."));
    }
    identifier(Some(&Value::String(value)))
}

fn app_version(directory: &Path) -> Result<String, HostError> {
    let unknown = || attention("Cannot identify the native app version.");
    let mut file = File::open(directory.join("app.asar")).map_err(os_error)?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header).map_err(|_| unknown())?;
    let word = |at: usize| u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]) as u64;
    let (header_size, json_size) = (word(4), word(12));
    if json_size == 0 || json_size > BODY_MAX {
        return Err(unknown());
    }
    let mut index = vec![0u8; json_size as usize];
    file.read_exact(&mut index).map_err(os_error)?;
    let index: Value = serde_json::from_slice(&index).map_err(|_| Other("JSONDecodeError", false))?;
    let entry = index.get("files").and_then(|f| f.get("package.json")).ok_or(Other("KeyError", false))?;
    let number = |value: Option<&Value>| match value {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    };
    let size = number(entry.get("size")).ok_or(Other("ValueError", false))?;
    if size == 0 || size >= 65536 {
        return Err(unknown());
    }
    let offset = number(entry.get("offset")).ok_or(Other("ValueError", false))?;
    file.seek(SeekFrom::Start(8 + header_size + offset)).map_err(os_error)?;
    let mut package = vec![0u8; size as usize];
    file.read_exact(&mut package).map_err(os_error)?;
    let package: Value = serde_json::from_slice(&package).map_err(|_| Other("JSONDecodeError", false))?;
    match package.get("version").and_then(Value::as_str) {
        Some(version) if !version.is_empty() && version.chars().count() <= 128 => Ok(version.to_string()),
        _ => Err(unknown()),
    }
}

/// The app's bundled app-server and the app version, read from its package
/// metadata without unpacking the app.
pub fn installed_app() -> Result<(PathBuf, String), HostError> {
    for directory in ["/usr/lib/chatgpt/resources", "/opt/ChatGPT/resources", "/opt/chatgpt/resources"] {
        let directory = Path::new(directory);
        let executable = directory.join("codex");
        if executable.is_file() {
            return Ok((executable, app_version(directory)?));
        }
    }
    Err(attention("The native ChatGPT Linux app is not installed on this host."))
}

/// The app-server's account RPC.
pub struct AccountClient {
    pub version: String,
    writer: Option<Box<dyn Write + Send>>,
    lines: Receiver<Line>,
    child: Option<Child>,
    sequence: u64,
}

impl AccountClient {
    pub fn spawn() -> Result<AccountClient, HostError> {
        let (executable, version) = installed_app()?;
        let mut command = Command::new(executable);
        command
            .args(["-c", "forced_login_method=\"chatgpt\"", "app-server"])
            .env("RUST_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::client_temp::contain(&mut command, "dot").map_err(|_| Other("OSError", true))?;
        let mut child = owned(&mut command).spawn().map_err(os_error)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            reap(&mut child, Duration::ZERO, Duration::from_secs(5));
            return Err(Other("OSError", true));
        };
        AccountClient::over(Box::new(stdout), Box::new(stdin), version, Some(child))
    }

    /// Speak to an app-server over these pipes and initialize it.
    pub fn over(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        version: String,
        child: Option<Child>,
    ) -> Result<AccountClient, HostError> {
        let mut client = AccountClient {
            version,
            writer: Some(writer),
            lines: lines(reader, BODY_MAX as usize),
            child,
            sequence: 0,
        };
        let params = json!({
            "clientInfo": {"name": "cepheus_dot_bridge", "version": VERSION},
            "capabilities": {"experimentalApi": true},
        });
        let started = client.call("initialize", params).and_then(|_| client.write(b"{\"method\":\"initialized\"}\n"));
        if let Err(err) = started {
            client.close();
            return Err(err);
        }
        Ok(client)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), HostError> {
        let writer = self.writer.as_mut().ok_or(Other("BrokenPipeError", true))?;
        writer.write_all(data).and_then(|_| writer.flush()).map_err(os_error)
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, HostError> {
        self.sequence += 1;
        let id = self.sequence;
        let mut line = json!({"id": id, "method": method, "params": params}).to_string();
        line.push('\n');
        self.write(line.as_bytes())?;
        let deadline = Instant::now() + ACCOUNT_TIMEOUT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.lines.recv_timeout(left) {
                Ok(Line::Data(line)) => {
                    let response: Value = serde_json::from_slice(&line).map_err(|_| Other("JSONDecodeError", false))?;
                    if response.get("id") != Some(&json!(id)) {
                        continue;
                    }
                    if response.get("error").is_some() {
                        return Err(attention("Native account access failed. Check the ChatGPT app login."));
                    }
                    return Ok(response.get("result").cloned().unwrap_or(Value::Null));
                }
                Ok(Line::TooLong) => return Err(attention("Native account response exceeded its limit.")),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        Err(Transient("Native account connection timed out.".into(), 5))
    }

    /// The app's current token and its `(account, user)`. The claims only
    /// bind the room selection to this login; the server verifies the token.
    pub fn auth(&mut self, refresh: bool) -> Result<(String, (String, String)), HostError> {
        let result = self.call("getAuthStatus", json!({"includeToken": true, "refreshToken": refresh}))?;
        let method = result.get("authMethod").and_then(Value::as_str);
        if !matches!(method, Some("chatgpt" | "chatgptAuthTokens")) {
            return Err(attention("Sign into the native ChatGPT app on the selected host."));
        }
        let token = match result.get("authToken").and_then(Value::as_str) {
            Some(token) if token.chars().count() <= 32768 => token.to_string(),
            _ => return Err(attention("Native ChatGPT login is unavailable.")),
        };
        let renewal = || attention("The native login needs renewal in the ChatGPT app.");
        let payload = token.split('.').nth(1).ok_or_else(renewal)?;
        let claims: Value =
            decode_segment(payload).and_then(|d| serde_json::from_slice(&d).ok()).ok_or_else(renewal)?;
        let account = claims.get("https://api.openai.com/auth").filter(|a| a.is_object()).ok_or_else(renewal)?;
        let account_id = identifier(Some(account.get("chatgpt_account_id").ok_or_else(renewal)?))?;
        let user_id = identifier(Some(account.get("chatgpt_user_id").ok_or_else(renewal)?))?;
        let expires = claims.get("exp").and_then(Value::as_f64).ok_or_else(renewal)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        if expires <= now + 15.0 {
            if !refresh && method == Some("chatgpt") {
                return self.auth(true);
            }
            return Err(renewal());
        }
        Ok((token, (account_id, user_id)))
    }

    pub fn close(&mut self) {
        self.writer.take();
        if let Some(mut child) = self.child.take() {
            reap(&mut child, Duration::from_secs(5), Duration::from_secs(5));
        }
    }
}

impl Drop for AccountClient {
    fn drop(&mut self) {
        self.close();
    }
}

/// A JWT segment: URL-safe base64, padding optional.
fn decode_segment(text: &str) -> Option<Vec<u8>> {
    let mut standard: String = text
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    while !standard.len().is_multiple_of(4) {
        standard.push('=');
    }
    crate::util::unbase64(&standard)
}

/// The room of the app's cached profile for this Dot under this login: one
/// room exactly. An app cache still loading after boot is retried automatically.
pub fn cached_profile(codex_home: &Path, dot_id: &str, identity: &(String, String)) -> Result<String, HostError> {
    let file = File::open(codex_home.join(".codex-global-state.json")).map_err(os_error)?;
    let mut data = Vec::new();
    file.take(BODY_MAX + 1).read_to_end(&mut data).map_err(os_error)?;
    if data.len() as u64 > BODY_MAX {
        return Err(attention("Native app state exceeded its limit."));
    }
    let root: Value = serde_json::from_slice(&data).map_err(|_| Other("JSONDecodeError", false))?;
    let root = root.as_object().ok_or(Other("AttributeError", false))?;
    let mut stores = Vec::new();
    for (key, value) in root {
        if key == "electron-persisted-atom-state" || key.starts_with("electron-owner-atoms:") {
            stores.push(value.as_object().ok_or(Other("AttributeError", false))?);
        }
    }
    let account = json!([identity.0, identity.1]);
    let mut matches = Vec::new();
    for atoms in stores {
        for (key, value) in atoms {
            let Some(rest) = key.strip_prefix("cloud-aeon-profiles-v1:") else { continue };
            if serde_json::from_str::<Value>(rest).ok().as_ref() != Some(&account) {
                continue;
            }
            let Some(records) = value.as_object() else { continue };
            for record in records.values() {
                if record.get("status").and_then(Value::as_str) != Some("available") {
                    continue;
                }
                if let Some(profile) =
                    record.get("profile").filter(|p| p.get("id").and_then(Value::as_str) == Some(dot_id))
                {
                    matches.push(profile);
                }
            }
        }
        if let Some(legacy) = atoms.get("cloud-aeon-sidebar-cache-v1").filter(|l| l.is_object()) {
            if legacy.get("accountId").and_then(Value::as_str) == Some(identity.0.as_str())
                && legacy.get("userId").and_then(Value::as_str) == Some(identity.1.as_str())
            {
                if let Some(profiles) = legacy.get("profilesByThreadId").and_then(Value::as_object) {
                    matches.extend(profiles.values().filter(|p| p.get("id").and_then(Value::as_str) == Some(dot_id)));
                }
            }
        }
    }
    let mut rooms = HashSet::new();
    for profile in matches {
        rooms.insert(identifier(profile.get("messaging_room_id"))?);
    }
    match (rooms.len(), rooms.into_iter().next()) {
        (1, Some(room)) => Ok(room),
        (0, _) => Err(Transient("Waiting for the native app's Dot profile; reconnecting automatically.".into(), 5)),
        _ => Err(attention(
            "Native app has conflicting rooms for this Dot. Check the signed-in app before reconnecting.",
        )),
    }
}

/// A timeout is temporary: the chat reconnects, and a send it interrupted
/// stays journaled until its echo or the owner's `/resolve`.
fn transport_error(err: ureq::Error) -> HostError {
    match err {
        ureq::Error::Timeout(_) => Transient("Native request timed out.".into(), 5),
        ureq::Error::Io(err) => os_error(err),
        _ => Other("URLError", true),
    }
}

fn agent(limits: &Limits) -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(limits.connect))
        .timeout_per_read(Some(limits.stall))
        .timeout_per_write(Some(limits.stall))
        .timeout_recv_response(Some(limits.response))
        .timeout_global(Some(limits.total))
        .build()
        .new_agent()
}

/// One opened Dot room for one app login.
pub struct Client {
    env: Arc<HostEnv>,
    account: AccountClient,
    agent: ureq::Agent,
    dot_id: Option<String>,
    room_id: Option<String>,
    identity: Option<(String, String)>,
    actors: HashMap<String, &'static str>,
    activity: Option<Activity>,
    events: Option<live::Notify>,
    request_identity: Option<(String, String)>,
}

impl Client {
    /// `events` hears every live activity change while the room is open.
    pub fn new(env: Arc<HostEnv>, events: Option<live::Notify>) -> Result<Client, HostError> {
        let account = (env.account)()?;
        Ok(Client {
            agent: agent(&env.limits),
            env,
            account,
            dot_id: None,
            room_id: None,
            identity: None,
            actors: HashMap::new(),
            activity: None,
            events,
            request_identity: None,
        })
    }

    /// One API request; HTTP 401 renews the login once and only for the same principal.
    fn request(
        &mut self,
        method: &str,
        path: &str,
        token: &str,
        account_id: &str,
        body: Option<&Value>,
        refresh: bool,
    ) -> Result<Value, HostError> {
        let bytes = body.map(serde_json::to_vec).transpose().map_err(|_| Other("TypeError", false))?;
        self.request_body(method, path, token, account_id, bytes.as_deref().map(|b| ("application/json", b)), refresh)
    }

    fn request_body(
        &mut self,
        method: &str,
        path: &str,
        token: &str,
        account_id: &str,
        body: Option<(&str, &[u8])>,
        refresh: bool,
    ) -> Result<Value, HostError> {
        let url = format!("{}{}", self.env.api, path);
        let headers = [
            ("Authorization", format!("Bearer {token}")),
            ("ChatGPT-Account-Id", account_id.to_string()),
            ("originator", "Codex Desktop".to_string()),
            ("User-Agent", format!("Codex Desktop/{} (X11; Linux; x64)", self.account.version)),
            ("Accept", "application/json".to_string()),
            ("x-openai-web-frontend", "codex_webview".to_string()),
            ("x-openai-codex-window-type", "electron".to_string()),
            ("OAI-Language", "en".to_string()),
        ];
        let response = match body {
            Some((media, data)) => {
                let mut request = if method == "POST" { self.agent.post(&url) } else { self.agent.put(&url) };
                for (name, value) in &headers {
                    request = request.header(*name, value.as_str());
                }
                request.header("Content-Type", media).send(data)
            }
            None => {
                let mut request = self.agent.get(&url);
                for (name, value) in &headers {
                    request = request.header(*name, value.as_str());
                }
                request.call()
            }
        }
        .map_err(transport_error)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let delay = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<i64>().ok())
                .map_or(15, |delay| delay.clamp(5, 300) as u64);
            drop(response);
            if status == 429 || (500..=599).contains(&status) {
                return Err(Transient("Native service temporarily unavailable.".into(), delay));
            }
            if status == 401 && refresh {
                let (renewed, identity) = self.account.auth(true)?;
                if Some(&identity) != self.request_identity.as_ref() || identity.0 != account_id {
                    return Err(attention("Native account changed during login renewal."));
                }
                let dot_id = self.dot_id.clone().unwrap_or_default();
                let cached = cached_profile(&self.env.codex_home, &dot_id, &identity)?;
                if self.room_id.as_ref().is_some_and(|room| *room != cached) {
                    return Err(attention("Native Dot room changed during login renewal."));
                }
                return self.request_body(method, path, &renewed, account_id, body, false);
            }
            return Err(Attention(format!(
                "Native Dot request returned HTTP {status}. Check app login and Dot access; nothing is resent."
            )));
        }
        let raw = match response.into_body().with_config().limit(BODY_MAX).read_to_vec() {
            Ok(raw) => raw,
            Err(ureq::Error::BodyExceedsLimit(_)) => return Err(attention("Native Dot response exceeded its limit.")),
            Err(ureq::Error::Timeout(_)) => return Err(Transient("Native response timed out.".into(), 5)),
            Err(err) => return Err(transport_error(err)),
        };
        serde_json::from_slice(&raw).map_err(|_| Other("JSONDecodeError", false))
    }

    /// The current token, checked against the opened login and room.
    fn credentials(&mut self) -> Result<(String, (String, String), String), HostError> {
        let (token, identity) = self.account.auth(false)?;
        if self.identity.as_ref().is_some_and(|opened| *opened != identity) {
            return Err(attention("Native app account changed. Reconnect to the intended account."));
        }
        let room = cached_profile(&self.env.codex_home, self.dot_id.as_deref().unwrap_or(""), &identity)?;
        if self.room_id.as_ref().is_some_and(|opened| *opened != room) {
            return Err(attention("Dot room changed. Check delivery, then reconnect."));
        }
        self.request_identity = Some(identity.clone());
        Ok((token, identity, room))
    }

    pub fn open(&mut self, url: Option<&Value>) -> Result<Value, HostError> {
        let dot_id = target_id(url)?;
        self.dot_id = Some(dot_id.clone());
        let (token, identity, room) = self.credentials()?;
        let metadata = self.request(
            "GET",
            &format!("/messaging/rooms/{}", quote(&room, "_.-~")),
            &token,
            &identity.0,
            None,
            true,
        )?;
        let metadata = metadata.as_object().ok_or(Other("AttributeError", false))?;
        if metadata.get("id").and_then(Value::as_str) != Some(room.as_str())
            || metadata.get("aeon_id").and_then(Value::as_str) != Some(dot_id.as_str())
            || metadata.get("type").and_then(Value::as_str) != Some("DM")
            || metadata.get("app_source").and_then(Value::as_str) != Some("chatgpt:messaging")
        {
            return Err(attention("Native room does not match the selected Dot."));
        }
        let members = match metadata.get("members") {
            None => Vec::new(),
            Some(Value::Array(members)) if members.iter().all(Value::is_object) => members.clone(),
            Some(_) => return Err(Other("AttributeError", false)),
        };
        let dots: Vec<&Value> =
            members.iter().filter(|m| m.get("aeon_id").and_then(Value::as_str) == Some(dot_id.as_str())).collect();
        let humans: Vec<&Value> = members.iter().filter(|m| !m.get("aeon_id").is_some_and(super::truthy)).collect();
        if members.len() != 2 || dots.len() != 1 || humans.len() != 1 {
            return Err(attention("Native Dot room membership is ambiguous."));
        }
        let mut actors = HashMap::new();
        actors.insert(identifier(humans[0].get("account_user_id"))?, "user");
        actors.insert(identifier(dots[0].get("account_user_id"))?, "assistant");
        if actors.len() != 2 {
            return Err(attention("Native Dot sender identity is ambiguous."));
        }
        self.actors = actors;
        self.identity = Some(identity.clone());
        self.room_id = Some(room.clone());
        if let Some(activity) = self.activity.take() {
            activity.close();
        }
        let events = self.events.clone();
        self.activity = self
            .env
            .live
            .clone()
            .map(|settings| Activity::start(settings, &token, &identity.0, &room, &self.account.version, events));
        self.snapshot()
    }

    pub fn snapshot(&mut self) -> Result<Value, HostError> {
        let (token, identity, room) = self.credentials()?;
        let path = format!("/messaging/rooms/{}/messages?limit=32", quote(&room, "_.-~"));
        let response = self.request("GET", &path, &token, &identity.0, None, true)?;
        let items = match response.as_object().ok_or(Other("AttributeError", false))?.get("items") {
            Some(Value::Array(items)) if items.len() <= 500 => items.clone(),
            _ => return Err(attention("Unsupported native message response.")),
        };
        let mut messages = Vec::new();
        let mut deleted = Vec::new();
        for item in &items {
            let item = item.as_object().ok_or(Other("AttributeError", false))?;
            if item.get("deleted_at").is_some_and(super::truthy) {
                deleted.push(identifier(item.get("id"))?);
                continue;
            }
            let role = item.get("account_user_id").and_then(Value::as_str).and_then(|id| self.actors.get(id));
            let Some(role) = role.copied() else {
                return Err(attention("Unsupported native message author. Update the adapter."));
            };
            let text = match item.get("content") {
                None => None,
                Some(content) => content.as_object().ok_or(Other("AttributeError", false))?.get("text"),
            };
            let attachments = item.get("content").and_then(|c| c.get("attachments")).and_then(Value::as_array)
                .into_iter().flatten().filter(|a| a.get("type").and_then(Value::as_str) == Some("file"))
                .map(|a| Ok(json!({"id":identifier(a.get("file_id"))?, "name":super::clip_chars(a.get("name").and_then(Value::as_str).unwrap_or("image"),100)})))
                .collect::<Result<Vec<Value>,HostError>>()?;
            let Some(text) = text.and_then(Value::as_str).or_else(|| (!attachments.is_empty()).then_some("")) else {
                return Err(attention("Unsupported native message content. Update the adapter."));
            };
            if !text.is_empty() || !attachments.is_empty() {
                messages.push(json!({
                    "id": identifier(item.get("id"))?,
                    "role": role,
                    "text": super::clip_chars(text, 100_000),
                    "at": item.get("created_at").cloned().unwrap_or(Value::Null),
                    "attachments": attachments,
                }));
            }
        }
        let sort_key = |m: &Value| {
            let at = m.get("at").and_then(Value::as_str).unwrap_or("").to_string();
            (at, m["id"].as_str().unwrap_or("").to_string())
        };
        messages.sort_by_key(sort_key);
        let activity = match &self.activity {
            Some(activity) => {
                activity.update(&token);
                activity.status()
            }
            None => Status::unknown(),
        };
        let label =
            if LABELS.contains(&activity.label.as_str()) { activity.label.clone() } else { live::UNAVAILABLE.into() };
        Ok(json!({
            "dot_id": self.dot_id,
            "room_id": self.room_id,
            "messages": messages,
            "deleted": deleted,
            "working": activity.working,
            "activity_known": activity.known,
            "activity_label": label,
            "activity_turn_id": activity.turn_id,
            "activity_revision": activity.revision,
        }))
    }

    pub(super) fn send_with_files(
        &mut self,
        text: Option<&Value>,
        request_id: Option<&Value>,
        files: Option<&Value>,
    ) -> Result<Value, HostError> {
        let files = files.and_then(Value::as_array).cloned().unwrap_or_default();
        if files.len() > 10 {
            return Err(attention("Too many Dot images."));
        }
        let files = files.iter().map(|id| identifier(Some(id))).collect::<Result<Vec<_>, _>>()?;
        let text = match text.and_then(Value::as_str) {
            Some(text)
                if (!text.chars().all(super::is_space) || !files.is_empty()) && text.chars().count() <= 32000 =>
            {
                text.to_string()
            }
            _ => return Err(attention("Message must contain 1 to 32000 characters.")),
        };
        let request_id = match request_id.and_then(Value::as_str) {
            Some(id) if id.len() == 36 && id.bytes().all(|b| b"0123456789abcdef-".contains(&b)) => id.to_string(),
            _ => return Err(attention("Invalid delivery identity.")),
        };
        let (token, identity, room) = self.credentials()?;
        let body = json!({
            "content": {"text": text, "attachments":files.iter().map(|id| json!({"type":"file","file_id":id})).collect::<Vec<_>>()},
            "request_id": request_id,
            "idempotency_token": request_id,
            "timezone_offset_min": timezone_offset_min(),
        });
        let path = format!("/messaging/rooms/{}/messages", quote(&room, "_.-~"));
        let response = self.request("POST", &path, &token, &identity.0, Some(&body), true)?;
        Ok(json!({"id": identifier(response.get("id"))?}))
    }

    /// The app's room-scoped multipart upload. The image never enters a command
    /// argument or a log, and a failed upload never submits a chat message.
    pub(super) fn upload(&mut self, value: Option<&Value>) -> Result<Value, HostError> {
        let image: super::api::Image = serde_json::from_value(value.cloned().unwrap_or(Value::Null))
            .map_err(|_| attention("Invalid Dot image."))?;
        let (bytes, media) = image.bytes().map_err(Attention)?;
        let (token, identity, room) = self.credentials()?;
        let boundary = format!("cepheus-{}", crate::util::uuid_v4().map_err(os_error)?);
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: {media}\r\n\r\n", image.name).into_bytes();
        body.extend_from_slice(&bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let path = format!("/messaging/rooms/{}/files", quote(&room, "_.-~"));
        let media = format!("multipart/form-data; boundary={boundary}");
        let response = self.request_body("POST", &path, &token, &identity.0, Some((&media, &body)), true)?;
        Ok(json!({"id":identifier(response.get("id"))?}))
    }

    /// The Dot's card, with its picture only when the owner's `have` key is
    /// not the current one. Any failure keeps the last card: it never ends
    /// the connection.
    pub fn profile(&mut self, have: Option<&Value>) -> Value {
        let card = (|| {
            let dot_id = self.dot_id.clone()?;
            let (token, identity, _) = self.credentials().ok()?;
            let path = format!("/tbo/{}", quote(&dot_id, "_.-~"));
            let value = self.request("GET", &path, &token, &identity.0, None, true).ok()?;
            profile::card(&value)
        })();
        let Some((card, url)) = card else { return json!({"card": null, "avatar": null}) };
        let have = have.and_then(Value::as_str);
        let avatar = match (&card.image, url) {
            (Some(image), Some(url)) if Some(image.as_str()) != have => self.picture(&url),
            _ => None,
        };
        let avatar = avatar.map(|(media, data)| json!({"media": media, "data": crate::util::base64(&data)}));
        json!({"card": card.to_json(), "avatar": avatar})
    }

    /// Download the picture without credentials: the URL is already signed.
    fn picture(&self, url: &str) -> Option<(&'static str, Vec<u8>)> {
        if !profile::avatar_allowed(url, self.env.loopback) {
            return None;
        }
        let response = self.agent.get(url).call().ok()?;
        if !response.status().is_success() {
            return None;
        }
        let data = response.into_body().with_config().limit(profile::AVATAR_MAX as u64).read_to_vec().ok()?;
        Some((profile::media(&data)?, data))
    }

    pub fn close(&mut self) {
        if let Some(activity) = self.activity.take() {
            activity.close();
        }
        self.account.close();
    }
}

/// Minutes behind UTC, as the app sends it.
fn timezone_offset_min() -> i64 {
    // SAFETY: localtime_r fills a tm this function owns.
    let offset = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_gmtoff as i64
    };
    (-offset).div_euclid(60)
}

fn handle(
    env: &Arc<HostEnv>,
    events: &live::Notify,
    client: &mut Option<Client>,
    raw: &[u8],
    id: &mut Value,
) -> Result<Value, HostError> {
    let request: Value = serde_json::from_slice(raw).map_err(|_| Other("JSONDecodeError", false))?;
    let request = request.as_object().ok_or(Other("AttributeError", false))?;
    *id = request.get("id").cloned().unwrap_or(Value::Null);
    let action = request.get("action").and_then(Value::as_str);
    match (action, client.as_mut()) {
        (Some("hello"), None) => Ok(json!({"version": VERSION, "protocol": PROTOCOL, "capabilities":["images"]})),
        (Some("open"), None) => {
            client.insert(Client::new(Arc::clone(env), Some(Arc::clone(events)))?).open(request.get("url"))
        }
        (Some("snapshot"), Some(open)) => open.snapshot(),
        (Some("send"), Some(open)) => {
            open.send_with_files(request.get("text"), request.get("request_id"), request.get("files"))
        }
        (Some("upload"), Some(open)) => open.upload(request.get("image")),
        (Some("profile"), Some(open)) => Ok(open.profile(request.get("have"))),
        _ => Err(attention("Unsupported native bridge request.")),
    }
}

/// Answer requests until the input ends. Any failure closes the client: the
/// chat reconnects or asks the owner, and nothing is resent. A live activity
/// change is announced between answers, so the chat refreshes only then.
pub fn serve(env: Arc<HostEnv>, input: impl Read, output: impl Write + Send + 'static) {
    let output: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(output)));
    let announce = Arc::clone(&output);
    let events: live::Notify = Arc::new(move || {
        let mut output = announce.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = output.write_all(b"{\"event\":\"live\"}\n").and_then(|_| output.flush());
    });
    let mut reader = BufReader::new(input);
    let mut client: Option<Client> = None;
    loop {
        let mut raw = Vec::new();
        match (&mut reader).take(REQUEST_MAX as u64 + 1).read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) if raw.len() > REQUEST_MAX => break,
            Ok(_) => {}
        }
        let mut id = Value::Null;
        let answer = match handle(&env, &events, &mut client, &raw, &mut id) {
            Ok(result) => json!({"id": id, "ok": true, "result": result}),
            Err(err) => {
                if let Some(mut open) = client.take() {
                    open.close();
                }
                match err {
                    Transient(text, delay) => {
                        json!({"id": id, "ok": false, "error": text, "retry": true, "retry_after": delay})
                    }
                    Attention(text) => json!({"id": id, "ok": false, "error": text}),
                    Other(kind, retry) => json!({
                        "id": id, "ok": false,
                        "error": format!("Native connection unavailable ({kind})."), "retry": retry,
                    }),
                }
            }
        };
        let mut output = output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if writeln!(output, "{answer}").and_then(|_| output.flush()).is_err() {
            break;
        }
    }
    if let Some(mut open) = client.take() {
        open.close();
    }
}

/// `cepheus dot host`.
pub fn main() -> i32 {
    // SAFETY: umask has no preconditions.
    unsafe { libc::umask(0o077) };
    serve(Arc::new(HostEnv::production()), io::stdin().lock(), io::stdout());
    0
}
