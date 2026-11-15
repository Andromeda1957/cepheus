//! Installing a plugin, MCP server or skill for the agents.
//!
//! A source is a GitHub repository (`owner/repo`, `gh:owner/repo` or its URL), another git URL, a local folder,
//! an npm package that is an MCP server (`npm:<package>`) or an MCP server given whole (`mcp:<name>=<command
//! and arguments, or URL>`). A repository is fetched into the deck's sources folder and read, never run: what
//! it holds is installed through each agent's own command line, so every agent gets the form it understands:
//!
//! - a Claude Code marketplace or plugin: `claude plugin` for Claude, and
//!   `agy plugin install` of the plugin folder for Antigravity;
//! - a Codex marketplace or plugin: `codex plugin` for Codex;
//! - otherwise, and for an agent the plugin is not made for, its MCP servers
//!   (`.mcp.json`) through `claude mcp`, `codex mcp` and `agy mcp`, and its
//!   skills (`SKILL.md` folders) linked for each agent (`skills.rs`).
//!
//! Every command is an argument list, never a shell line, with a time limit. Names taken from the source are
//! reduced to ordinary characters and never start with a dash. What was installed is recorded so `remove`
//! undoes it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::protocol::InstallRequest;
use crate::{run, skills};

/// The agents the installer installs for, by the names requests use.
pub const AGENTS: [&str; 3] = ["claude", "codex", "agy"];
/// How long one agent command, or fetching a repository, may take.
const STEP_LIMIT: Duration = Duration::from_secs(180);
/// The largest manifest read from a source.
const JSON_LIMIT: u64 = 1024 * 1024;
/// The deck's own marketplace, which carries a lone plugin to Claude and
/// Codex (each installs plugins only from a marketplace). Its earlier name
/// stays: agents registered it, and installed its plugins, under that name.
pub const DECK_MARKETPLACE: &str = "agent-deck"; // legacy-name

/// What an install did, line by line, for the deck that asked.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub ok: bool,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum McpServer {
    Stdio { command: String, args: Vec<String>, env: Vec<(String, String)> },
    Http { url: String, headers: Vec<(String, String)> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Git { url: String, name: String },
    Dir { path: PathBuf, name: String },
    Mcp { name: String, server: McpServer },
}

impl Source {
    pub fn name(&self) -> &str {
        match self {
            Source::Git { name, .. } | Source::Dir { name, .. } | Source::Mcp { name, .. } => name,
        }
    }
}

/// A name from a source as the deck uses it: ordinary characters, at most
/// 64, never starting with a dot or dash.
pub fn clean_name(raw: &str) -> String {
    let name: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .take(64)
        .collect();
    let name = name.trim_start_matches(['.', '-']).trim_end_matches(['.', '-']).to_string();
    if name.is_empty() {
        "source".to_string()
    } else {
        name
    }
}

