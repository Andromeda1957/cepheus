//! Extension composition and generic service-owned process supervision.
//! Integration policy lives in the bundled packages or trusted profile scripts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::processes::{self, Ended, ManagedProcess};

#[path = "../plugins/conventions/native.rs"]
pub mod conventions;

/// Keep non-cancellable cleanup alive through service shutdown and handover.
pub struct WorkGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl WorkGuard {
    pub fn new(count: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(count)
    }
}
impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pub const CATALOG: &str = include_str!("../plugins/catalog.json");

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Command {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub commands: Vec<Command>,
}

/// The catalog owns built-in names and capabilities. Unknown extension IDs
/// have no native capability; trusted service scripts register their own commands.
pub fn plugins(settings: &BTreeMap<String, bool>) -> Vec<Plugin> {
    let catalog: Value = serde_json::from_str(CATALOG).expect("validated bundled plugin catalog");
    catalog["plugins"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(id, item)| Plugin {
            id: id.clone(),
            name: item["name"].as_str().unwrap_or(id).to_string(),
            description: item["description"].as_str().unwrap_or_default().to_string(),
            enabled: settings
                .get(id)
                .copied()
                .unwrap_or_else(|| item["default"].as_bool().unwrap_or(item["kind"].as_str() == Some("builtin"))),
            commands: serde_json::from_value(item["commands"].clone()).unwrap_or_default(),
        })
        .collect()
}

pub fn enabled(settings: &BTreeMap<String, bool>, id: &str) -> bool {
    settings
        .get(id)
        .copied()
        .unwrap_or_else(|| plugins(settings).iter().any(|plugin| plugin.id == id && plugin.enabled))
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessSpec {
    pub cmd: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessSnapshot {
    pub plugin: String,
    pub name: String,
    pub spec: ProcessSpec,
    pub pid: u32,
    pub started_at: u64,
    pub log: PathBuf,
    pub stopping: bool,
}

struct Process {
    plugin: String,
    name: String,
    spec: ProcessSpec,
    child: ManagedProcess,
}

#[derive(Default)]
pub struct Processes {
    items: BTreeMap<String, Process>,
}

impl Processes {
    pub fn start(
        &mut self,
        plugin: &str,
        name: &str,
        spec: ProcessSpec,
        start_dir: &Path,
        state_dir: &Path,
        env_remove: &[String],
    ) -> Result<()> {
        if !valid_id(plugin) || !valid_id(name) {
            bail!("Invalid plugin process name");
        }
        let key = format!("{plugin}.{name}");
        if let Some(previous) = self.items.get(&key) {
            if previous.spec == spec && !previous.child.stopping() {
                return Ok(());
            }
            bail!("This plugin process is already running");
        }
        if self.items.len() >= 16 {
            bail!("Too many plugin processes");
        }
        if spec.cmd.is_empty() || spec.cmd.len() > 128 || spec.cmd.iter().any(|s| s.len() > 4096 || s.contains('\0')) {
            bail!("A plugin process needs a bounded argument list");
        }
        if spec.port == Some(0) {
            bail!("A process port must be nonzero");
        }
        if spec.port.is_some_and(processes::port_listening) {
            bail!("The process port is already in use");
        }
        let cwd = spec.cwd.as_deref().unwrap_or(start_dir).canonicalize().context("Plugin process directory")?;
        if !cwd.is_dir() {
            bail!("The plugin process directory is not a directory");
        }
        let dir = state_dir.join("plugins").join(plugin);
        crate::util::ensure_private_dir(&dir)?;
        let child = ManagedProcess::start(&spec.cmd, &cwd, &dir.join(format!("{name}.log")), env_remove)?;
        self.items.insert(key, Process { plugin: plugin.into(), name: name.into(), spec, child });
        Ok(())
    }

    pub fn stop(&mut self, plugin: &str, name: &str) -> Result<()> {
        self.items.get_mut(&format!("{plugin}.{name}")).context("No such owned plugin process")?.child.stop();
        Ok(())
    }

    pub fn stop_all(&mut self) {
        for process in self.items.values_mut() {
            process.child.stop();
        }
    }

    pub fn kill_all(&mut self) {
        for process in self.items.values_mut() {
            process.child.kill();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn poll(&mut self, now: Instant) -> Vec<(String, String, Ended)> {
        let ended: Vec<_> = self
            .items
            .iter_mut()
            .filter_map(|(key, process)| {
                process.child.poll(now).map(|ended| (key.clone(), process.plugin.clone(), process.name.clone(), ended))
            })
            .collect();
        ended
            .into_iter()
            .map(|(key, plugin, name, ended)| {
                self.items.remove(&key);
                (plugin, name, ended)
            })
            .collect()
    }

    pub fn snapshot(&self) -> Vec<ProcessSnapshot> {
        self.items
            .values()
            .map(|process| ProcessSnapshot {
                plugin: process.plugin.clone(),
                name: process.name.clone(),
                spec: process.spec.clone(),
                pid: process.child.pid,
                started_at: process.child.started_at,
                log: process.child.log.clone(),
                stopping: process.child.stopping(),
            })
            .collect()
    }

    /// Only the same service's authenticated exec handover calls this method.
    pub fn adopt(&mut self, snapshot: Vec<ProcessSnapshot>, state_dir: &Path) -> Result<()> {
        let root = state_dir.join("plugins");
        let mut invalid = false;
        for process in snapshot {
            let Some(stat) = crate::procs::stat(process.pid) else {
                continue;
            };
            // SAFETY: getpgid only inspects the supplied process id.
            let group = unsafe { libc::getpgid(process.pid as libc::pid_t) };
            if !valid_id(&process.plugin)
                || !valid_id(&process.name)
                || process.pid < 2
                || stat.ppid != std::process::id()
                || group != process.pid as libc::pid_t
                || !process.log.starts_with(&root)
            {
                invalid = true;
                continue;
            }
            let key = format!("{}.{}", process.plugin, process.name);
            self.items.insert(
                key,
                Process {
                    plugin: process.plugin,
                    name: process.name,
                    spec: process.spec,
                    child: ManagedProcess::adopt(process.pid, process.started_at, process.log, process.stopping),
                },
            );
        }
        if invalid {
            bail!("Invalid or unowned plugin process in handover");
        }
        Ok(())
    }
}

#[path = "../plugins/agent-extensions/commands.rs"]
pub mod agent_extensions_commands;
#[path = "../plugins/desktop/commands.rs"]
pub mod desktop_commands;
#[path = "../plugins/dev-services/commands.rs"]
pub mod dev_services_commands;
#[path = "../plugins/launch-settings/commands.rs"]
pub mod launch_settings_commands;
#[path = "../plugins/security-scans/commands.rs"]
pub mod security_scans_commands;

/// Compatibility action names belong to their plugin package's catalog entry.
pub fn phone_action_owner(action: &str) -> Option<String> {
    let catalog: Value = serde_json::from_str(CATALOG).ok()?;
    catalog["plugins"]
        .as_object()?
        .iter()
        .find(|(_, item)| {
            item["phone_actions"].as_array().is_some_and(|a| a.iter().any(|v| v.as_str() == Some(action)))
        })
        .map(|(id, _)| id.clone())
}

pub fn command_owner(command: &str) -> Option<String> {
    let catalog: Value = serde_json::from_str(CATALOG).ok()?;
    catalog["plugins"]
        .as_object()?
        .iter()
        .find(|(_, item)| {
            item["tui_commands"].as_array().is_some_and(|a| a.iter().any(|v| v.as_str() == Some(command)))
        })
        .map(|(id, _)| id.clone())
}
