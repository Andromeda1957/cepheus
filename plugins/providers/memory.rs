//! Conservative, bounded transcript signals for a memory-killed test/build.
//!
//! Only typed Claude Bash results/task notifications and Codex exec_command /
//! write_stdin results are correlated. User prose, exit 137 and SIGKILL alone
//! are not evidence. Antigravity, Grok and opencode have no verified typed
//! memory-kill signal here; their screens deliberately produce no rerun offer.
//! Codex code-mode wrappers without typed inner command metadata and untyped
//! Claude notices likewise stay silent rather than inferring a command.
//! Commands stay in these private facts, never categorical events or metrics.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::codex;
use crate::config::CommandWrapper;
use crate::protocol::Tool;

const MAX_COMMAND: usize = 4000;
const MAX_CALLS: usize = 64;
const MAX_SESSIONS: usize = 32;
const MAX_KILLS: usize = 16;
const MAX_RECORD: usize = 512 * 1024;
const READ_LIMIT: u64 = 512 * 1024;
const INITIAL_TAIL: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemoryKill {
    /// Stable provider call/task identity, independent of scan time.
    pub id: String,
    /// Exact original test/build shell command; oversized commands are omitted.
    pub command: String,
    /// Original exec tool directory, without rewriting the command text.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Unix seconds from the provider result; zero means unavailable, never fresh.
    pub at: u64,
    /// Set only by the service's repository/session hook ownership metadata.
    pub authoritative: bool,
}

#[derive(Clone)]
struct Origin {
    id: String,
    command: String,
    cwd: Option<String>,
}

struct Call {
    id: String,
    origin: Option<Origin>,
    session: Option<String>,
    claude: bool,
    description: Option<String>,
}

#[derive(Default)]
pub(super) struct Scanner {
    path: PathBuf,
    identity: Option<(u64, u64)>,
    offset: u64,
    record: Vec<u8>,
    skipping: bool,
    calls: VecDeque<Call>,
    sessions: VecDeque<(String, Origin)>,
    kills: VecDeque<MemoryKill>,
    codex_items: bool,
    anchor: Vec<u8>,
    written: Option<(i64, i64)>,
}

impl Scanner {
    /// Read only new bytes after a bounded initial tail. A truncated/replaced
    /// transcript resets correlation; a partial/oversized JSON record cannot
    /// attach an old command to a later result. `wrappers` are read through
    /// (`safe_rerun`).
    pub(super) fn read(&mut self, path: &Path, tool: Tool, _now: u64, wrappers: &[CommandWrapper]) -> Vec<MemoryKill> {
        if !matches!(tool, Tool::Claude | Tool::Codex) {
            return Vec::new();
        }
        let Ok(mut file) = fs::File::open(path) else { return Vec::new() };
        let Ok(meta) = file.metadata() else { return Vec::new() };
        if !meta.is_file() {
            return Vec::new();
        }
        let identity = (meta.dev(), meta.ino());
        let written = (meta.mtime(), meta.mtime_nsec());
        // Pin one descriptor for identity, boundary and data reads. A path
        // replacement cannot attach the old file's command to a new result.
        // Copy-truncate/regrowth changes the last 64 bytes we consumed.
        let rewritten = self.path == path
            && self.identity == Some(identity)
            && !self.anchor.is_empty()
            && (meta.len() != self.offset || self.written != Some(written))
            && read_bytes(&mut file, self.offset.saturating_sub(self.anchor.len() as u64), self.offset)
                .is_ok_and(|bytes| bytes != self.anchor);
        if self.path != path || self.identity != Some(identity) || meta.len() < self.offset || rewritten {
            *self = Self {
                path: path.to_path_buf(),
                identity: Some(identity),
                offset: meta.len().saturating_sub(INITIAL_TAIL),
                skipping: meta.len() > INITIAL_TAIL,
                ..Self::default()
            };
        }
        let end = meta.len().min(self.offset.saturating_add(READ_LIMIT));
        if end > self.offset {
            if let Ok(bytes) = read_bytes(&mut file, self.offset, end) {
                self.offset += bytes.len() as u64;
                self.anchor.extend_from_slice(&bytes);
                if self.anchor.len() > 64 {
                    self.anchor.drain(..self.anchor.len() - 64);
                }
                for byte in bytes {
                    if byte == b'\n' {
                        if !self.skipping {
                            let record = std::mem::take(&mut self.record);
                            if let Ok(value) = serde_json::from_slice::<Value>(&record) {
                                let at = value
                                    .get("timestamp")
                                    .and_then(Value::as_str)
                                    .and_then(codex::parse_rfc3339_ms)
                                    .map_or(0, |ms| ms / 1000);
                                match tool {
                                    Tool::Claude => self.claude(&value, at, wrappers),
                                    Tool::Codex => self.codex(&value, at, wrappers),
                                    _ => {}
                                }
                            } else {
                                self.calls.clear();
                                self.sessions.clear();
                            }
                        }
                        self.skipping = false;
                    } else if !self.skipping {
                        if self.record.len() < MAX_RECORD {
                            self.record.push(byte);
                        } else {
                            self.record.clear();
                            self.calls.clear();
                            self.sessions.clear();
                            self.skipping = true;
                        }
                    }
                }
            }
        }
        self.written = Some(written);
        self.kills.iter().cloned().collect()
    }

