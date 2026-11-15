//! The environment the service runs in and passes to hosted programs.
//!
//! Agent tool environments may override colors, terminal type, loaders and provider configuration paths. When
//! started inside an agent, the service uses the user's login-session environment from the user manager,
//! retaining only its own socket, state and configuration addresses. Starts from a terminal or unit retain
//! their supplied environment.

use std::ffi::{OsStr, OsString};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

/// Variables an agent sets for the commands it runs: Claude Code's tools
/// (`CLAUDECODE`, `AI_AGENT`), Codex's (`CODEX_THREAD_ID`, `CODEX_CI`) and
/// the deck's hosted programs (`CEPHEUS_SESSION`).
const AGENT_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "AI_AGENT",
    "CODEX_THREAD_ID",
    "CODEX_CI",
    "CEPHEUS_SESSION",
    "AGENT_DECK_SESSION", // legacy-name
];

/// The deck's own addresses (`paths.rs`), kept from the starting environment.
const DECK_ADDRESSES: &[&str] = &["CONFIG", "STATE_DIR", "SOCKET", "DATA_DIR", "ANTIGRAVITY_DIR", "START_DIR"];

/// The error when the owner's environment could not be taken at a handover,
/// for the service to report once it runs.
static UNSETTLED: OnceLock<String> = OnceLock::new();

/// The first agent-session variable set among `vars`, if any.
pub fn agent_marker<'a>(vars: impl IntoIterator<Item = (&'a OsStr, &'a OsStr)>) -> Option<&'static str> {
    let set: Vec<&OsStr> = vars.into_iter().filter(|(_, value)| !value.is_empty()).map(|(name, _)| name).collect();
    AGENT_MARKERS.iter().copied().find(|marker| set.iter().any(|name| *name == OsStr::new(marker)))
}

/// Whether `name` is one of the deck's own address variables.
fn deck_address(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else { return false };
    let suffix = name.strip_prefix("CEPHEUS_").or_else(|| name.strip_prefix("AGENT_DECK_")); // legacy-name
    suffix.is_some_and(|suffix| DECK_ADDRESSES.contains(&suffix))
}

/// The service's environment: the owner's `session` environment, with the
/// deck's own addresses from `current`.
pub fn owner_environment(
    current: &[(OsString, OsString)],
    session: Vec<(OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut out: Vec<(OsString, OsString)> = session.into_iter().filter(|(name, _)| !deck_address(name)).collect();
    out.extend(current.iter().filter(|(name, _)| deck_address(name)).cloned());
    out
}

/// The `Environment` property `busctl --json=short` printed for the user
/// manager.
pub fn parse_manager_environment(text: &str) -> Result<Vec<(OsString, OsString)>> {
    let value: serde_json::Value = serde_json::from_str(text).context("the user manager's environment is not JSON")?;
    if value["type"] != "as" {
        bail!("the user manager's environment is not a list of strings");
    }
    let entries = value["data"].as_array().context("the user manager's environment has no entries")?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.as_str().context("the user manager's environment holds a non-string entry")?;
        let (name, value) =
            entry.split_once('=').context("the user manager's environment holds an entry without '='")?;
        if name.is_empty() || entry.contains('\0') {
            bail!("the user manager's environment holds an invalid entry");
        }
        out.push((OsString::from(name), OsString::from(value)));
    }
    Ok(out)
}

/// The owner's login session environment, from the user manager.
fn session_environment() -> Result<Vec<(OsString, OsString)>> {
    let output = Command::new("busctl")
        .args([
            "--user",
            "--json=short",
            "get-property",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "Environment",
        ])
        .stdin(Stdio::null())
        .output()
        .context("running busctl")?;
    if !output.status.success() {
        bail!("busctl: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    parse_manager_environment(&String::from_utf8_lossy(&output.stdout))
}

/// Run this process in the owner's environment when it was started inside an
/// agent session. Call first in `main`, while the process has one thread.
pub fn settle() -> Result<()> {
    let current: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let Some(marker) = agent_marker(current.iter().map(|(name, value)| (name.as_os_str(), value.as_os_str()))) else {
        return Ok(());
    };
    let session = session_environment().with_context(|| {
        format!(
            "started inside an agent session ({marker} is set), and the owner's session environment could not be \
             read from the user manager; hosted programs would inherit the agent's environment. Start the deck \
             from your own terminal"
        )
    })?;
    for (name, _) in &current {
        std::env::remove_var(name);
    }
    for (name, value) in owner_environment(&current, session) {
        std::env::set_var(name, value);
    }
    Ok(())
}

/// Keep the error from a handover, where the process must carry on with every
/// session it took over.
pub fn record_unsettled(error: &anyhow::Error) {
    let _ = UNSETTLED.set(format!("The service runs in an agent session's environment: {error:#}"));
}

pub fn unsettled() -> Option<&'static str> {
    UNSETTLED.get().map(String::as_str)
}

#[cfg(test)]
#[path = "tests/owner_env.rs"]
mod tests;
