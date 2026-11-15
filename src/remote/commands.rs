//! Completion metadata for the phone; never returns skill bodies or credentials.
//! Built-in names: code.claude.com/docs/en/commands and
//! learn.chatgpt.com/docs/developer-commands?surface=cli (2026-10-28).
use crate::protocol::Tool;
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Entry {
    pub name: String,
    pub insert: String,
    pub description: String,
    pub kind: &'static str,
}

const CLAUDE: &str = "add-dir advisor agents artifact-capabilities artifact-diagramming artifacts auto-mode-setup autocompact autofix-pr background batch branch btw bug cd chrome claude-api claude-in-chrome clear code-review color compact config context copy cost dataviz debug deep-research design design-login design-sync desktop diff doctor effort exit export fast feedback fewer-permission-prompts focus fork goal heapdump help hooks ide import init insights install-github-app install-slack-app keybindings list-agents login logout loop mcp memory mobile model output-style passes permissions plan plugin plugin-authoring powerup pr-comments privacy-settings radio rate-limit-options recap release-notes reload-plugins reload-skills remote-control remote-env rename resume review rewind run run-skill-generator sandbox schedule scroll-speed security-review setup-bedrock setup-vertex simplify skill-doctor skills slides stats status statusline stickers stop subtask tasks team-onboarding teleport terminal-setup theme tui ultraplan ultrareview update-config upgrade usage usage-credits verify vim voice web-setup workflow-authoring workflows bg reset new share settings checkpoint rc allowed-tools android app bashes checkup continue ios peers proactive quit routines tp undo";
const CODEX: &str = "copy exit model fast permissions approve raw agents subagents status ide keymap vim setup-default-sandbox apps plugins hooks clear rename archive delete compact diff quit experimental memories skills import feedback init logout mcp mention plan goal ps stop fork app side btw resume new review usage debug-config statusline title theme pets pet tui export recap cd pwd cwd worktree daemon warnings clean";

fn builtin(name: &str) -> Entry {
    let description = match name {
        "model" => "Choose the model and reasoning effort",
        "clear" | "new" | "reset" => "Start a fresh conversation",
        "compact" => "Summarize context to make room",
        "status" | "usage" => "Show session information and usage",
        "skills" => "Browse skills",
        "permissions" => "Manage approval permissions",
        "help" => "Show help",
        "goal" => "Manage a persistent task goal",
        "plan" => "Switch planning mode",
        "resume" => "Continue an earlier conversation",
        "review" | "code-review" => "Review code changes",
        "mcp" => "Manage MCP connections",
        _ => "Agent command",
    };
    Entry { name: format!("/{name}"), insert: format!("/{name}"), description: description.into(), kind: "command" }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 200 && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
}

fn read(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(64 * 1024).read_to_string(&mut text).ok()?;
    Some(text)
}

