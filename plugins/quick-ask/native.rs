//! Quick questions answered without opening a session.
//!
//! Ordinary questions use general chat, lookup requests use web research, and repository questions use the code
//! route. General and web default to Gemini Flash High through Antigravity in a separate folder; code defaults
//! to Claude Sonnet in the start directory. Prefixes choose a route or provider. Commands use argument lists,
//! read-only permissions and a time limit.
//!
//! A follow-up continues the program's own conversation, and `/clear` starts over; each program streams its
//! answer as JSON, read here as it is written, and a question can be stopped part way.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{AskConfig, AskRoute, Config};
use crate::run;

/// The longest question passed on, and the most of an answer kept.
pub const MAX_QUESTION: usize = 4000;
pub const MAX_ANSWER: usize = 64 * 1024;
/// At most this many questions are kept in the history.
pub const HISTORY_LEN: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    General,
    Web,
    Code,
}

impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Route::General => "general",
            Route::Web => "web",
            Route::Code => "code",
        }
    }
}

/// Words that make a question one about the code in front of the owner.
const CODE_HINTS: &[&str] = &[
    "this repo",
    "the repo",
    "our repo",
    "codebase",
    "our code",
    "this code",
    "in the code",
    "the deck",
    "cepheus",
    // How the owner still refers to it after the rename to Cepheus.
    "agent deck",
    "agent-deck", // legacy-name
    "which file",
    "what file",
    "where is",
    "where do we",
    "where does",
    "how do we",
    "in this project",
    "this project",
    "function",
    "struct ",
    "class ",
    "module",
    "stack trace",
    "traceback",
    "error[e",
    "compile error",
    "failing test",
    "test fails",
    "our test",
    "worktree",
    "branch",
    "commit",
    "runbook",
    "readme",
];
const CODE_EXTENSIONS: &[&str] =
    &[".rs", ".py", ".ts", ".tsx", ".js", ".kt", ".go", ".toml", ".json", ".yaml", ".yml", ".md", ".sh", ".lua"];

/// Positive lookup cues distinguish research from ordinary conversation.
const WEB_HINTS: &[&str] = &[
    "latest",
    "today",
    "tonight",
    "weather",
    "news",
    "current ",
    "currently",
    "search the web",
    "search online",
    "look up",
    "look online",
    "google ",
    "live score",
    "this week",
    "this month",
];

/// Repository context wins; web research needs a lookup cue. Other questions
/// stay in general chat instead of treating every non-code input as a search.
pub fn classify(question: &str, code_words: &[String]) -> Route {
    let lower = question.to_lowercase();
    let hinted = CODE_HINTS.iter().any(|h| lower.contains(h))
        || code_words.iter().any(|w| !w.is_empty() && lower.contains(&w.to_lowercase()));
    let names_file =
        lower.split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | ',' | '?')).any(|word| {
            CODE_EXTENSIONS.iter().any(|ext| word.ends_with(ext) && word.len() > ext.len()) || word.contains("src/")
        });
    if hinted || names_file {
        Route::Code
    } else if WEB_HINTS.iter().any(|hint| lower.contains(hint)) {
        Route::Web
    } else {
        Route::General
    }
}

/// Who answers a question: a route, and a program chosen by prefix.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub route: Route,
    /// `agy`, `claude` or `codex` named by a prefix; else the route's own.
    pub tool: Option<String>,
    pub text: String,
}

/// The prefixes that choose who answers.
const PREFIXES: &[&str] =
    &["/general", "/chat", "/web", "/google", "/search", "/code", "/gemini", "/agy", "/claude", "/codex"];

/// Read a question's prefix, if any, and decide who answers.
pub fn choose(input: &str, ask: &crate::config::AskConfig) -> Choice {
    let input = input.trim();
    let (word, rest) = match input.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (input, ""),
    };
    let pick =
        |route: Route, tool: Option<&str>| Choice { route, tool: tool.map(str::to_string), text: rest.to_string() };
    match word {
        "/general" | "/chat" => pick(Route::General, None),
        "/web" | "/google" | "/search" => pick(Route::Web, None),
        "/code" => pick(Route::Code, None),
        "/gemini" | "/agy" => pick(classify(rest, &ask.code_words), Some("agy")),
        "/claude" => pick(Route::Code, Some("claude")),
        "/codex" => pick(Route::Code, Some("codex")),
        _ => Choice { route: classify(input, &ask.code_words), tool: None, text: input.to_string() },
    }
}

