//! Agent integrations. Native adapters ship enabled; additional PTY agents use
//! validated, argument-vector manifests without extending the protocol enum.
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    config::Config,
    protocol::{Tool, ALL_TOOLS},
    service::StartModel,
    status::ScreenSignals,
};

#[path = "../../plugins/providers/accounts.rs"]
pub mod accounts;
#[path = "../../plugins/providers/antigravity.rs"]
pub mod antigravity;
#[path = "../../plugins/providers/claude.rs"]
pub mod claude;
#[path = "../../plugins/providers/claude_keybindings.rs"]
pub mod claude_keybindings;
#[path = "../../plugins/providers/codex.rs"]
pub mod codex;
#[path = "../../plugins/launch-settings/settings.rs"]
pub mod launch;
#[path = "../../plugins/providers/messages.rs"]
pub mod messages;
#[path = "../../plugins/providers/models.rs"]
pub mod models;
#[path = "../../plugins/providers/monitor.rs"]
pub mod monitor;
#[path = "../../plugins/providers/native.rs"]
pub mod native;
#[path = "../../plugins/providers/other.rs"]
pub mod other;
#[path = "../../plugins/providers/peers.rs"]
pub mod peers;
#[path = "../../plugins/providers/quotas.rs"]
pub mod quotas;
#[path = "../../plugins/providers/recovery.rs"]
pub mod recovery;
#[path = "../../plugins/providers/screen.rs"]
pub mod screen;
#[path = "../../plugins/providers/snapshot.rs"]
pub mod snapshot;
#[path = "../../plugins/providers/transcripts.rs"]
pub mod transcripts;

/// A bounded intern table keeps Tool Copy, as the existing session protocol and
/// UI expect. Unknown remote IDs are accepted for display, never for execution.
pub fn intern(text: &str) -> Result<&'static str> {
    static STRINGS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut strings = STRINGS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&text) = strings.get(text) {
        return Ok(text);
    }
    anyhow::ensure!(strings.len() < 4096 && text.len() <= 100, "provider string limit reached");
    let text = Box::leak(text.to_string().into_boxed_str());
    strings.insert(text);
    Ok(text)
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id.as_bytes()[0].is_ascii_lowercase()
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub efforts: Vec<String>,
}

/// A provider manifest. Placeholders are whole argument words; they are never
/// evaluated or interpolated by a shell. A new session gets a UUID when
/// new_session_args is set, so its persisted reopen can name the same chat.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub label: String,
    pub command: Vec<String>,
    /// Persistent cloud connections are presented above local agents.
    #[serde(default)]
    pub cloud: bool,
    /// Local-only JSON message snapshot; never advertised to other hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_snapshot: Option<PathBuf>,
    #[serde(default)]
    pub model_flag: Option<String>,
    #[serde(default)]
    pub effort_flag: Option<String>,
    #[serde(default)]
    pub new_session_args: Vec<String>,
    #[serde(default)]
    pub resume_args: Vec<String>,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default = "yes")]
    pub broadcast: bool,
    #[serde(default)]
    pub working_patterns: Vec<String>,
    #[serde(default)]
    pub prompt_patterns: Vec<String>,
}