fn json(path: &Path) -> Value {
    read(path).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Display fields only. Folded/literal YAML descriptions and quoted scalars are common.
fn field(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    while let Some(line) = lines.next() {
        if line.trim() == "---" {
            break;
        }
        let Some(value) = line.strip_prefix(key).and_then(|s| s.strip_prefix(':')) else { continue };
        let value = value.trim();
        if matches!(value, ">" | ">-" | "|" | "|-") {
            return Some(
                lines
                    .take_while(|s| s.starts_with([' ', '\t']) || s.is_empty())
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        if value.starts_with('"') {
            return serde_json::from_str::<String>(value).ok();
        }
        return Some(value.trim_matches('\'').to_string());
    }
    None
}

struct Catalog {
    entries: BTreeMap<String, Entry>,
    visits: usize,
}

impl Catalog {
    fn scan(&mut self, root: &Path, ns: &str, skills: bool, codex: bool) -> Result<()> {
        self.walk(root, root, ns, skills, codex, &mut HashSet::new())
    }
    fn walk(
        &mut self,
        root: &Path,
        dir: &Path,
        ns: &str,
        skills: bool,
        codex: bool,
        seen: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        self.visits += 1;
        if self.visits > 20_000 {
            bail!("Skill catalog is too large; narrow the installed skill directories");
        }
        let Ok(real) = dir.canonicalize() else { return Ok(()) };
        if !seen.insert(real) {
            return Ok(());
        }
        if skills && dir.join("SKILL.md").is_file() {
            self.add(&dir.join("SKILL.md"), dir.file_name().and_then(|n| n.to_str()).unwrap_or(""), ns, true, codex);
            return Ok(());
        }
        for path in children(dir) {
            self.visits += 1;
            if self.visits > 20_000 {
                bail!("Skill catalog is too large; narrow the installed skill directories");
            }
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                self.walk(root, &path, ns, skills, codex, seen)?;
            } else if !skills && path.extension().is_some_and(|e| e == "md") {
                let relative = path.strip_prefix(root).unwrap_or(&path).with_extension("");
                self.add(&path, &relative.to_string_lossy().replace('/', ":"), ns, false, codex);
            }
        }
        Ok(())
    }
    fn add(&mut self, path: &Path, fallback: &str, ns: &str, skill: bool, codex: bool) {
        let Some(text) = read(path) else { return };
        if field(&text, "user-invocable").as_deref() == Some("false") {
            return;
        }
        let name = if skill { field(&text, "name").unwrap_or_else(|| fallback.into()) } else { fallback.into() };
        let name = if ns.is_empty() { name } else { format!("{ns}:{name}") };
        if !valid_name(&name) {
            return;
        }
        let prefix = if skill && codex { '$' } else { '/' };
        let name = format!("{prefix}{name}");
        let description =
            field(&text, "description").unwrap_or_else(|| if skill { "Skill" } else { "Custom command" }.into());
        let description = description.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(1000).collect();
        self.entries.insert(
            name.clone(),
            Entry { name: name.clone(), insert: name, description, kind: if skill { "skill" } else { "command" } },
        );
    }
    fn local(&mut self, dir: &Path, codex: bool) -> Result<()> {
        self.scan(&dir.join("skills"), "", true, codex)?;
        if !codex {
            self.scan(&dir.join("commands"), "", false, false)?;
        }
        Ok(())
    }
    fn plugin(&mut self, dir: &Path, name: &str, codex: bool) -> Result<()> {
        let manifest = json(&dir.join(if codex { ".codex-plugin/plugin.json" } else { ".claude-plugin/plugin.json" }));
        for (key, skills) in [("skills", true), ("commands", false)] {
            if codex && !skills {
                continue;
            }
            self.scan(&dir.join(key), name, skills, codex)?;
            let paths: Vec<&str> = match &manifest[key] {
                Value::String(s) => vec![s],
                Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            for path in paths {
                let target = dir.join(path);
                if let (Ok(root), Ok(real)) = (dir.canonicalize(), target.canonicalize()) {
                    if real.starts_with(root) {
                        self.scan(&target, name, skills, codex)?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<_> = fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.path()).collect();
    out.sort();
    out
}

fn codex_disabled(dir: &Path) -> HashSet<String> {
    let mut disabled = HashSet::new();
    let mut plugin = None;
    for line in read(&dir.join("config.toml")).unwrap_or_default().lines().map(str::trim) {
        if line.starts_with('[') {
            plugin = line
                .strip_prefix("[plugins.")
                .and_then(|s| s.strip_suffix(']'))
                .map(|s| s.trim_matches(['"', '\'']).to_string());
        } else if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "enabled" && value.split('#').next().unwrap_or("").trim() == "false" {
                if let Some(name) = &plugin {
                    disabled.insert(name.clone());
                }
            }
        }
    }
    disabled
}

pub fn all(tool: Tool, cwd: &Path, home: &Path, claude: &Path, codex_dir: &Path) -> Result<Vec<Entry>> {
    let codex = tool == Tool::Codex;
    if !codex && tool != Tool::Claude {
        return Ok(Vec::new());
    }
    let mut catalog = Catalog { entries: BTreeMap::new(), visits: 0 };
    for name in if codex { CODEX } else { CLAUDE }.split_whitespace() {
        let entry = builtin(name);
        catalog.entries.insert(entry.name.clone(), entry);
    }
    if codex {
        let disabled = codex_disabled(codex_dir);
        for market in children(&codex_dir.join("plugins/cache")) {
            let Some(market_name) = market.file_name().and_then(|n| n.to_str()) else { continue };
            for plugin in children(&market) {
                let Some(name) = plugin.file_name().and_then(|n| n.to_str()) else { continue };
                if disabled.contains(&format!("{name}@{market_name}")) {
                    continue;
                }
                let latest = children(&plugin)
                    .into_iter()
                    .filter(|p| p.join(".codex-plugin/plugin.json").is_file())
                    .max_by_key(|p| fs::metadata(p.join(".codex-plugin/plugin.json")).and_then(|m| m.modified()).ok());
                if let Some(dir) = latest {
                    catalog.plugin(&dir, name, true)?;
                }
            }
        }
        catalog.local(codex_dir, true)?;
        catalog.scan(&codex_dir.join("prompts"), "prompts", false, true)?;
        catalog.scan(&home.join(".agents/skills"), "", true, true)?;
    } else {
        let mut enabled =
            json(&claude.join("settings.json"))["enabledPlugins"].as_object().cloned().unwrap_or_default();
        for dir in cwd.ancestors().collect::<Vec<_>>().into_iter().rev() {
            for settings in [".claude/settings.json", ".claude/settings.local.json"] {
                if let Some(values) = json(&dir.join(settings))["enabledPlugins"].as_object() {
                    enabled.extend(values.clone());
                }
            }
        }
        let installed = json(&claude.join("plugins/installed_plugins.json"));
        if let Some(plugins) = installed["plugins"].as_object() {
            for (id, versions) in plugins {
                if enabled.get(id).and_then(Value::as_bool) != Some(true) {
                    continue;
                }
                let name = id.split('@').next().unwrap_or(id);
                for version in versions.as_array().into_iter().flatten() {
                    if let Some(project) = version["projectPath"].as_str() {
                        if !cwd.starts_with(project) {
                            continue;
                        }
                    }
                    if let Some(path) = version["installPath"].as_str() {
                        catalog.plugin(Path::new(path), name, false)?;
                    }
                }
            }
        }
        catalog.local(claude, false)?;
    }
    for dir in cwd.ancestors().collect::<Vec<_>>().into_iter().rev() {
        if codex {
            catalog.scan(&dir.join(".agents/skills"), "", true, true)?;
            catalog.local(&dir.join(".codex"), true)?;
        } else {
            catalog.local(&dir.join(".claude"), false)?;
        }
    }
    Ok(catalog.entries.into_values().collect())
}
#[cfg(test)]
#[path = "../tests/remote_commands.rs"]
mod tests;