/// What a line typed into the quick-question box is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typed<'a> {
    /// A question, perhaps after a prefix that chooses who answers.
    Question,
    /// `/clear` (or `/new`): start over, then ask what follows, if anything.
    Clear(&'a str),
    /// `/word` that is no command: never sent to a program as a question.
    Unknown(&'a str),
}

/// Tell a command from a question. A word after `/` with no other `/` is a
/// command, as in the agents' own boxes; `/usr/bin is what?` is a question.
pub fn typed(input: &str) -> Typed<'_> {
    let input = input.trim();
    let (word, rest) = match input.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (input, ""),
    };
    let Some(name) = word.strip_prefix('/') else { return Typed::Question };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Typed::Question;
    }
    match word {
        "/clear" | "/new" => Typed::Clear(rest),
        _ if PREFIXES.contains(&word) => Typed::Question,
        _ => Typed::Unknown(word),
    }
}

/// Why `word` was not asked.
pub fn unknown_command(word: &str) -> String {
    format!(
        "{word} is not a command: /clear starts over; /general, /web, /code, /agy, /claude or /codex chooses who answers"
    )
}

/// The route's settings, with the program a prefix chose.
pub fn route_settings(cfg: &AskConfig, choice: &Choice) -> AskRoute {
    let base = match choice.route {
        Route::General => cfg.general.clone(),
        Route::Web => cfg.web.clone(),
        Route::Code => cfg.code.clone(),
    };
    match &choice.tool {
        Some(tool) if *tool != base.tool => {
            // Another program than the route's: its own default model,
            // except Gemini Flash High for Antigravity.
            let model = if tool == "agy" { AskRoute::default().model } else { String::new() };
            AskRoute { tool: tool.clone(), model }
        }
        _ => base,
    }
}

/// The conversation a follow-up continues: who answers, in which folder,
/// and the program's own id for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub route: Route,
    pub tool: String,
    #[serde(default)]
    pub model: String,
    pub cwd: PathBuf,
    pub id: String,
}

/// One question, planned: who answers it, where, and the conversation it
/// continues (none: it starts one).
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub route: Route,
    pub settings: AskRoute,
    /// The question without its prefix.
    pub question: String,
    pub cwd: PathBuf,
    pub resume: Option<String>,
    /// The deck chose who answers: no prefix, and no route picked with Tab.
    pub automatic: bool,
}

impl Plan {
    /// "agy · gemini-3.8-flash-high (general)".
    pub fn who(&self) -> String {
        let who = if self.settings.model.is_empty() {
            self.settings.tool.clone()
        } else {
            format!("{} · {}", self.settings.tool, self.settings.model)
        };
        format!("{who} ({})", self.route.label())
    }

    pub fn argv(&self, config: &Config) -> Result<Vec<String>> {
        command(config, &self.settings, &self.question, self.resume.as_deref())
    }

    /// The conversation this question was asked in, its program's id `id`.
    pub fn thread(&self, id: String) -> Thread {
        Thread {
            route: self.route,
            tool: self.settings.tool.clone(),
            model: self.settings.model.clone(),
            cwd: self.cwd.clone(),
            id,
        }
    }

    /// The conversation it continues.
    pub fn continued(&self) -> Option<Thread> {
        self.resume.clone().map(|id| self.thread(id))
    }
}

/// Plan `input`. A prefix or `pick` (Tab) chooses who answers, else the deck
/// does. A follow-up continues `thread`, unless that choice names another
/// program, model or folder, which starts a new conversation.
pub fn plan(
    cfg: &AskConfig,
    input: &str,
    pick: Option<Route>,
    thread: Option<&Thread>,
    start_dir: &Path,
    state_dir: &Path,
) -> Result<Plan> {
    match typed(input) {
        Typed::Question => {}
        Typed::Clear(_) => bail!("/clear starts over; it is not a question"),
        Typed::Unknown(word) => bail!("{}", unknown_command(word)),
    }
    let mut choice = choose(input, cfg);
    let prefixed = choice.text != input.trim();
    if let (Some(route), false) = (pick, prefixed) {
        choice.route = route;
    }
    let explicit = prefixed || pick.is_some();
    let settings = route_settings(cfg, &choice);
    let cwd = workdir(choice.route, start_dir, state_dir);
    if let Some(thread) = thread {
        let same = thread.tool == settings.tool && thread.model == settings.model && thread.cwd == cwd;
        if !explicit || same {
            return Ok(Plan {
                route: if explicit { choice.route } else { thread.route },
                settings: AskRoute { tool: thread.tool.clone(), model: thread.model.clone() },
                question: choice.text,
                cwd: thread.cwd.clone(),
                resume: Some(thread.id.clone()),
                automatic: !explicit,
            });
        }
    }
    Ok(Plan { route: choice.route, settings, question: choice.text, cwd, resume: None, automatic: !explicit })
}