impl Provider {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(valid_id(&self.id) && Tool::builtin(&self.id).is_none(), "invalid or reserved provider id");
        anyhow::ensure!(text(&self.label, 80) && !self.label.trim().is_empty(), "invalid provider label");
        argv(&self.command)?;
        if let Some(path) = &self.chat_snapshot {
            anyhow::ensure!(
                path.is_absolute() && path.to_str().is_some_and(|s| text(s, 4096)),
                "chat snapshot must be an absolute local path"
            );
        }
        let exe = Path::new(&self.command[0]).file_name().and_then(|s| s.to_str()).unwrap_or("");
        anyhow::ensure!(
            !["sh", "bash", "zsh", "dash", "fish", "cmd", "cmd.exe", "powershell", "pwsh"].contains(&exe),
            "provider command must execute the agent directly, without a shell"
        );
        for flag in [&self.model_flag, &self.effort_flag].into_iter().flatten() {
            anyhow::ensure!(
                flag.len() <= 64
                    && flag.starts_with('-')
                    && flag.len() > 1
                    && flag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "invalid provider option flag"
            );
        }
        for args in [&self.new_session_args, &self.resume_args] {
            anyhow::ensure!(args.len() <= 32 && args.iter().all(|s| text(s, 1024)), "invalid session arguments");
            anyhow::ensure!(
                args.is_empty() || args.iter().filter(|s| s.as_str() == "{conversation}").count() == 1,
                "session arguments require one whole {{conversation}} argument"
            );
            anyhow::ensure!(
                args.iter().all(|s| !s.contains('{') || s == "{conversation}"),
                "unknown argument placeholder"
            );
        }
        anyhow::ensure!(
            self.new_session_args.is_empty() || !self.resume_args.is_empty(),
            "new session id requires resume arguments"
        );
        anyhow::ensure!(self.models.is_empty() || self.model_flag.is_some(), "model choices require a model flag");
        anyhow::ensure!(
            (self.efforts.is_empty() && self.models.iter().all(|m| m.efforts.is_empty())) || self.effort_flag.is_some(),
            "effort choices require an effort flag"
        );
        anyhow::ensure!(self.models.len() <= 128 && self.efforts.len() <= 16, "too many model choices");
        let mut seen = HashSet::new();
        for model in &self.models {
            anyhow::ensure!(
                crate::service::valid_model_name(&model.id)
                    && text(&model.label, 80)
                    && seen.insert(&model.id)
                    && model.efforts.len() <= 16
                    && model.efforts.iter().all(|s| crate::service::valid_effort(s)),
                "invalid or duplicate model choice"
            );
        }
        anyhow::ensure!(self.efforts.iter().all(|s| crate::service::valid_effort(s)), "invalid effort choice");
        for patterns in [&self.working_patterns, &self.prompt_patterns] {
            anyhow::ensure!(
                patterns.len() <= 32 && patterns.iter().all(|s| text(s, 160) && !s.trim().is_empty()),
                "invalid status patterns"
            );
        }
        Ok(())
    }

    pub fn launch(&self, resume: Option<&str>) -> Result<Launch> {
        self.validate()?;
        if let Some(id) = resume {
            anyhow::ensure!(crate::service::valid_conversation_id(id), "invalid conversation id");
        }
        let conversation = match resume {
            Some(id) => {
                anyhow::ensure!(!self.resume_args.is_empty(), "provider does not support conversation resume");
                Some(id.to_string())
            }
            None if !self.new_session_args.is_empty() => Some(crate::util::uuid_v4()?),
            None => None,
        };
        let mut argv = self.command.clone();
        let args = if resume.is_some() { &self.resume_args } else { &self.new_session_args };
        argv.extend(args.iter().map(|arg| {
            if arg == "{conversation}" {
                conversation.clone().unwrap_or_default()
            } else {
                arg.clone()
            }
        }));
        Ok(Launch { argv, conversation, attached: false })
    }

    pub fn flags(&self, start: &StartModel) -> Result<Vec<String>> {
        let mut args = Vec::new();
        for (value, flag, valid) in [
            (start.model.as_deref(), &self.model_flag, crate::service::valid_model_name as fn(&str) -> bool),
            (start.effort.as_deref(), &self.effort_flag, crate::service::valid_effort as fn(&str) -> bool),
        ] {
            if let Some(value) = value.filter(|s| !s.is_empty()) {
                anyhow::ensure!(valid(value), "invalid model or effort");
                let flag = flag.as_ref().context("provider does not support this model or effort option")?;
                args.extend([flag.clone(), value.to_string()]);
            }
        }
        Ok(args)
    }
}

fn text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}
fn argv(args: &[String]) -> Result<()> {
    anyhow::ensure!(
        !args.is_empty() && args.len() <= 64 && args.iter().all(|s| text(s, 4096)),
        "invalid provider command"
    );
    anyhow::ensure!(
        !args[0].is_empty() && !args[0].starts_with('-') && !args[0].trim().is_empty(),
        "invalid provider executable"
    );
    Ok(())
}

/// Safe discovery data: no executable or filesystem paths cross a host boundary.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Info {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub cloud: bool,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default = "yes")]
    pub broadcast: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}
