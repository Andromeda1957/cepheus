//! Opt-in provider account directories. Configuration is local to each host;
//! peers exchange profile IDs, never paths selected by another machine.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;

use crate::{
    config::Config,
    hosts::logins::{LoginLocation, LoginTool},
    protocol::Tool,
};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub rotate: bool,
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub provider: Tool,
    pub directory: PathBuf,
}

impl Settings {
    pub fn from_config(config: &Config) -> Result<Self> {
        if !config.plugin_enabled("provider-accounts") {
            return Ok(Self::default());
        }
        let settings = config
            .plugin_settings
            .get("provider-accounts")
            .map(|value| serde_json::from_value::<Self>(value.clone()))
            .transpose()
            .context("invalid provider-accounts settings")?
            .unwrap_or_default();
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.profiles.len() <= 64, "at most 64 account profiles are allowed");
        let mut directories: Vec<PathBuf> = Vec::new();
        for (id, profile) in &self.profiles {
            ensure!(
                !id.is_empty()
                    && id.len() <= 64
                    && id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
                "account profile IDs require 1..64 letters, digits, hyphens or underscores"
            );
            ensure!(
                matches!(profile.provider, Tool::Claude | Tool::Codex),
                "account directories are supported only for Claude and Codex"
            );
            ensure!(
                profile.directory.is_absolute()
                    && profile.directory.parent().is_some()
                    && profile.directory.to_str().is_some_and(|text| !text.contains(['\n', '\r', '\0']))
                    && profile.directory.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
                "account profile directory must be a normalized absolute directory"
            );
            let resolved = resolve_directory(&profile.directory)?;
            ensure!(
                directories.iter().all(|other| !resolved.starts_with(other) && !other.starts_with(&resolved)),
                "account profile directories must not overlap"
            );
            directories.push(resolved);
        }
        Ok(())
    }

    pub fn profile(&self, id: &str, provider: Tool) -> Result<&Profile> {
        let profile = self.profiles.get(id).context("account profile is not configured on this host")?;
        ensure!(profile.provider == provider, "account profile belongs to another provider");
        Ok(profile)
    }

    /// Stable IDs; selection and its persisted cursor belong to the scheduler.
    /// No default-account usage reading is assigned to a profile here.
    pub fn rotation_profiles(&self, provider: Tool) -> Vec<&str> {
        if !self.rotate {
            return Vec::new();
        }
        self.profiles.iter().filter(|(_, profile)| profile.provider == provider).map(|(id, _)| id.as_str()).collect()
    }
}

/// Resolve existing ancestors as well, so two configured aliases cannot share
/// credential storage. Missing profile roots remain valid until explicit use.
fn resolve_directory(directory: &Path) -> Result<PathBuf> {
    let mut ancestor = directory;
    let mut tail = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                ensure!(resolved.is_dir(), "account profile ancestor is not a directory");
                for name in tail.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tail.push(ancestor.file_name().context("account directory has no existing ancestor")?.to_owned());
                ancestor = ancestor.parent().context("account directory has no existing ancestor")?;
            }
            Err(_) => bail!("cannot resolve account profile directory"),
        }
    }
}

impl Profile {
    /// Read only identity-tagged profile observations. Untagged Codex rollout
    /// samples cannot prove account identity, so Codex remains unknown here.
    pub fn usage(&self, now: u64) -> Vec<crate::protocol::UsageWindow> {
        if self.provider != Tool::Claude {
            return Vec::new();
        }
        let Some(account) =
            crate::hosts::logins::state(self, self.login_tool(), now.saturating_mul(1000) as i64).account
        else {
            return Vec::new();
        };
        let path = self.directory.join(".agent-deck-usage.json"); // legacy-name
        let Ok(file) = std::fs::File::open(path) else {
            return Vec::new();
        };
        let mut bytes = Vec::new();
        if file.take(65537).read_to_end(&mut bytes).is_err() || bytes.len() > 65536 {
            return Vec::new();
        }
        let Ok(snapshot) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Vec::new();
        };
        if snapshot.get("account").and_then(serde_json::Value::as_str) != Some(&account)
            || snapshot.get("provider").and_then(serde_json::Value::as_str) != Some("claude")
        {
            return Vec::new();
        }
        crate::providers::monitor::claude_windows(&snapshot, now)
    }

    pub fn launch_env(&self) -> Vec<(String, String)> {
        let name = match self.provider {
            Tool::Claude => "CLAUDE_CONFIG_DIR",
            Tool::Codex => "CODEX_HOME",
            _ => return Vec::new(),
        };
        let directory = self.directory.to_string_lossy().into_owned();
        let mut env = vec![(name.into(), directory.clone())];
        env.extend(crate::paths::env_names("ACCOUNT_DIRECTORY").into_iter().map(|name| (name, directory.clone())));
        // Capture identity at launch; a later manual sign-in must not re-label
        // an old session's quota as another account's. Unknown identity stays
        // explicitly empty under both names.
        let account = crate::hosts::logins::state(self, self.login_tool(), crate::hosts::logins::now_ms())
            .account
            .unwrap_or_default();
        env.extend(crate::paths::env_names("ACCOUNT_ID").into_iter().map(|name| (name, account.clone())));
        env
    }

    pub fn login_tool(&self) -> LoginTool {
        match self.provider {
            Tool::Claude => LoginTool::Claude,
            Tool::Codex => LoginTool::Codex,
            _ => unreachable!("validated profile"),
        }
    }
}

impl LoginLocation for Profile {
    fn directory(&self, _tool: LoginTool) -> &Path {
        &self.directory
    }
    // An explicit CLAUDE_CONFIG_DIR keeps this file inside that root even if
    // the selected directory happens to be named `.claude`.
    fn claude_json(&self) -> PathBuf {
        self.directory.join(".claude.json")
    }
}

#[cfg(test)]
#[path = "../../src/tests/accounts.rs"]
mod tests;