fn plain_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(['.', '-'])
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Read what the owner typed as a source.
pub fn parse_source(text: &str, home: &Path) -> Result<Source> {
    let text = text.trim();
    if text.is_empty() {
        bail!("give a GitHub repository, git URL, folder, npm:<package> or mcp:<name>=<command or URL>");
    }
    if text.len() > 500 || text.chars().any(char::is_control) {
        bail!("that source is too long or holds control characters");
    }
    if let Some(rest) = text.strip_prefix("mcp:") {
        let (name, spec) = rest.split_once('=').context("write it as mcp:<name>=<command and arguments, or URL>")?;
        let name = name.trim();
        if !plain_segment(name) {
            bail!("{name:?} is not a server name the deck takes");
        }
        let spec = spec.trim();
        let server = if spec.starts_with("https://")
            || spec.starts_with("http://127.0.0.1")
            || spec.starts_with("http://localhost")
        {
            if !web_url(spec) {
                bail!("a server's URL holds no spaces");
            }
            McpServer::Http { url: spec.to_string(), headers: Vec::new() }
        } else {
            let mut words = spec.split_whitespace().map(str::to_string);
            let command = words.next().context("the server needs a command")?;
            if command.starts_with('-') {
                bail!("the server's command cannot start with a dash");
            }
            McpServer::Stdio { command, args: words.collect(), env: Vec::new() }
        };
        return Ok(Source::Mcp { name: name.to_string(), server });
    }
    if let Some(package) = text.strip_prefix("npm:") {
        let package = package.trim();
        let ok = !package.is_empty()
            && !package.starts_with(['-', '.'])
            && package.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '/' | '.' | '_' | '-'));
        if !ok {
            bail!("{package:?} is not an npm package name");
        }
        let last = package.rsplit('/').next().unwrap_or(package);
        let bare = last.split('@').next().filter(|s| !s.is_empty()).unwrap_or(last);
        let name = clean_name(bare.trim_start_matches("mcp-server-").trim_start_matches("server-"));
        let server = McpServer::Stdio {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), package.to_string()],
            env: Vec::new(),
        };
        return Ok(Source::Mcp { name, server });
    }
    if let Some(path) = text.strip_prefix("~/").map(|p| home.join(p)).or_else(|| {
        (text.starts_with('/') || text.starts_with("./") || text.starts_with("../")).then(|| PathBuf::from(text))
    }) {
        let path = path.canonicalize().with_context(|| format!("{text} is not a folder here"))?;
        if !path.is_dir() {
            bail!("{} is not a folder", path.display());
        }
        let name = clean_name(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        return Ok(Source::Dir { path, name });
    }
    let github = |owner: &str, repo: &str| -> Result<Source> {
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        if !plain_segment(owner) || !plain_segment(repo) {
            bail!("{owner}/{repo} is not a GitHub repository name");
        }
        Ok(Source::Git { url: format!("https://github.com/{owner}/{repo}.git"), name: clean_name(repo) })
    };
    if let Some(rest) = text.strip_prefix("gh:").or_else(|| text.strip_prefix("github:")) {
        let (owner, repo) = rest.split_once('/').context("write it as gh:owner/repo")?;
        return github(owner, repo);
    }
    if let Some(rest) = text.strip_prefix("https://") {
        if text.chars().any(char::is_whitespace) {
            bail!("a URL holds no spaces");
        }
        let mut parts = rest.split('/').filter(|p| !p.is_empty());
        let host = parts.next().unwrap_or_default();
        if host.eq_ignore_ascii_case("github.com") {
            let owner = parts.next().unwrap_or_default();
            let repo = parts.next().context("the URL names no repository")?;
            return github(owner, repo);
        }
        if host.is_empty() || host.starts_with('-') {
            bail!("{text} names no host");
        }
        let last = rest.trim_end_matches('/').rsplit('/').next().unwrap_or(rest);
        let name = clean_name(last.strip_suffix(".git").unwrap_or(last));
        return Ok(Source::Git { url: text.trim_end_matches('/').to_string(), name });
    }
    if let Some(rest) = text.strip_prefix("git@") {
        let (host, path) = rest.split_once(':').context("write an SSH source as git@host:owner/repo.git")?;
        if !plain_segment(host) || path.starts_with('-') || path.chars().any(char::is_whitespace) {
            bail!("{text} is not a git address the deck takes");
        }
        let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
        return Ok(Source::Git { url: text.to_string(), name: clean_name(last.strip_suffix(".git").unwrap_or(last)) });
    }
    if let Some((owner, repo)) = text.split_once('/') {
        if !repo.contains('/') {
            return github(owner, repo);
        }
    }
    bail!("{text:?} is not a source the deck knows: give owner/repo, a git URL, a folder, npm:<package> or mcp:<name>=<command>")
}

/// A marketplace's name and its plugins: (name, plugin folder in the
/// source, when the plugin is kept there).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Market {
    pub name: String,
    pub plugins: Vec<(String, Option<PathBuf>)>,
}

/// What a fetched source holds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    pub claude_marketplace: Option<Market>,
    pub claude_plugin: Option<String>,
    pub codex_marketplace: Option<Market>,
    pub codex_plugin: Option<String>,
    pub mcp: Vec<(String, McpServer)>,
    pub skills: Vec<(String, PathBuf)>,
}

impl Found {
    pub fn is_empty(&self) -> bool {
        self.claude_marketplace.is_none()
            && self.claude_plugin.is_none()
            && self.codex_marketplace.is_none()
            && self.codex_plugin.is_none()
            && self.mcp.is_empty()
            && self.skills.is_empty()
    }
}

fn read_json(path: &Path) -> Option<Value> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > JSON_LIMIT {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Whether `path` is a plain file that stays inside `root`, through every
/// folder on the way: a fetched repository may hold links that lead out.
fn file_inside(root: &Path, path: &Path) -> bool {
    let plain = fs::symlink_metadata(path).is_ok_and(|m| m.is_file());
    plain
        && match (path.canonicalize(), root.canonicalize()) {
            (Ok(real), Ok(root)) => real.starts_with(root),
            _ => false,
        }
}

/// A manifest inside a fetched source.
fn read_json_in(root: &Path, path: &Path) -> Option<Value> {
    file_inside(root, path).then(|| read_json(path)).flatten()
}

/// A URL an MCP server is reached at: the web, with nothing an agent's
/// command line could take for a flag.
fn web_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() <= 2000
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn text_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// A folder `rel` names inside `root`, refusing anything that leaves it.
fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.trim_start_matches("./");
    if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|p| p == "..") {
        return None;
    }
    let path = root.join(rel);
    let real = path.canonicalize().ok()?;
    (real.starts_with(root.canonicalize().ok()?) && real.is_dir()).then_some(real)
}