impl From<&Provider> for Info {
    fn from(p: &Provider) -> Self {
        Self {
            id: p.id.clone(),
            label: p.label.clone(),
            cloud: p.cloud,
            models: p.models.clone(),
            efforts: p.efforts.clone(),
            broadcast: p.broadcast,
            host: None,
        }
    }
}
fn advertised() -> &'static RwLock<BTreeMap<&'static str, (Info, &'static str)>> {
    static ADVERTISED: OnceLock<RwLock<BTreeMap<&'static str, (Info, &'static str)>>> = OnceLock::new();
    ADVERTISED.get_or_init(Default::default)
}
/// A remote service may advertise display/model capabilities, never executable registrations.
pub fn advertise(infos: &[Info]) {
    let mut next = BTreeMap::new();
    for info in infos.iter().take(256) {
        if !valid_id(&info.id)
            || Tool::builtin(&info.id).is_some()
            || !text(&info.label, 80)
            || info.models.len() > 128
            || info.efforts.len() > 16
            || !info.models.iter().all(|m| {
                crate::service::valid_model_name(&m.id)
                    && text(&m.label, 80)
                    && m.efforts.len() <= 16
                    && m.efforts.iter().all(|e| crate::service::valid_effort(e))
            })
            || !info.efforts.iter().all(|e| crate::service::valid_effort(e))
        {
            continue;
        }
        if let (Ok(id), Ok(label)) = (intern(&info.id), intern(&info.label)) {
            next.insert(id, (info.clone(), label));
        }
    }
    *advertised().write().unwrap_or_else(|e| e.into_inner()) = next;
}
pub fn info(tool: Tool) -> Option<Info> {
    get(tool).as_ref().map(Info::from).or_else(|| {
        advertised().read().unwrap_or_else(|e| e.into_inner()).get(tool.key()).map(|(info, _)| info.clone())
    })
}

struct Registered {
    definition: Provider,
    label: &'static str,
}
fn registry() -> &'static RwLock<BTreeMap<&'static str, Registered>> {
    static REGISTRY: OnceLock<RwLock<BTreeMap<&'static str, Registered>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Replace the custom registry atomically after validating every registration.
pub fn install(providers: &[Provider]) -> Result<()> {
    anyhow::ensure!(providers.len() <= 64, "at most 64 additional providers are supported");
    let mut ids = HashSet::new();
    for provider in providers {
        provider.validate()?;
        anyhow::ensure!(ids.insert(&provider.id), "duplicate provider id");
    }
    let mut next = BTreeMap::new();
    for provider in providers {
        next.insert(
            intern(&provider.id)?,
            Registered { definition: provider.clone(), label: intern(&provider.label)? },
        );
    }
    *registry().write().unwrap_or_else(|e| e.into_inner()) = next;
    Ok(())
}

pub fn get(tool: Tool) -> Option<Provider> {
    registry().read().unwrap_or_else(|e| e.into_inner()).get(tool.key()).map(|p| p.definition.clone())
}
pub fn label(id: &'static str) -> &'static str {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .map(|p| p.label)
        .or_else(|| advertised().read().unwrap_or_else(|e| e.into_inner()).get(id).map(|(_, label)| *label))
        .unwrap_or(id)
}
pub fn parse(id: &str) -> Option<Tool> {
    registry().read().unwrap_or_else(|e| e.into_inner()).get_key_value(id).map(|(&id, _)| Tool::Dynamic(id)).or_else(
        || advertised().read().unwrap_or_else(|e| e.into_inner()).get_key_value(id).map(|(&id, _)| Tool::Dynamic(id)),
    )
}
fn plugin_settings() -> &'static RwLock<BTreeMap<String, bool>> {
    static SETTINGS: OnceLock<RwLock<BTreeMap<String, bool>>> = OnceLock::new();
    SETTINGS.get_or_init(Default::default)
}
pub fn configure_plugins(settings: &BTreeMap<String, bool>) {
    *plugin_settings().write().unwrap_or_else(|e| e.into_inner()) = settings.clone();
}
pub fn enabled(tool: Tool) -> bool {
    tool == Tool::Shell
        || plugin_settings().read().unwrap_or_else(|e| e.into_inner()).get(&format!("provider-{}", tool.key()))
            != Some(&false)
}

pub fn tools() -> Vec<Tool> {
    let mut tools = ALL_TOOLS.into_iter().filter(|t| t.is_agent()).collect::<Vec<_>>();
    tools.extend(registry().read().unwrap_or_else(|e| e.into_inner()).keys().map(|&id| Tool::Dynamic(id)));
    for &id in advertised().read().unwrap_or_else(|e| e.into_inner()).keys() {
        if !tools.contains(&Tool::Dynamic(id)) {
            tools.push(Tool::Dynamic(id));
        }
    }
    tools.retain(|tool| enabled(*tool));
    tools
}
pub fn takes_broadcast(tool: Tool) -> bool {
    tool.is_agent() && info(tool).is_none_or(|p| p.broadcast)
}

pub fn load_manifests(config: &Path, providers: &mut Vec<Provider>) -> Result<()> {
    let directory = config.with_file_name("providers");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut files = entries.collect::<std::io::Result<Vec<_>>>()?;
    files.sort_by_key(|entry| entry.file_name());
    for entry in files {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        anyhow::ensure!(providers.len() < 64, "too many providers");
        let meta = fs::symlink_metadata(&path)?;
        anyhow::ensure!(meta.is_file() && meta.len() <= 65536, "provider manifest must be a regular file up to 64 KiB");
        let mut bytes = Vec::new();
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?
            .take(65537)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 65536, "provider manifest exceeds 64 KiB");
        let provider = serde_json::from_slice::<Provider>(&crate::config::strip_comments(&bytes))
            .with_context(|| format!("reading {}", path.display()))?;
        providers.push(provider);
    }
    Ok(())
}