/// The program's first word from the deck's config, so a program kept
/// outside the PATH is found.
fn program(config: &Config, tool: &str) -> String {
    let configured = match tool {
        "claude" => config.claude_command.first(),
        "codex" => config.codex_command.first(),
        "agy" => config.antigravity_command.first(),
        _ => None,
    };
    configured.cloned().unwrap_or_else(|| tool.to_string())
}

/// A conversation id a program gave: letters, digits and dashes, so it can
/// never be read as an option.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('-')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The command that answers `question` with `route`, read only, printing
/// its answer as a JSON stream; `resume` continues that conversation.
pub fn command(config: &Config, route: &AskRoute, question: &str, resume: Option<&str>) -> Result<Vec<String>> {
    let question = question.trim();
    if question.is_empty() {
        bail!("type a question first");
    }
    if question.chars().count() > MAX_QUESTION {
        bail!("a quick question is at most {MAX_QUESTION} characters; open a session for more");
    }
    let model = route.model.trim();
    let valid_model = model.is_empty() || crate::service::valid_model_name(model);
    if !valid_model {
        bail!("{model:?} is not a model name");
    }
    if let Some(id) = resume.filter(|id| !valid_id(id)) {
        bail!("{id:?} is not a conversation id");
    }
    let mut argv = vec![program(config, &route.tool)];
    match route.tool.as_str() {
        "agy" => {
            if !model.is_empty() {
                argv.extend(["--model".into(), model.to_string()]);
            }
            if let Some(id) = resume {
                argv.extend(["--conversation".into(), id.to_string()]);
            }
            argv.extend([
                // AGY plan mode expands /plan into a planning workflow.
                // Ask directly; enforce read-only access through permissions.
                // https://antigravity.google/docs/permissions
                "--disable-slash-commands".into(),
                "--settings".into(),
                serde_json::json!({
                    "agentMode": "",
                    "toolPermission": "request-review",
                    "permissions": {
                        "allow": ["read_url(*)"],
                        "deny": ["write_file(*)", "command(*)", "execute_url(*)", "mcp(*)"]
                    }
                })
                .to_string(),
                "--output-format".into(),
                "stream-json".into(),
                "--print-timeout".into(),
                format!("{}s", config.ask.timeout.saturating_sub(5).max(5)),
                // Go's flag reads the question as `-p`'s value, so one that
                // starts with a dash stays a question.
                "-p".into(),
                question.to_string(),
            ]);
        }
        "claude" => {
            argv.extend([
                "-p".into(),
                "--output-format".into(),
                "stream-json".into(),
                // Print mode streams JSON only with --verbose; partial
                // messages are the text as it is written.
                "--verbose".into(),
                "--include-partial-messages".into(),
                "--permission-mode".into(),
                "plan".into(),
            ]);
            if !model.is_empty() {
                argv.extend(["--model".into(), model.to_string()]);
            }
            if let Some(id) = resume {
                argv.extend(["--resume".into(), id.to_string()]);
            }
            argv.extend(["--".into(), question.to_string()]);
        }
        "codex" => {
            // `exec resume` has no --sandbox; the same policy is its setting.
            match resume {
                Some(_) => argv.extend([
                    "exec".into(),
                    "resume".into(),
                    "--json".into(),
                    "-c".into(),
                    "sandbox_mode=\"read-only\"".into(),
                ]),
                None => argv.extend(["exec".into(), "--json".into(), "--sandbox".into(), "read-only".into()]),
            }
            argv.push("--skip-git-repo-check".into());
            if !model.is_empty() {
                argv.extend(["--model".into(), model.to_string()]);
            }
            if let Some(id) = resume {
                argv.push(id.to_string());
            }
            argv.extend(["--".into(), question.to_string()]);
        }
        other => bail!("{other:?} cannot answer quick questions (agy, claude or codex)"),
    }
    Ok(argv)
}

/// Code questions run in the start directory; general chat and web lookups
/// use a separate folder without the repository's agent instructions.
pub fn workdir(route: Route, start_dir: &Path, state_dir: &Path) -> PathBuf {
    match route {
        Route::Code => start_dir.to_path_buf(),
        Route::General | Route::Web => state_dir.join("ask"),
    }
}