fn market(root: &Path, file: &Value) -> Option<Market> {
    let name = clean_name(&text_of(file, "name")?);
    let plugins = file
        .get("plugins")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|p| {
                    let name = clean_name(&text_of(p, "name")?);
                    let source = p.get("source");
                    let rel = source
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| source.and_then(|s| text_of(s, "path")));
                    Some((name, rel.and_then(|r| inside(root, &r))))
                })
                .take(100)
                .collect()
        })
        .unwrap_or_default();
    Some(Market { name, plugins })
}

fn mcp_servers(file: &Value) -> Vec<(String, McpServer)> {
    let Some(servers) = file.get("mcpServers").and_then(Value::as_object) else { return Vec::new() };
    let pairs = |v: Option<&Value>| -> Vec<(String, String)> {
        v.and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).take(50).collect())
            .unwrap_or_default()
    };
    servers
        .iter()
        .take(50)
        .filter_map(|(name, spec)| {
            let name = clean_name(name);
            if let Some(url) = text_of(spec, "url") {
                return web_url(&url).then(|| (name, McpServer::Http { url, headers: pairs(spec.get("headers")) }));
            }
            let command = text_of(spec, "command").filter(|c| !c.starts_with('-'))?;
            let args = spec
                .get("args")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            Some((name, McpServer::Stdio { command, args, env: pairs(spec.get("env")) }))
        })
        .collect()
}

fn skill_name(dir: &Path) -> String {
    let text = fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
    let front = text.strip_prefix("---").and_then(|r| r.split_once("\n---")).map(|(f, _)| f.to_string());
    let named = front
        .and_then(|f| f.lines().find_map(|l| l.strip_prefix("name:").map(|n| n.trim().trim_matches('"').to_string())));
    clean_name(
        &named
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
    )
}

fn skill_dirs(root: &Path, parent: &Path, out: &mut Vec<(String, PathBuf)>) {
    // `skills` itself may be a link out of the source.
    let real_dir = fs::symlink_metadata(parent).is_ok_and(|m| m.is_dir());
    let inside = matches!((parent.canonicalize(), root.canonicalize()), (Ok(p), Ok(r)) if p.starts_with(&r));
    if !real_dir || !inside {
        return;
    }
    let Ok(entries) = fs::read_dir(parent) else { return };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| file_inside(root, &p.join("SKILL.md")))
        .collect();
    dirs.sort();
    for dir in dirs.into_iter().take(100) {
        let name = skill_name(&dir);
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, dir));
        }
    }
}

/// Read what a fetched source holds. Only manifests and `SKILL.md` files
/// are read, each bounded.
pub fn detect(root: &Path) -> Found {
    let mut found = Found::default();
    if let Some(file) = read_json_in(root, &root.join(".claude-plugin/marketplace.json")) {
        found.claude_marketplace = market(root, &file);
    }
    if let Some(file) = read_json_in(root, &root.join(".claude-plugin/plugin.json")) {
        found.claude_plugin = text_of(&file, "name").map(|n| clean_name(&n));
    }
    if let Some(file) = read_json_in(root, &root.join(".agents/plugins/marketplace.json")) {
        found.codex_marketplace = market(root, &file);
    }
    if let Some(file) = read_json_in(root, &root.join(".codex-plugin/plugin.json")) {
        found.codex_plugin = text_of(&file, "name").map(|n| clean_name(&n));
    }
    let mut plugin_dirs = vec![root.to_path_buf()];
    for m in [&found.claude_marketplace, &found.codex_marketplace].into_iter().flatten() {
        plugin_dirs.extend(m.plugins.iter().filter_map(|(_, dir)| dir.clone()));
    }
    plugin_dirs.dedup();
    for dir in &plugin_dirs {
        if let Some(file) = read_json_in(root, &dir.join(".mcp.json")) {
            for (name, server) in mcp_servers(&file) {
                if !found.mcp.iter().any(|(n, _)| *n == name) {
                    found.mcp.push((name, server));
                }
            }
        }
    }
    if file_inside(root, &root.join("SKILL.md")) {
        found.skills.push((skill_name(root), root.to_path_buf()));
    }
    for dir in &plugin_dirs {
        skill_dirs(root, &dir.join("skills"), &mut found.skills);
    }
    found
}

/// One thing to do for an agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Run an agent's command; `record` is what to note for removing it.
    Run { agent: &'static str, what: String, argv: Vec<String>, record: Option<(String, String)> },
    /// Add a lone plugin to the deck's own marketplace first.
    DeckMarket { plugin: String, dir: PathBuf },
    /// Copy a skill into the deck's store and link it for `agents`.
    Skill { name: String, dir: PathBuf, agents: Vec<String> },
}