pub struct Launch {
    pub argv: Vec<String>,
    pub conversation: Option<String>,
    pub attached: bool,
}
pub fn configured_launch(cfg: &Config, tool: Tool, resume: Option<&str>) -> Result<Launch> {
    cfg.providers.iter().find(|p| p.id == tool.key()).context("provider is not registered on this host")?.launch(resume)
}
pub fn configured_model_flags(tool: Tool, start: &StartModel) -> Result<Vec<String>> {
    get(tool).context("provider is not registered on this host")?.flags(start)
}

/// Whether a program's screen signals follow from its screen alone, so an
/// unchanged screen gives the same ones. A chat snapshot bridge's working
/// state comes from its snapshot file instead.
pub fn signals_follow_screen(tool: Tool) -> bool {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(tool.key())
        .is_none_or(|p| p.definition.id != "dot" && p.definition.chat_snapshot.is_none())
}

pub fn screen_signals(tool: Tool, lines: &[String], pid: Option<u32>) -> ScreenSignals {
    match get(tool) {
        Some(provider) => configured_screen_signals(&provider, lines, pid, crate::util::now_ms()),
        None if tool == Tool::Dynamic("dot") => ScreenSignals::default(),
        None => crate::status::screen_signals(lines),
    }
}

fn configured_screen_signals(provider: &Provider, lines: &[String], pid: Option<u32>, now_ms: u64) -> ScreenSignals {
    if provider.id == "dot" {
        return ScreenSignals {
            working: provider.chat_snapshot.as_ref().is_some_and(|path| snapshot::working(path, pid, now_ms)),
            ..Default::default()
        };
    }
    // A snapshot bridge prints conversation text rather than interactive
    // terminal approvals. Only its explicitly configured signals apply.
    let mut signals =
        if provider.chat_snapshot.is_some() { ScreenSignals::default() } else { crate::status::screen_signals(lines) };
    let tail = crate::status::live_tail(lines, 30).join("\n").to_lowercase();
    signals.working |= provider.working_patterns.iter().any(|s| tail.contains(&s.to_lowercase()));
    if provider.prompt_patterns.iter().any(|s| tail.contains(&s.to_lowercase())) {
        signals.prompt = Some("question");
    }
    signals
}

#[cfg(test)]
#[path = "../tests/providers.rs"]
mod tests;