/// What a program has said so far while it answers, read from its JSON
/// stream: the answer's text, what it is doing, its conversation id, and a
/// failure it reported.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Progress {
    pub text: String,
    /// The tool it is using, and on what: "search_web · rust release".
    pub activity: Option<String>,
    pub conversation: Option<String>,
    /// The answer as the program finally gave it; else `text`.
    pub result: Option<String>,
    pub error: Option<String>,
    /// The last line that was not JSON: a program's own complaint.
    pub other: Option<String>,
    /// The response step `text` belongs to (Antigravity), or that a new
    /// message began and its text replaces the last's (Claude).
    step: Option<i64>,
    fresh: bool,
}

/// `value` on one line, at most 80 characters.
fn brief(value: &str) -> String {
    let line = crate::util::one_line(value);
    match line.char_indices().nth(80) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// The most of an answer kept while it streams; `bounded` cuts the reply to
/// `MAX_ANSWER` once it ends. `run::stream` keeps none of the output itself.
const STREAM_CAP: usize = 2 * MAX_ANSWER;

impl Progress {
    /// Add `delta` to the answer, keeping at most `STREAM_CAP` bytes.
    fn append(&mut self, delta: &str) {
        let mut cut = delta.len().min(STREAM_CAP.saturating_sub(self.text.len()));
        while !delta.is_char_boundary(cut) {
            cut -= 1;
        }
        self.text.push_str(&delta[..cut]);
    }

    /// Read one line of `tool`'s stream; true when what it shows changed.
    pub fn read(&mut self, tool: &str, line: &str) -> bool {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            if !line.trim().is_empty() {
                self.other = Some(brief(line));
            }
            return false;
        };
        let before = (self.text.len(), self.activity.clone(), self.step);
        match tool {
            "agy" => self.agy(&value),
            "claude" => self.claude(&value),
            "codex" => self.codex(&value),
            _ => {}
        }
        before != (self.text.len(), self.activity.clone(), self.step)
    }

    /// The answer: what the program finally gave, else what it streamed.
    pub fn answer(&self) -> &str {
        self.result.as_deref().filter(|r| !r.trim().is_empty()).unwrap_or(&self.text)
    }

    fn agy(&mut self, value: &Value) {
        match value["event"].as_str() {
            Some("init") => self.conversation = text(&value["conversation_id"]).or(self.conversation.take()),
            Some("step_update") => {
                let step = &value["step_update"];
                if self.conversation.is_none() {
                    self.conversation = text(&step["conversation_id"]);
                }
                match step["step_type"].as_str() {
                    Some("agent_response") => {
                        if let Some(delta) = step["text_delta"].as_str() {
                            // A later response step is a new reply; the
                            // answer is the last one, as the result says.
                            let index = step["step_index"].as_i64();
                            if self.step != index {
                                self.text.clear();
                                self.step = index;
                            }
                            self.append(delta);
                        }
                        self.activity = None;
                    }
                    Some("tool") if step["state"] == "ACTIVE" => {
                        let name = step["tool_name"].as_str().unwrap_or("a tool");
                        let what = step["tool_info"]["parameters"]
                            .as_object()
                            .and_then(|params| params.values().find_map(Value::as_str))
                            .map(brief);
                        self.activity = Some(match what {
                            Some(what) => format!("{name} · {what}"),
                            None => name.to_string(),
                        });
                    }
                    Some("tool") => self.activity = None,
                    _ => {}
                }
            }
            Some("result") => {
                let result = &value["result"];
                if self.conversation.is_none() {
                    self.conversation = text(&result["conversation_id"]);
                }
                match result["status"].as_str() {
                    Some("SUCCESS") => self.result = text(&result["response"]),
                    status => {
                        self.error = text(&result["error"]["message"])
                            .or_else(|| text(&result["error"]))
                            .or_else(|| status.map(|s| format!("it ended with {s}")));
                    }
                }
            }
            Some("error") => {
                self.error = text(&value["error"]["message"])
                    .or_else(|| text(&value["message"]))
                    .or_else(|| text(&value["error"]));
            }
            _ => {}
        }
    }

    fn claude(&mut self, value: &Value) {
        if let Some(id) = value["session_id"].as_str() {
            self.conversation = Some(id.to_string());
        }
        match value["type"].as_str() {
            Some("stream_event") => {
                let event = &value["event"];
                match event["type"].as_str() {
                    Some("message_start") => self.fresh = true,
                    Some("content_block_start") => {
                        let block = &event["content_block"];
                        if block["type"] == "tool_use" {
                            self.activity = text(&block["name"]);
                        }
                    }
                    Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                        if self.fresh {
                            // After a tool, a new message: the answer is
                            // its text, as the result says.
                            self.text.clear();
                            self.fresh = false;
                            self.step = Some(self.step.map_or(0, |s| s + 1));
                        }
                        self.append(event["delta"]["text"].as_str().unwrap_or_default());
                        self.activity = None;
                    }
                    _ => {}
                }
            }
            Some("result") => {
                let result = text(&value["result"]);
                if value["is_error"] == true {
                    self.error =
                        result.filter(|r| !r.trim().is_empty()).or_else(|| text(&value["subtype"])).map(|e| brief(&e));
                } else {
                    self.result = result;
                }
            }
            _ => {}
        }
    }

    fn codex(&mut self, value: &Value) {
        let item = &value["item"];
        match value["type"].as_str() {
            Some("thread.started") => self.conversation = text(&value["thread_id"]),
            Some("item.started") => {
                self.activity = match item["type"].as_str() {
                    Some("command_execution") => text(&item["command"]).map(|c| brief(&c)),
                    Some("web_search") => Some(match item["query"].as_str() {
                        Some(query) => format!("web search · {}", brief(query)),
                        None => "web search".into(),
                    }),
                    Some(other) => Some(other.replace('_', " ")),
                    None => None,
                };
            }
            Some("item.completed") => {
                if item["type"] == "agent_message" {
                    // Each message replaces the last; the last is the answer.
                    self.text.clear();
                    self.append(item["text"].as_str().unwrap_or_default());
                    self.step = Some(self.step.map_or(0, |s| s + 1));
                }
                self.activity = None;
            }
            Some("turn.failed") => {
                self.error = text(&value["error"]["message"]).or_else(|| Some("the turn failed".into()));
            }
            Some("error") => self.error = text(&value["message"]),
            _ => {}
        }
    }
}