impl Step {
    pub fn describe(&self) -> String {
        match self {
            Step::Run { agent, what, argv, .. } => format!("{agent}: {what} ({})", argv.join(" ")),
            Step::DeckMarket { plugin, .. } => format!("add {plugin} to the deck's own marketplace"),
            Step::Skill { name, agents, .. } => format!("skill {name} for {}", agents.join(", ")),
        }
    }
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn mcp_steps(agent: &'static str, name: &str, server: &McpServer) -> Step {
    let record = Some((format!("{agent}-mcp"), name.to_string()));
    let what = format!("add MCP server {name}");
    let argv = match (agent, server) {
        ("claude", server) => {
            let spec = match server {
                McpServer::Stdio { command, args, env } => {
                    let env: BTreeMap<_, _> = env.iter().cloned().collect();
                    json!({"type": "stdio", "command": command, "args": args, "env": env})
                }
                McpServer::Http { url, headers } => {
                    let headers: BTreeMap<_, _> = headers.iter().cloned().collect();
                    json!({"type": "http", "url": url, "headers": headers})
                }
            };
            vec![
                "claude".into(),
                "mcp".into(),
                "add-json".into(),
                "-s".into(),
                "user".into(),
                name.into(),
                spec.to_string(),
            ]
        }
        ("codex", McpServer::Stdio { command, args, env }) => {
            let mut v = argv(&["codex", "mcp", "add"]);
            for (k, val) in env {
                v.push("--env".into());
                v.push(format!("{k}={val}"));
            }
            v.push(name.into());
            v.push("--".into());
            v.push(command.clone());
            v.extend(args.iter().cloned());
            v
        }
        ("codex", McpServer::Http { url, .. }) => {
            vec!["codex".into(), "mcp".into(), "add".into(), name.into(), "--url".into(), url.clone()]
        }
        (_, McpServer::Stdio { command, args, env }) => {
            let mut v = argv(&["agy", "mcp", "add"]);
            for (k, val) in env {
                v.push("--env".into());
                v.push(format!("{k}={val}"));
            }
            v.push(name.into());
            v.push("--".into());
            v.push(command.clone());
            v.extend(args.iter().cloned());
            v
        }
        (_, McpServer::Http { url, headers }) => {
            let mut v = argv(&["agy", "mcp", "add"]);
            for (k, val) in headers {
                v.push("--header".into());
                v.push(format!("{k}: {val}"));
            }
            v.push(name.into());
            v.push(url.clone());
            v
        }
    };
    Step::Run { agent, what, argv, record }
}

/// Where a marketplace is added from: the repository's own address, so
/// the agent keeps it current, or the folder.
fn market_source(source: &Source, root: &Path) -> String {
    match source {
        Source::Git { url, .. } => match url.strip_prefix("https://github.com/") {
            Some(rest) => rest.trim_end_matches(".git").to_string(),
            None => url.clone(),
        },
        _ => root.to_string_lossy().into_owned(),
    }
}

/// What installing `found` for `agents` takes, in order.
pub fn plan(source: &Source, found: &Found, root: Option<&Path>, agents: &[&'static str]) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut skill_agents: Vec<String> = Vec::new();
    for &agent in agents {
        let mut fallback = true;
        match (agent, root) {
            ("claude", Some(root)) => {
                if let Some(m) = &found.claude_marketplace {
                    let from = market_source(source, root);
                    steps.push(Step::Run {
                        agent,
                        what: format!("add marketplace {}", m.name),
                        argv: vec!["claude".into(), "plugin".into(), "marketplace".into(), "add".into(), from],
                        record: Some(("claude-marketplace".into(), m.name.clone())),
                    });
                    for (plugin, _) in &m.plugins {
                        let id = format!("{plugin}@{}", m.name);
                        steps.push(Step::Run {
                            agent,
                            what: format!("install plugin {id}"),
                            argv: argv(&["claude", "plugin", "install", "-s", "user", &id]),
                            record: Some(("claude-plugin".into(), id)),
                        });
                    }
                    fallback = false;
                } else if let Some(plugin) = &found.claude_plugin {
                    steps.push(Step::DeckMarket { plugin: plugin.clone(), dir: root.to_path_buf() });
                    let id = format!("{plugin}@{DECK_MARKETPLACE}");
                    steps.push(Step::Run {
                        agent,
                        what: format!("install plugin {id}"),
                        argv: argv(&["claude", "plugin", "install", "-s", "user", &id]),
                        record: Some(("claude-plugin".into(), id)),
                    });
                    fallback = false;
                }
            }
            ("codex", Some(root)) => {
                if let Some(m) = &found.codex_marketplace {
                    let from = market_source(source, root);
                    steps.push(Step::Run {
                        agent,
                        what: format!("add marketplace {}", m.name),
                        argv: vec!["codex".into(), "plugin".into(), "marketplace".into(), "add".into(), from],
                        record: Some(("codex-marketplace".into(), m.name.clone())),
                    });
                    for (plugin, _) in &m.plugins {
                        let id = format!("{plugin}@{}", m.name);
                        steps.push(Step::Run {
                            agent,
                            what: format!("install plugin {id}"),
                            argv: argv(&["codex", "plugin", "add", &id]),
                            record: Some(("codex-plugin".into(), id)),
                        });
                    }
                    fallback = false;
                } else if let Some(plugin) = &found.codex_plugin {
                    steps.push(Step::DeckMarket { plugin: plugin.clone(), dir: root.to_path_buf() });
                    let id = format!("{plugin}@{DECK_MARKETPLACE}");
                    steps.push(Step::Run {
                        agent,
                        what: format!("install plugin {id}"),
                        argv: argv(&["codex", "plugin", "add", &id]),
                        record: Some(("codex-plugin".into(), id)),
                    });
                    fallback = false;
                }
            }
            ("agy", Some(root)) => {
                // Antigravity installs a plugin from its folder.
                let mut dirs: Vec<(String, PathBuf)> = Vec::new();
                if let Some(plugin) = found.claude_plugin.as_ref().or(found.codex_plugin.as_ref()) {
                    dirs.push((plugin.clone(), root.to_path_buf()));
                }
                for m in [&found.claude_marketplace, &found.codex_marketplace].into_iter().flatten() {
                    for (plugin, dir) in &m.plugins {
                        if let Some(dir) = dir {
                            if !dirs.iter().any(|(_, d)| d == dir) {
                                dirs.push((plugin.clone(), dir.clone()));
                            }
                        }
                    }
                }
                for (plugin, dir) in &dirs {
                    steps.push(Step::Run {
                        agent,
                        what: format!("install plugin {plugin}"),
                        argv: vec!["agy".into(), "plugin".into(), "install".into(), dir.to_string_lossy().into_owned()],
                        record: Some(("agy-plugin".into(), plugin.clone())),
                    });
                }
                fallback = dirs.is_empty();
            }
            _ => {}
        }
        if fallback {
            let servers: Vec<(String, McpServer)> = match source {
                Source::Mcp { name, server } => vec![(name.clone(), server.clone())],
                _ => found.mcp.clone(),
            };
            for (name, server) in &servers {
                steps.push(mcp_steps(agent, name, server));
            }
            if !found.skills.is_empty() {
                skill_agents.push(agent.to_string());
            }
        }
    }
    if !skill_agents.is_empty() {
        for (name, dir) in &found.skills {
            steps.push(Step::Skill { name: name.clone(), dir: dir.clone(), agents: skill_agents.clone() });
        }
    }
    steps
}

/// The installs the deck made, for `remove`: by source name, what each
/// agent was given.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub source: String,
    /// (kind, id): `claude-plugin`, `codex-mcp`, `skill` and so on.
    pub installed: Vec<(String, String)>,
}

pub fn records_path(data: &Path) -> PathBuf {
    data.join("installed.json")
}

pub fn read_records(data: &Path) -> BTreeMap<String, Record> {
    read_json(&records_path(data)).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

fn write_records(data: &Path, records: &BTreeMap<String, Record>) -> Result<()> {
    crate::util::write_atomic(&records_path(data), &serde_json::to_vec_pretty(records)?)
        .context("saving installed.json")
}

/// Change the receipts as they are now, under the install records' lock
/// (`skills::lock`): a copy read when an install began would drop the
/// receipts of installs that finished meanwhile (audit OPS-S11601).
fn update_records(data: &Path, change: impl FnOnce(&mut BTreeMap<String, Record>)) -> Result<()> {
    let _lock = skills::lock(data)?;
    let mut records = read_records(data);
    change(&mut records);
    write_records(data, &records)
}

/// Fetch a git source into `<data>/sources/<name>`, replacing an earlier
/// copy. Nothing in it is run; submodules are not fetched.
pub fn fetch(url: &str, name: &str, data: &Path) -> Result<PathBuf> {
    let sources = data.join("sources");
    fs::create_dir_all(&sources)?;
    let dest = sources.join(name);
    let staging = sources.join(format!(".{name}.new"));
    let _ = fs::remove_dir_all(&staging);
    let argv = vec![
        "git".to_string(),
        "-c".into(),
        "protocol.ext.allow=never".into(),
        "-c".into(),
        "protocol.file.allow=never".into(),
        "clone".into(),
        "--depth".into(),
        "1".into(),
        "--quiet".into(),
        "--".into(),
        url.to_string(),
        staging.to_string_lossy().into_owned(),
    ];
    let env = [("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())];
    let ran = run::run(&argv, None, &env, STEP_LIMIT)?;
    if !ran.ok() {
        let _ = fs::remove_dir_all(&staging);
        bail!("could not fetch {url}: {}", ran.reason());
    }
    if dest.exists() {
        fs::remove_dir_all(&dest).with_context(|| format!("replacing {}", dest.display()))?;
    }
    fs::rename(&staging, &dest)?;
    Ok(dest)
}

/// Add `plugin`, kept in `dir`, to the deck's own marketplace for Claude
/// and Codex, whose entries point at a link to the folder.
fn add_to_deck_market(data: &Path, plugin: &str, dir: &Path) -> Result<PathBuf> {
    // Both marketplace files list every plugin: read and replace them
    // under the install records' lock, as `update_records` does.
    let _lock = skills::lock(data)?;
    let root = data.join("marketplace");
    let plugins = root.join("plugins");
    fs::create_dir_all(&plugins)?;
    fs::create_dir_all(root.join(".claude-plugin"))?;
    fs::create_dir_all(root.join(".agents/plugins"))?;
    let link = plugins.join(plugin);
    if fs::symlink_metadata(&link).is_ok() {
        fs::remove_file(&link).with_context(|| format!("replacing {}", link.display()))?;
    }
    std::os::unix::fs::symlink(dir, &link)?;
    let rel = format!("./plugins/{plugin}");
    let claude = root.join(".claude-plugin/marketplace.json");
    let mut file = read_json(&claude)
        .unwrap_or_else(|| json!({"name": DECK_MARKETPLACE, "owner": {"name": "cepheus"}, "plugins": []}));
    upsert(&mut file, plugin, json!({"name": plugin, "source": rel}));
    crate::util::write_atomic(&claude, &serde_json::to_vec_pretty(&file)?)?;
    let codex = root.join(".agents/plugins/marketplace.json");
    let mut file = read_json(&codex)
        .unwrap_or_else(|| json!({"name": DECK_MARKETPLACE, "interface": {"displayName": "Cepheus"}, "plugins": []}));
    upsert(
        &mut file,
        plugin,
        json!({"name": plugin, "source": {"source": "local", "path": rel},
               "policy": {"installation": "AVAILABLE", "authentication": "ON_INSTALL"}}),
    );
    crate::util::write_atomic(&codex, &serde_json::to_vec_pretty(&file)?)?;
    Ok(root)
}

fn upsert(file: &mut Value, plugin: &str, entry: Value) {
    if !file.get("plugins").is_some_and(Value::is_array) {
        file["plugins"] = json!([]);
    }
    if let Some(list) = file.get_mut("plugins").and_then(Value::as_array_mut) {
        list.retain(|p| p.get("name").and_then(Value::as_str) != Some(plugin));
        list.push(entry);
    }
}

/// Whether `program` is on the PATH.
pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let path = dir.join(program);
            fs::metadata(&path)
                .is_ok_and(|m| m.is_file() && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0)
        })
    })
}

