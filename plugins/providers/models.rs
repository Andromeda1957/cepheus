//! The models and reasoning efforts the phone's model pill, the New dialog
//! and the launch settings offer per program. Claude, Codex and Grok are read
//! from their own files; Antigravity and opencode answer only when run, so
//! `refresh` asks them in the service's background and `all` reads the saved
//! answer.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::service::{valid_effort, valid_model_name};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Model {
    pub id: String,
    pub label: String,
    /// Efforts this model takes, when they differ from the program's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
}

/// Where the programs' model lists and their own defaults are read from.
#[derive(Clone, Debug, PartialEq)]
pub struct Homes {
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub grok: PathBuf,
    pub antigravity: PathBuf,
    /// The service's state folder, which keeps the lists it fetched
    /// (`refresh`).
    pub state: PathBuf,
}

impl Homes {
    /// This machine's, where the deck finds them.
    pub fn here() -> Self {
        Self {
            claude: crate::paths::claude_dir(),
            codex: crate::paths::codex_dir(),
            grok: crate::paths::grok_home(),
            antigravity: crate::paths::antigravity_dir(),
            state: crate::paths::state_dir(),
        }
    }

    /// Each in its own folder under `dir`, for tests.
    #[cfg(test)]
    pub fn under(dir: &Path) -> Self {
        Self {
            claude: dir.join("claude"),
            codex: dir.join("codex"),
            grok: dir.join("grok"),
            antigravity: dir.join("agy"),
            state: dir.join("state"),
        }
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ToolModels {
    pub tool: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub cloud: bool,
    pub models: Vec<Model>,
    pub efforts: Vec<String>,
    /// A running session's model can be switched from the phone.
    pub switch: bool,
    /// What the program starts with when the phone picks nothing, from its
    /// own settings, so the phone can say so instead of just "Default". The
    /// deck's saved launch settings replace them where set (`launch::overlay`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

fn model(id: &str, label: &str) -> Model {
    Model { id: id.into(), label: label.into(), efforts: Vec::new() }
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// Codex's listed models from its own cache (`models_cache.json`, written
/// by Codex 0.164): slug, display name and efforts.
pub fn codex_models(codex_dir: &Path) -> Vec<Model> {
    let Ok(bytes) = fs::read(codex_dir.join("models_cache.json")) else { return Vec::new() };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { return Vec::new() };
    let mut out = Vec::new();
    for m in v.get("models").and_then(Value::as_array).into_iter().flatten() {
        if m.get("visibility").and_then(Value::as_str) == Some("hide") {
            continue;
        }
        let Some(slug) = m.get("slug").and_then(Value::as_str).filter(|s| crate::service::valid_model_name(s)) else {
            continue;
        };
        let label = m.get("display_name").and_then(Value::as_str).unwrap_or(slug);
        let efforts = m
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|e| e.get("effort").and_then(Value::as_str))
            .filter(|e| crate::service::valid_effort(e))
            .map(str::to_string)
            .collect();
        out.push(Model { id: slug.into(), label: label.into(), efforts });
    }
    out
}

/// Every program's list. Claude takes the aliases its `--help` names
/// (2026-10-27: "fable", "opus", "sonnet") and `--effort`'s levels; Codex
/// and Grok their own caches; Antigravity and opencode the lists the
/// service last fetched (`refresh`), none before it has.
pub fn all(homes: &Homes) -> Vec<ToolModels> {
    let (claude_model, claude_effort) = claude_defaults(&homes.claude);
    let (codex_model, codex_effort) = codex_defaults(&homes.codex);
    let (grok, grok_model, grok_effort) = grok_models(&homes.grok);
    let fetched = fetched(&homes.state);
    let (agy_model, agy_effort) = antigravity_defaults(&homes.antigravity, &fetched.antigravity);
    let plain = |tool| ToolModels {
        tool,
        label: None,
        cloud: false,
        models: Vec::new(),
        efforts: Vec::new(),
        switch: false,
        default_model: None,
        default_effort: None,
    };
    let mut tools = vec![
        ToolModels {
            tool: "claude",
            label: None,
            cloud: false,
            models: vec![model("opus", "Opus"), model("sonnet", "Sonnet"), model("fable", "Fable")],
            efforts: words(&["low", "medium", "high", "xhigh", "max"]),
            switch: true,
            default_model: claude_model,
            default_effort: claude_effort,
        },
        ToolModels {
            tool: "codex",
            label: None,
            cloud: false,
            models: codex_models(&homes.codex),
            efforts: Vec::new(),
            switch: true,
            default_model: codex_model,
            default_effort: codex_effort,
        },
        ToolModels {
            models: fetched.antigravity,
            default_model: agy_model,
            default_effort: agy_effort,
            ..plain("antigravity")
        },
        ToolModels { models: grok, default_model: grok_model, default_effort: grok_effort, ..plain("grok") },
        ToolModels { models: fetched.opencode, ..plain("opencode") },
    ];
    for tool in crate::providers::tools().into_iter().filter(|t| matches!(t, crate::protocol::Tool::Dynamic(_))) {
        if let Some(provider) = crate::providers::info(tool) {
            tools.push(ToolModels {
                tool: tool.key(),
                label: Some(provider.label.clone()),
                cloud: provider.cloud,
                models: provider
                    .models
                    .into_iter()
                    .map(|m| Model {
                        label: if m.label.is_empty() { m.id.clone() } else { m.label },
                        id: m.id,
                        efforts: m.efforts,
                    })
                    .collect(),
                efforts: provider.efforts,
                switch: false,
                default_model: None,
                default_effort: None,
            });
        }
    }
    tools.retain(|entry| crate::protocol::Tool::builtin(entry.tool).is_none_or(crate::providers::enabled));
    tools
}

fn checked(model: Option<&str>, effort: Option<&str>) -> (Option<String>, Option<String>) {
    (
        model.filter(|m| crate::service::valid_model_name(m)).map(str::to_string),
        effort.filter(|e| crate::service::valid_effort(e)).map(str::to_string),
    )
}

/// Claude Code's own `model` and `effortLevel` from its user settings.
pub(crate) fn claude_defaults(claude_dir: &Path) -> (Option<String>, Option<String>) {
    let settings = fs::read(claude_dir.join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_default();
    checked(settings.get("model").and_then(Value::as_str), settings.get("effortLevel").and_then(Value::as_str))
}

/// Codex's top-level `model` and `model_reasoning_effort` from its
/// `config.toml`; a profile's or other table's values are not the default.
pub(crate) fn codex_defaults(codex_dir: &Path) -> (Option<String>, Option<String>) {
    let text = fs::read_to_string(codex_dir.join("config.toml")).unwrap_or_default();
    let (mut model, mut effort) = (None, None);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            break;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        let quote = match value.chars().next() {
            Some(q @ ('"' | '\'')) => q,
            _ => continue,
        };
        let Some(value) = value[1..].split(quote).next() else { continue };
        match key.trim() {
            "model" => model = Some(value.to_string()),
            "model_reasoning_effort" => effort = Some(value.to_string()),
            _ => {}
        }
    }
    checked(model.as_deref(), effort.as_deref())
}

fn read_json(path: &Path) -> Value {
    fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Grok's listed models from its own cache (`models_cache.json`, written by
/// Grok 1.0.41: `models.<slug>.info` with `name`, `hidden` and
/// `reasoning_efforts`), and the model it starts on: `[models] default` in
/// its `config.toml`, else the account's (`settings_cache.json`), at that
/// model's own effort.
pub(crate) fn grok_models(home: &Path) -> (Vec<Model>, Option<String>, Option<String>) {
    let cache = read_json(&home.join("models_cache.json"));
    let mut models = Vec::new();
    for (slug, entry) in cache["models"].as_object().into_iter().flatten() {
        let info = &entry["info"];
        if info["hidden"] == true || !valid_model_name(slug) {
            continue;
        }
        let efforts = info["reasoning_efforts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["value"].as_str())
            .filter(|e| valid_effort(e))
            .map(str::to_string)
            .collect();
        models.push(Model { id: slug.clone(), label: info["name"].as_str().unwrap_or(slug).into(), efforts });
    }
    let configured = fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|config| config.get("models")?.get("default")?.as_str().map(str::to_string));
    let account = || {
        let settings = read_json(&home.join("settings_cache.json"));
        let payload = match &settings["payload"] {
            Value::String(text) => serde_json::from_str(text).unwrap_or_default(),
            other => other.clone(),
        };
        payload["settings"]["default_model"].as_str().map(str::to_string)
    };
    let model = configured.or_else(account);
    let effort = model.as_deref().and_then(|m| cache["models"][m]["info"]["reasoning_effort"].as_str());
    let (model, effort) = checked(model.as_deref(), effort);
    (models, model, effort)
}

/// The efforts Antigravity's `--effort` takes, in its order.
const AGY_EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Antigravity's models from `agy models` ("slug<TAB>Label" lines). Each
/// effort variant (`gemini-4-argon-high`, "Gemini 4 Argon (High)") folds
/// into its base model, which `--model` takes with `--effort` (Antigravity's
/// changelog: "`--model` and `/model` … accept a base model name").
pub(crate) fn antigravity_models(text: &str) -> Vec<Model> {
    let mut out: Vec<Model> = Vec::new();
    for line in text.lines() {
        let Some((slug, label)) = line.split_once('\t') else { continue };
        let (slug, label) = (slug.trim(), label.trim());
        let (id, effort) = AGY_EFFORTS
            .iter()
            .find_map(|e| slug.strip_suffix(&format!("-{e}")).map(|base| (base, Some(*e))))
            .unwrap_or((slug, None));
        if !valid_model_name(id) {
            continue;
        }
        let label = match label.rsplit_once(" (") {
            Some((base, _)) if effort.is_some() => base,
            _ => label,
        };
        let index = match out.iter().position(|m| m.id == id) {
            Some(index) => index,
            None => {
                out.push(Model { id: id.into(), label: label.into(), efforts: Vec::new() });
                out.len() - 1
            }
        };
        if let Some(effort) = effort.filter(|e| !out[index].efforts.iter().any(|x| x == e)) {
            out[index].efforts.push(effort.into());
        }
    }
    for m in &mut out {
        m.efforts.sort_by_key(|e| AGY_EFFORTS.iter().position(|x| x == e));
    }
    out
}

/// Antigravity's own model from its `settings.json`, which names it by its
/// label ("Gemini 4 Argon (Medium)"), as a base model and effort.
fn antigravity_defaults(dir: &Path, models: &[Model]) -> (Option<String>, Option<String>) {
    let settings = read_json(&dir.join("settings.json"));
    let Some(name) = settings["model"].as_str() else { return (None, None) };
    let (base, effort) = match name.rsplit_once(" (") {
        Some((base, rest)) => (base, rest.strip_suffix(')').map(str::to_lowercase)),
        None => (name, None),
    };
    let Some(model) = models.iter().find(|m| m.label == base || m.id == name) else { return (None, None) };
    let effort = effort.filter(|e| model.efforts.contains(e));
    checked(Some(&model.id), effort.as_deref())
}

/// opencode's models from its native catalog (`opencode api model.list`):
/// `provider/model` ids, as `-m` and native New take them, with their
/// variants as efforts. Disabled and deprecated models are left out.
pub(crate) fn opencode_models(catalog: &Value) -> Vec<Model> {
    let mut out = Vec::new();
    for m in catalog["data"].as_array().into_iter().flatten() {
        if m["enabled"] == false || m["status"] == "deprecated" {
            continue;
        }
        let (Some(provider), Some(id)) = (m["providerID"].as_str(), m["id"].as_str()) else { continue };
        let id = format!("{provider}/{id}");
        if !valid_model_name(&id) {
            continue;
        }
        let efforts = m["variants"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["id"].as_str())
            .filter(|v| *v != "default" && valid_effort(v))
            .map(str::to_string)
            .collect();
        out.push(Model { label: m["name"].as_str().unwrap_or(&id).into(), id, efforts });
    }
    out
}

/// The lists that take running a program to read: Antigravity's comes from
/// the network and opencode's from a server of its own. The service fetches
/// them now and then (`refresh`), so reading a list never waits on either.
#[derive(Serialize, Deserialize, Default, Debug)]
struct Fetched {
    #[serde(default)]
    antigravity: Vec<Model>,
    #[serde(default)]
    opencode: Vec<Model>,
}

fn fetched_path(state: &Path) -> PathBuf {
    state.join("model-lists.json")
}

fn fetched(state: &Path) -> Fetched {
    fs::read(fetched_path(state)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// How long one program has to list its models.
const FETCH_LIMIT: Duration = Duration::from_secs(30);

/// Run `command` (a single program, as the quota readers require, so no
/// launch argument can start a session) with `args` in `cwd`.
fn list_output(command: &[String], args: &[String], cwd: &Path) -> Option<String> {
    let [program] = command else { return None };
    let argv: Vec<String> = std::iter::once(program.clone()).chain(args.iter().cloned()).collect();
    let ran = crate::run::run(&argv, Some(cwd), &crate::client_temp::vars("models").ok()?, FETCH_LIMIT).ok()?;
    ran.ok().then_some(ran.stdout)
}

/// Fetch Antigravity's and opencode's lists again and keep them in the
/// state folder. A program that does not answer keeps its last list.
/// opencode answers from a private server (`--standalone`), so this never
/// starts its shared background one.
pub fn refresh(config: &Config, cwd: &Path, state: &Path) -> anyhow::Result<()> {
    use crate::protocol::Tool;
    let mut lists = fetched(state);
    if crate::providers::enabled(Tool::Antigravity) {
        let found = list_output(&config.antigravity_command, &["models".into()], cwd)
            .map(|text| antigravity_models(&text))
            .filter(|models| !models.is_empty());
        if let Some(models) = found {
            lists.antigravity = models;
        }
    }
    if crate::providers::enabled(Tool::OpenCode) {
        let directory = format!("location[directory]={}", cwd.display());
        let args = ["api", "model.list", "--standalone", "--param", &directory].map(str::to_string);
        let found = list_output(&config.opencode_command, &args, cwd)
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|catalog| opencode_models(&catalog))
            .filter(|models| !models.is_empty());
        if let Some(models) = found {
            lists.opencode = models;
        }
    }
    fs::create_dir_all(state)?;
    let staging = state.join(format!(".model-lists-{}.json", std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&staging)?;
        file.write_all(&serde_json::to_vec(&lists)?)?;
        file.sync_all()?;
        fs::rename(&staging, fetched_path(state))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

#[cfg(test)]
#[path = "../../src/tests/remote_models.rs"]
mod tests;

/// Whether option `label` in Codex's `/model` or effort list is `want`:
/// the slug or display name, case aside; "xhigh" is shown "Extra high".
pub fn option_matches(label: &str, want: &str) -> bool {
    let norm = |s: &str| s.to_lowercase().replace(['-', '_'], " ");
    let label = norm(label);
    let want = norm(want);
    let first = label.split_whitespace().next().unwrap_or("");
    let first_two: String = label.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    first == want
        || label.split(" (").next() == Some(want.as_str())
        || (want == "xhigh" && (first_two == "extra high" || first == "xhigh"))
        || label.starts_with(&format!("{want} "))
}