/// An answer: its text, the conversation it belongs to, and whether it was
/// stopped part way (`text` is what came by then).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub text: String,
    pub conversation: Option<String>,
    pub stopped: bool,
}

fn bounded(text: &str) -> String {
    let mut text = text.trim().to_string();
    if text.len() > MAX_ANSWER {
        let mut cut = MAX_ANSWER;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n…");
    }
    text
}

/// Ask as `plan` says, and wait for the answer; `progress` sees it as it is
/// written. Setting `stop` ends the program; what came by then is the reply.
pub fn answer(config: &Config, plan: &Plan, stop: &AtomicBool, progress: &mut dyn FnMut(&Progress)) -> Result<Reply> {
    let argv = plan.argv(config)?;
    let limit = Duration::from_secs(config.ask.timeout);
    stream_answer(&plan.settings.tool, &argv, &plan.cwd, limit, &config.scheduler.probe, stop, progress)
}

/// Run `argv`, `tool`'s command, and read its stream.
pub fn stream_answer(
    tool: &str,
    argv: &[String],
    cwd: &Path,
    limit: Duration,
    probe: &crate::scheduler::probe::Probe,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(&Progress),
) -> Result<Reply> {
    let mut said = Progress::default();
    if stop.load(Ordering::SeqCst) {
        // Stopped before it started.
        return Ok(Reply { text: String::new(), conversation: None, stopped: true });
    }
    anyhow::ensure!(probe.read().admits(), "Start held: memory or CPU headroom is short");
    std::fs::create_dir_all(cwd)?;
    let ran = run::stream(argv, Some(cwd), &crate::client_temp::vars("ask")?, limit, stop, &mut |line| {
        if said.read(tool, line) {
            progress(&said);
        }
    })?;
    let text = bounded(said.answer());
    if stop.load(Ordering::SeqCst) {
        return Ok(Reply { text, conversation: said.conversation, stopped: true });
    }
    let name = argv.first().map(String::as_str).unwrap_or("it");
    if let Some(error) = &said.error {
        bail!("{name} could not answer: {error}");
    }
    if !ran.ok() {
        let reason = match (&said.other, ran.timed_out || !ran.stderr.trim().is_empty()) {
            (Some(other), false) => other.clone(),
            _ => ran.reason(),
        };
        bail!("{name} could not answer: {reason}");
    }
    if text.is_empty() {
        bail!("the answer was empty");
    }
    Ok(Reply { text, conversation: said.conversation, stopped: false })
}