/// The agents a request is for that this host has.
pub fn agents_for(request: &[String], home: &Path) -> (Vec<&'static str>, Vec<String>) {
    let mut chosen = Vec::new();
    let mut missing = Vec::new();
    for agent in AGENTS {
        let asked = request.is_empty() || request.iter().any(|a| a == agent || (agent == "agy" && a == "antigravity"));
        if !asked {
            continue;
        }
        // A skill needs only the agent's folder; the rest its command.
        let marker = skills::AGENTS.iter().find(|(name, ..)| *name == agent).map(|(_, _, m)| home.join(m));
        if on_path(agent) || marker.is_some_and(|m| m.is_dir()) {
            chosen.push(agent);
        } else if !request.is_empty() {
            missing.push(format!("{agent} is not installed here"));
        }
    }
    (chosen, missing)
}

/// Install (or with `request.remove`, remove) what a source holds, for the
/// agents asked for that this host has.
pub fn install(request: &InstallRequest, home: &Path) -> Report {
    install_in(request, home, &crate::paths::data_dir())
}

pub fn install_in(request: &InstallRequest, home: &Path, data: &Path) -> Report {
    let mut report = Report { ok: true, lines: Vec::new() };
    if let Err(err) = crate::util::ensure_private_dir(data) {
        return Report { ok: false, lines: vec![format!("could not make {}: {err}", data.display())] };
    }
    let source = match parse_source(&request.source, home) {
        Ok(source) => source,
        Err(err) => return Report { ok: false, lines: vec![format!("{err:#}")] },
    };
    if request.remove {
        // A dry run is the preview `:uninstall` shows before asking: it
        // removes nothing (audit ROOT-036).
        return if request.dry_run { removal_preview(&source, data) } else { remove(&source, home, data) };
    }
    let (agents, missing) = agents_for(&request.agents, home);
    report.lines.extend(missing);
    if agents.is_empty() {
        report.ok = false;
        report.lines.push("none of the agents asked for is installed here".to_string());
        return report;
    }
    let root = match &source {
        Source::Git { url, name } => {
            if request.dry_run {
                report.lines.push(format!("fetching {url} to look inside"));
            }
            match fetch(url, name, data) {
                Ok(dir) => Some(dir),
                Err(err) => return Report { ok: false, lines: vec![format!("{err:#}")] },
            }
        }
        Source::Dir { path, .. } => Some(path.clone()),
        Source::Mcp { .. } => None,
    };
    let found = root.as_deref().map(detect).unwrap_or_default();
    if root.is_some() && found.is_empty() {
        return Report {
            ok: false,
            lines: vec![format!(
                "{} holds no plugin, marketplace, .mcp.json or SKILL.md the deck knows how to install",
                source.name()
            )],
        };
    }
    let steps = plan(&source, &found, root.as_deref(), &agents);
    if request.dry_run {
        report.lines.push(format!("{} would install:", source.name()));
        report.lines.extend(steps.iter().map(|s| format!("  {}", s.describe())));
        return report;
    }
    // What this install added, recorded when it is done (`update_records`).
    let mut added: Vec<(String, String)> = Vec::new();
    for step in &steps {
        match step {
            Step::DeckMarket { plugin, dir } => match add_to_deck_market(data, plugin, dir) {
                Ok(market) => {
                    // Adding again after the first time fails; the update
                    // that follows reads the new entry.
                    let path = market.to_string_lossy().into_owned();
                    for (agent, words) in [
                        ("claude", vec!["claude", "plugin", "marketplace", "add", &path]),
                        ("codex", vec!["codex", "plugin", "marketplace", "add", &path]),
                    ] {
                        if !agents.contains(&agent) {
                            continue;
                        }
                        let _ = run::run_args(&words, None, STEP_LIMIT);
                    }
                    if agents.contains(&"claude") {
                        let _ = run::run_args(
                            &["claude", "plugin", "marketplace", "update", DECK_MARKETPLACE],
                            None,
                            STEP_LIMIT,
                        );
                    }
                    if agents.contains(&"codex") {
                        let _ = run::run_args(
                            &["codex", "plugin", "marketplace", "upgrade", DECK_MARKETPLACE],
                            None,
                            STEP_LIMIT,
                        );
                    }
                    report.lines.push(format!("✓ added {plugin} to the deck's marketplace"));
                }
                Err(err) => {
                    report.ok = false;
                    report.lines.push(format!("✗ adding {plugin} to the deck's marketplace: {err:#}"));
                }
            },
            Step::Run { agent, what, argv, record: note } => match run::run(argv, None, &[], STEP_LIMIT) {
                Ok(ran) if ran.ok() => {
                    report.lines.push(format!("✓ {agent}: {what}"));
                    if let Some(note) = note {
                        if !added.contains(note) {
                            added.push(note.clone());
                        }
                    }
                }
                Ok(ran) => {
                    // A marketplace already added is not a failure.
                    let already =
                        what.starts_with("add marketplace") && ran.reason().to_lowercase().contains("already");
                    if already {
                        report.lines.push(format!("✓ {agent}: {what} (already added)"));
                    } else {
                        report.ok = false;
                        report.lines.push(format!("✗ {agent}: {what}: {}", ran.reason()));
                    }
                }
                Err(err) => {
                    report.ok = false;
                    report.lines.push(format!("✗ {agent}: {what}: {err}"));
                }
            },
            Step::Skill { name, dir, agents } => {
                match skills::install_dir(data, home, dir, name, agents, &request.source) {
                    Ok(_) => {
                        report.lines.push(format!("✓ skill {name} for {}", agents.join(", ")));
                        let note = ("skill".to_string(), name.clone());
                        if !added.contains(&note) {
                            added.push(note);
                        }
                    }
                    Err(err) => {
                        report.ok = false;
                        report.lines.push(format!("✗ skill {name}: {err:#}"));
                    }
                }
            }
        }
    }
    if steps.is_empty() {
        report.ok = false;
        report.lines.push("nothing to install for the agents asked for".to_string());
    }
    let given = request.source.trim().to_string();
    let saved = update_records(data, |records| {
        let record = records.entry(source.name().to_string()).or_default();
        record.source = given;
        for note in &added {
            if !record.installed.contains(note) {
                record.installed.push(note.clone());
            }
        }
    });
    if let Err(err) = saved {
        // Without its receipt `:uninstall` cannot undo the install, so it
        // has not completed (audit ROOT-038); name what is unrecorded.
        report.ok = false;
        report.lines.push(format!("{err:#}"));
        if !added.is_empty() {
            let items: Vec<String> = added.iter().map(|(kind, id)| format!("{kind} {id}")).collect();
            report.lines.push(format!("not recorded, so :uninstall cannot remove: {}", items.join(", ")));
        }
    }
    report
}