    fn remember(&mut self, call: Call) {
        self.calls.retain(|old| old.id != call.id);
        // Unrelated Claude Bash inspection calls must not evict a long-running build.
        if call.claude && call.origin.is_none() {
            return;
        }
        self.calls.push_back(call);
        while self.calls.len() > MAX_CALLS {
            self.calls.pop_front();
        }
    }

    fn killed(&mut self, prefix: &str, origin: &Origin, at: u64) {
        let id = format!("{prefix}:{}", origin.id);
        if self.kills.iter().any(|kill| kill.id == id) {
            return;
        }
        self.kills.push_back(MemoryKill {
            id,
            command: origin.command.clone(),
            cwd: origin.cwd.clone(),
            at,
            authoritative: false,
        });
        while self.kills.len() > MAX_KILLS {
            self.kills.pop_front();
        }
    }

    fn codex(&mut self, value: &Value, at: u64, wrappers: &[CommandWrapper]) {
        let replayable = |raw: &str| command(raw, wrappers);
        let Some(payload) = value.get("payload") else { return };
        if value["type"].as_str() == Some("event_msg") {
            if matches!(payload["type"].as_str(), Some("item_started" | "item_completed")) {
                self.codex_items = true;
            }
            let item = &payload["item"];
            if payload["type"].as_str() == Some("item_completed")
                && item["type"].as_str() == Some("CommandExecution")
                && item["exit_code"].as_i64().is_some_and(|code| code != 0)
                && item["aggregated_output"].as_str().is_some_and(definitive_kill)
            {
                if let (Some(id), Some(command)) =
                    (item["id"].as_str().filter(|id| valid_id(id)), shell_command(item).and_then(replayable))
                {
                    self.killed("codex", &Origin { id: id.into(), command, cwd: None }, at);
                }
            }
            return;
        }
        if value["type"].as_str() != Some("response_item") || self.codex_items {
            return;
        }
        let Some(id) = payload.get("call_id").and_then(Value::as_str).filter(|id| valid_id(id)) else { return };
        match payload.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                self.calls.retain(|call| call.id != id);
                let Some(args) = payload
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                else {
                    return;
                };
                match payload.get("name").and_then(Value::as_str) {
                    Some("exec_command" | "functions.exec_command") => {
                        let origin = shell_command(&args)
                            .and_then(replayable)
                            .filter(|_| {
                                args.get("workdir")
                                    .is_none_or(|cwd| cwd.is_null() || cwd.as_str().is_some_and(valid_cwd))
                            })
                            .map(|command| Origin {
                                id: id.into(),
                                command,
                                cwd: args.get("workdir").and_then(Value::as_str).map(str::to_owned),
                            });
                        self.remember(Call { id: id.into(), origin, session: None, claude: false, description: None });
                    }
                    Some("write_stdin" | "functions.write_stdin") => {
                        let Some(session) = args.get("session_id").and_then(identifier) else { return };
                        // Sending input can change what a persistent shell is executing.
                        let poll = args.get("chars").is_none_or(|v| v.as_str() == Some(""));
                        if !poll {
                            self.sessions.retain(|(old, _)| old != &session);
                        }
                        let origin = poll
                            .then(|| {
                                self.sessions.iter().find(|(old, _)| old == &session).map(|(_, origin)| origin.clone())
                            })
                            .flatten();
                        self.remember(Call {
                            id: id.into(),
                            origin,
                            session: Some(session),
                            claude: false,
                            description: None,
                        });
                    }
                    _ => {}
                }
            }
            Some("function_call_output") => {
                let Some(index) = self.calls.iter().position(|call| call.id == id && !call.claude) else { return };
                let call = self.calls.remove(index).unwrap();
                if let Some(session) = &call.session {
                    let current =
                        self.sessions.iter().find(|(id, _)| id == session).map(|(_, origin)| origin.id.as_str());
                    if current != call.origin.as_ref().map(|origin| origin.id.as_str()) {
                        return;
                    }
                }
                let output = Output::parse(payload.get("output").unwrap_or(&Value::Null));
                if call.session.as_ref().zip(output.session.as_ref()).is_some_and(|(a, b)| a != b) {
                    self.sessions
                        .retain(|(id, _)| Some(id) != call.session.as_ref() && Some(id) != output.session.as_ref());
                    return;
                }
                if output.exit != Some(0) && definitive_kill(&output.text) {
                    if let Some(origin) = &call.origin {
                        self.killed("codex", origin, at);
                    }
                }
                if let Some(session) = output.session.or(call.session) {
                    // Always replace a reused process session id, even for a
                    // non-build command: an older build must never leak into it.
                    self.sessions.retain(|(old, _)| old != &session);
                    if output.exit.is_none() {
                        if let Some(origin) = call.origin {
                            self.sessions.push_back((session, origin));
                            while self.sessions.len() > MAX_SESSIONS {
                                self.sessions.pop_front();
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn claude(&mut self, value: &Value, at: u64, wrappers: &[CommandWrapper]) {
        let replayable = |raw: &str| command(raw, wrappers);
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else { return };
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_use")
                        || block.get("name").and_then(Value::as_str) != Some("Bash")
                    {
                        continue;
                    }
                    let Some(id) = block.get("id").and_then(Value::as_str).filter(|id| valid_id(id)) else { continue };
                    let input = &block["input"];
                    let origin = input
                        .get("command")
                        .and_then(Value::as_str)
                        .and_then(replayable)
                        .map(|command| Origin { id: id.into(), command, cwd: None });
                    self.remember(Call {
                        id: id.into(),
                        origin,
                        session: None,
                        claude: true,
                        description: input
                            .get("description")
                            .and_then(Value::as_str)
                            .filter(|s| s.len() <= 1024)
                            .map(str::to_owned),
                    });
                }
            }
            Some("user") => {
                // A human can quote any notice in a text block. Only tool
                // result blocks have provider-owned call correlation here.
                let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else { return };
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result")
                        || block.get("is_error").and_then(Value::as_bool) != Some(true)
                    {
                        continue;
                    }
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else { continue };
                    if let Some(origin) =
                        self.calls.iter().find(|call| call.claude && call.id == id).and_then(|call| call.origin.clone())
                    {
                        if definitive_kill(&text(&block["content"])) {
                            self.killed("claude", &origin, at);
                        }
                    }
                }
            }
            Some("attachment") => {
                let attachment = &value["attachment"];
                if attachment["type"].as_str() != Some("queued_command")
                    || attachment["commandMode"].as_str() != Some("task-notification")
                    || attachment.pointer("/origin/kind").and_then(Value::as_str) != Some("task-notification")
                {
                    return;
                }
                let prompt = text(&attachment["prompt"]);
                for notice in prompt.split("<task-notification>").skip(1) {
                    let Some((notice, _)) = notice.split_once("</task-notification>") else { continue };
                    if tag(notice, "status") != Some("killed") {
                        continue;
                    }
                    let (Some(task), Some(id), Some(summary)) =
                        (tag(notice, "task-id"), tag(notice, "tool-use-id"), tag(notice, "summary"))
                    else {
                        continue;
                    };
                    if !valid_id(task) {
                        continue;
                    }
                    let Some(call) = self.calls.iter().find(|call| call.claude && call.id == id) else { continue };
                    let (Some(origin), Some(description)) = (call.origin.clone(), &call.description) else { continue };
                    let expected = format!(
                        "Background command \"{description}\" was stopped because the system is running low on memory"
                    );
                    if summary == expected {
                        self.killed("claude-task", &Origin { id: task.into(), ..origin }, at);
                    }
                }
            }
            _ => {}
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}

fn valid_cwd(cwd: &str) -> bool {
    cwd.len() <= MAX_COMMAND && Path::new(cwd).is_absolute() && !cwd.chars().any(char::is_control)
}

fn read_bytes(file: &mut fs::File, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let limit = end.saturating_sub(start).min(READ_LIMIT);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(limit as usize);
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn identifier(value: &Value) -> Option<String> {
    value.as_u64().map(|n| n.to_string()).or_else(|| value.as_str().filter(|s| valid_id(s)).map(str::to_owned))
}

fn shell_command(input: &Value) -> Option<&str> {
    let value = input.get("cmd").or_else(|| input.get("command"))?;
    value.as_str().or_else(|| {
        let args = value.as_array()?;
        let shell = args.first()?.as_str()?.rsplit('/').next()?;
        (matches!(shell, "sh" | "bash" | "zsh" | "dash") && matches!(args.get(1)?.as_str()?, "-c" | "-lc"))
            .then(|| args.get(2)?.as_str())
            .flatten()
    })
}

#[derive(Default)]
struct Output {
    text: String,
    session: Option<String>,
    exit: Option<i64>,
}

impl Output {
    fn parse(value: &Value) -> Self {
        if let Some(raw) = value.as_str() {
            if let Ok(decoded) = serde_json::from_str::<Value>(raw) {
                if decoded.is_object() {
                    return Self::parse(&decoded);
                }
            }
            let boundary = ["\nOutput:\n", "\nFinal output:\n"]
                .into_iter()
                .filter_map(|marker| raw.find(marker).map(|at| (at, marker.len())))
                .min_by_key(|(at, _)| *at);
            let (header, body) = boundary.map_or(("", raw), |(at, len)| (&raw[..at], &raw[at + len..]));
            let mut out = Self { text: body.into(), ..Self::default() };
            for line in header.lines().take(16) {
                if let Some(id) = line
                    .strip_prefix("Process running with session ID ")
                    .or_else(|| line.strip_prefix("Process still running with session ID "))
                {
                    out.session = id.split_whitespace().next().filter(|id| valid_id(id)).map(str::to_owned);
                }
                if let Some(code) = line.strip_prefix("Process exited with code ") {
                    out.exit = code.trim().parse().ok();
                }
            }
            out
        } else {
            Self {
                text: text(value.get("output").unwrap_or(value)),
                session: value.get("session_id").and_then(identifier),
                exit: value.get("exit_code").or_else(|| value.pointer("/metadata/exit_code")).and_then(Value::as_i64),
            }
        }
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block["type"].as_str() == Some("text"))
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn tag<'a>(notice: &'a str, name: &str) -> Option<&'a str> {
    notice.split_once(&format!("<{name}>"))?.1.split_once(&format!("</{name}>")).map(|(value, _)| value.trim())
}

fn definitive_kill(output: &str) -> bool {
    output.lines().any(|line| {
        let lower = line.trim().to_ascii_lowercase();
        let line = lower.strip_prefix('[').and_then(|s| s.split_once(']')).map_or(lower.as_str(), |(_, s)| s.trim());
        line.starts_with("out of memory: killed process ")
            || line.starts_with("oom-kill:constraint=")
            || matches!(
                line,
                "killed by oom killer"
                    | "killed by the oom killer"
                    | "killed due to out of memory"
                    | "killed (out of memory)"
            )
    })
}

fn command(raw: &str, wrappers: &[CommandWrapper]) -> Option<String> {
    safe_rerun(raw, wrappers).then(|| raw.to_owned())
}

const FORBIDDEN: &[&str] = &[
    "merge",
    "push",
    "ship",
    "deploy",
    "release",
    "publish",
    "delete",
    "remove",
    "rm",
    "rmdir",
    "reset",
    "clean",
    "install",
    "uninstall",
    "dispatch",
];

/// Conservative replay validation, shared with the final service action gate.
/// The exact text is retained; never truncate or reconstruct it for delivery.
/// A configured wrapper (`Conventions::command_wrappers`) is read through to
/// the command it runs.
pub fn safe_rerun(raw: &str, wrappers: &[CommandWrapper]) -> bool {
    raw.len() <= MAX_COMMAND
        && !raw.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|word| FORBIDDEN.contains(&word))
        && !raw.contains(['\0', '`', '<', '>'])
        && !raw.contains("$(")
        && test_or_build(raw, 0, wrappers)
}

/// A small lexer preserves quoted shell arguments. Every segment must be a
/// test/build or narrow directory/environment setup: finding one test in an
/// arbitrary shell compound does not authorize replaying its other commands.
fn test_or_build(raw: &str, depth: usize, wrappers: &[CommandWrapper]) -> bool {
    if depth > 4 {
        return false;
    }
    let mut parts = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in raw.chars().chain(std::iter::once('\n')) {
        if escaped {
            word.push(ch);
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_some() {
            word.push(ch);
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if ch.is_whitespace() || matches!(ch, ';' | '&' | '|') {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            if matches!(ch, '\n' | ';' | '&' | '|') && !words.is_empty() {
                parts.push(std::mem::take(&mut words));
            }
        } else {
            word.push(ch);
        }
    }
    if quote.is_some() || escaped {
        return false;
    }
    let mut found = false;
    for args in parts {
        match safe_segment(&args, depth, wrappers) {
            Some(build) => found |= build,
            None => return false,
        }
    }
    found
}

/// Some(true) is a test/build, Some(false) is setup, None is unrecognized.
fn safe_segment(mut args: &[String], depth: usize, wrappers: &[CommandWrapper]) -> Option<bool> {
    if depth > 4 {
        return None;
    }
    while args.first().is_some_and(|a| assignment(a)) {
        args = &args[1..];
    }
    let Some(program) = args.first().and_then(|p| p.rsplit('/').next()) else { return Some(false) };
    let rest = &args[1..];
    // These can confer integration or irreversible authority even when a
    // preceding segment happened to run tests. Keep them out of all wrappers.
    if args.iter().any(|arg| FORBIDDEN.contains(&arg.as_str())) {
        return None;
    }
    if wraps(args, wrappers) {
        return wrapped(args, depth, wrappers);
    }
    let first = rest.first().map(String::as_str);
    let action = |allowed: &[&str]| first.is_some_and(|s| allowed.contains(&s));
    match program {
        "cd" if rest.len() == 1 && !rest[0].starts_with('-') => Some(false),
        "export" if !rest.is_empty() && rest.iter().all(|a| assignment(a)) => Some(false),
        "cargo" => {
            let verb = rest.iter().find(|s| {
                !matches!(s.as_str(), "--offline" | "--locked" | "--frozen" | "-q" | "--quiet" | "-v" | "--verbose")
                    && !s.starts_with('+')
            });
            verb.filter(|s| ["test", "build", "check", "clippy", "bench"].contains(&s.as_str())).map(|_| true)
        }
        "pytest" | "pytest-3" => Some(true),
        "make" | "gmake" | "ninja" => rest
            .iter()
            .all(|s| {
                s.starts_with('-')
                    || assignment(s)
                    || matches!(s.as_str(), "all" | "build" | "test" | "tests" | "check")
            })
            .then_some(true),
        "gradle" | "gradlew" => rest
            .iter()
            .any(|s| {
                matches!(s.as_str(), "test" | "build" | "check" | "assemble")
                    || s.starts_with("test")
                    || s.ends_with("UnitTest")
            })
            .then_some(true),
        "npm" | "pnpm" | "yarn" | "bun" => (action(&["test", "build", "check", "typecheck"])
            || (first == Some("run")
                && rest.get(1).is_some_and(|s| ["test", "build", "check", "typecheck"].contains(&s.as_str()))))
        .then_some(true),
        "go" | "dotnet" | "swift" => action(&["test", "build"]).then_some(true),
        "mvn" | "mvnw" => action(&["test", "verify", "package", "compile"]).then_some(true),
        "cmake" => (first == Some("--build")).then_some(true),
        "python" | "python3" => {
            if (first == Some("-m") && rest.get(1).is_some_and(|s| matches!(s.as_str(), "pytest" | "unittest")))
                || (first.is_some_and(|s| s.ends_with("/manage.py") || s == "manage.py")
                    && rest.get(1).is_some_and(|s| s == "test"))
            {
                Some(true)
            } else if wraps(rest, wrappers) {
                // A wrapper script run by the interpreter.
                wrapped(rest, depth, wrappers)
            } else {
                None
            }
        }
        "sh" | "bash" | "zsh" | "dash" => (rest.len() == 2 && matches!(first, Some("-c" | "-lc")))
            .then(|| test_or_build(&rest[1], depth + 1, wrappers))
            .filter(|build| *build),
        "uv" if first == Some("run") => safe_segment(&rest[1..], depth + 1, wrappers).filter(|build| *build),
        "env" => {
            let i = rest.iter().position(|s| !s.starts_with('-') && !assignment(s))?;
            safe_segment(&rest[i..], depth + 1, wrappers).filter(|build| *build)
        }
        "timeout" => {
            let duration = rest.iter().position(|s| !s.starts_with('-'))?;
            if !rest[duration].chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return None;
            }
            safe_segment(&rest[duration + 1..], depth + 1, wrappers).filter(|build| *build)
        }
        "nice" => {
            let skip = if first == Some("-n") { 2 } else { 0 };
            safe_segment(rest.get(skip..)?, depth + 1, wrappers).filter(|build| *build)
        }
        _ => None,
    }
}

/// Whether `args` runs one of `wrappers`: its first word, or that word's
/// file name, is the wrapper's program, followed by its subcommand (when it
/// has one).
fn wraps(args: &[String], wrappers: &[CommandWrapper]) -> bool {
    let Some(word) = args.first() else { return false };
    let name = word.rsplit('/').next().unwrap_or(word);
    wrappers.iter().any(|wrapper| {
        !wrapper.program.is_empty()
            && (wrapper.program == *word || wrapper.program == name)
            && (wrapper.subcommand.is_empty() || args.get(1).is_some_and(|s| *s == wrapper.subcommand))
    })
}

/// A wrapper's run is a test/build only when the command after its first
/// `--` is one; its own options before that are not interpreted.
fn wrapped(args: &[String], depth: usize, wrappers: &[CommandWrapper]) -> Option<bool> {
    let at = args.iter().position(|s| s == "--")?;
    safe_segment(&args[at + 1..], depth + 1, wrappers).filter(|build| *build)
}

fn assignment(raw: &str) -> bool {
    raw.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
    })
}

#[cfg(test)]
#[path = "../../src/tests/provider_memory.rs"]
mod tests;