/// Plan and answer one question that continues no conversation: the
/// phone's and the shell's questions. A question that cannot be asked is
/// refused; a program's failure is the answer, marked.
pub fn ask_once(config: &Config, text: &str, start_dir: &Path, state_dir: &Path) -> Result<Asked> {
    let plan = plan(&config.ask, text, None, None, start_dir, state_dir)?;
    plan.argv(config)?;
    let reply = answer(config, &plan, &AtomicBool::new(false), &mut |_| {});
    Ok(Asked {
        question: text.to_string(),
        who: format!("{} {}", plan.settings.tool, plan.settings.model).trim().to_string(),
        answer: reply.as_ref().map_or_else(|e| format!("{e:#}"), |r| r.text.clone()),
        error: reply.is_err(),
        at: crate::util::now_secs(),
        ..Asked::default()
    })
}

/// A question asked and its answer, for the history.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Asked {
    pub question: String,
    pub who: String,
    pub answer: String,
    pub error: bool,
    pub at: u64,
    /// The conversation it was asked in, when its program named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<Thread>,
    /// Stopped part way (Ctrl-c): `answer` is what came by then.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stopped: bool,
}

pub fn history_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ask-history.json")
}

pub fn load_history(state_dir: &Path) -> Vec<Asked> {
    let path = history_path(state_dir);
    match std::fs::metadata(&path) {
        Ok(m) if m.len() <= 4 * 1024 * 1024 => {
            std::fs::read(&path).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
pub fn save_history(state_dir: &Path, history: &[Asked]) {
    let start = history.len().saturating_sub(HISTORY_LEN);
    if let Ok(data) = serde_json::to_vec(&history[start..]) {
        let _ = crate::util::write_atomic(&history_path(state_dir), &data);
    }
}

/// The history's process-shared lock, held until the file is dropped.
/// Desktop and phone answers must not replace each other's newer history.
fn lock_history(state_dir: &Path) -> Result<std::fs::File> {
    use anyhow::Context;
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::Instant;

    crate::util::ensure_private_dir(state_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(state_dir.join("ask-history.lock"))?;
    if !lock.metadata()?.is_file() {
        bail!("The question history lock is not a regular file.");
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // The lock stays held until this descriptor is dropped, including on error.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(lock);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock || Instant::now() >= deadline {
            return Err(error).context("Could not lock question history");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Append against the current file while holding the history's lock.
pub fn append_history(state_dir: &Path, asked: &Asked) -> Result<Vec<Asked>> {
    use anyhow::Context;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let _lock = lock_history(state_dir)?;
    let path = history_path(state_dir);
    let mut history: Vec<Asked> =
        match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path) {
            Ok(file) => {
                const MAX: u64 = 4 * 1024 * 1024;
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.len() > MAX {
                    bail!("Question history is not a bounded regular file.");
                }
                let mut bytes = Vec::new();
                file.take(MAX + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX {
                    bail!("Question history is too large.");
                }
                serde_json::from_slice(&bytes).context("Question history is invalid; preserved the existing file")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error).context("Could not read question history"),
        };
    history.push(asked.clone());
    let start = history.len().saturating_sub(HISTORY_LEN);
    history.drain(..start);
    let bytes = serde_json::to_vec(&history)?;
    if bytes.len() > 4 * 1024 * 1024 {
        bail!("Question history is too large.");
    }
    crate::util::write_atomic(&history_path(state_dir), &bytes)?;
    Ok(history)
}

/// `/clear`: the history is emptied for every deck and the phone.
pub fn clear_history(state_dir: &Path) -> Result<()> {
    let _lock = lock_history(state_dir)?;
    let path = history_path(state_dir);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
        bail!("Question history is not a regular file.");
    }
    crate::util::write_atomic(&path, b"[]")?;
    Ok(())
}

/// `cepheus ask <question>`: answered here, as F4 answers a first question.
pub fn run_cli(args: &[String]) -> i32 {
    let input = args.join(" ");
    let config = Config::load().0;
    let start_dir = std::env::current_dir().unwrap_or_else(|_| crate::paths::home());
    match ask_once(&config, &input, &start_dir, &crate::paths::state_dir()) {
        Ok(asked) if !asked.error => {
            println!("{}", asked.answer);
            0
        }
        Ok(asked) => {
            eprintln!("cepheus ask: {}", asked.answer);
            1
        }
        Err(err) => {
            eprintln!("cepheus ask: {err:#}");
            1
        }
    }
}

#[cfg(test)]
#[path = "../../src/tests/ask.rs"]
mod tests;