/// The command that undoes one recorded install.
pub fn undo_argv(kind: &str, id: &str) -> Option<Vec<String>> {
    if id.starts_with('-') {
        return None;
    }
    let words: Vec<&str> = match kind {
        "claude-plugin" => vec!["claude", "plugin", "uninstall", "-s", "user", id],
        "claude-marketplace" => vec!["claude", "plugin", "marketplace", "remove", id],
        "claude-mcp" => vec!["claude", "mcp", "remove", "-s", "user", id],
        "codex-plugin" => vec!["codex", "plugin", "remove", id],
        "codex-marketplace" => vec!["codex", "plugin", "marketplace", "remove", id],
        "codex-mcp" => vec!["codex", "mcp", "remove", id],
        "agy-plugin" => vec!["agy", "plugin", "uninstall", id],
        "agy-mcp" => vec!["agy", "mcp", "remove", id],
        _ => return None,
    };
    Some(argv(&words))
}

/// A source's recorded items in the order `remove` undoes them: plugins
/// before the marketplaces they came from.
fn undo_order(record: &Record) -> Vec<(String, String)> {
    let mut items = record.installed.clone();
    items.sort_by_key(|(kind, _)| kind.ends_with("marketplace"));
    items
}

fn nothing_from(source: &Source) -> Report {
    Report { ok: false, lines: vec![format!("the deck has installed nothing from {}", source.name())] }
}

/// What `remove` would undo, changing nothing.
fn removal_preview(source: &Source, data: &Path) -> Report {
    let records = read_records(data);
    let Some(record) = records.get(source.name()) else { return nothing_from(source) };
    let mut lines = vec![format!("{} would remove:", source.name())];
    for (kind, id) in undo_order(record) {
        if kind == "skill" {
            lines.push(format!("  skill {id}"));
        } else if let Some(argv) = undo_argv(&kind, &id) {
            lines.push(format!("  {kind} {id} ({})", argv.join(" ")));
        }
    }
    Report { ok: true, lines }
}

fn remove(source: &Source, home: &Path, data: &Path) -> Report {
    let Some(record) = read_records(data).remove(source.name()) else { return nothing_from(source) };
    let mut report = Report { ok: true, lines: Vec::new() };
    let mut failed = Vec::new();
    for (kind, id) in undo_order(&record) {
        let error = if kind == "skill" {
            skills::remove(data, home, &id).err().map(|err| format!("{err:#}"))
        } else {
            let Some(argv) = undo_argv(&kind, &id) else { continue };
            match run::run(&argv, None, &[], STEP_LIMIT) {
                Ok(ran) if ran.ok() => None,
                Ok(ran) => Some(ran.reason()),
                Err(err) => Some(err.to_string()),
            }
        };
        match error {
            None => report.lines.push(format!("✓ removed {kind} {id}")),
            Some(why) => {
                report.ok = false;
                report.lines.push(format!("✗ {kind} {id}: {why}"));
                failed.push((kind, id));
            }
        }
    }
    // Retire the receipt of what went; what failed keeps it, so the next
    // uninstall can try again (audit ROOT-037), as does anything another
    // install recorded meanwhile.
    let saved = update_records(data, |records| {
        let Some(current) = records.get_mut(source.name()) else { return };
        current.installed.retain(|item| failed.contains(item) || !record.installed.contains(item));
        if current.installed.is_empty() {
            records.remove(source.name());
        }
    });
    if let Err(err) = saved {
        // The receipt on disk still names what was removed.
        report.ok = false;
        report.lines.push(format!("{err:#}"));
    }
    report
}

/// `cepheus add <source> [--agents claude,codex,agy] [--dry-run]` and
/// `cepheus uninstall <source>`: the installer from a shell, on this host.
pub fn run_cli(args: &[String]) -> i32 {
    let mut request = InstallRequest::default();
    let mut words = args.iter();
    while let Some(arg) = words.next() {
        match arg.as_str() {
            "--dry-run" | "-n" => request.dry_run = true,
            "--remove" => request.remove = true,
            "--agents" | "--agent" => {
                let list = words.next().map(String::as_str).unwrap_or_default();
                request.agents = list.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
            }
            other if request.source.is_empty() => request.source = other.to_string(),
            other => {
                eprintln!("cepheus: unexpected {other:?}");
                return 2;
            }
        }
    }
    let report = install(&request, &crate::paths::home());
    for line in &report.lines {
        println!("{line}");
    }
    if report.ok {
        0
    } else {
        1
    }
}

#[cfg(test)]
#[path = "../../src/tests/installer.rs"]
mod tests;
