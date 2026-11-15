//! Filesystem locations used by the service, the TUI and the helper commands.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The name of this program's runtime, state, config and data folders.
const NAME: &str = "cepheus";

/// What those folders, and the environment variables below, were called
/// before the program was renamed Cepheus on 2026-11-05. A running service
/// hands over to a new build in place, its sessions keep the variables they
/// were started with, and an existing install keeps its folders.
const LEGACY_NAME: &str = "agent-deck"; // legacy-name
const LEGACY_ENV_PREFIX: &str = "AGENT_DECK_"; // legacy-name

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The non-empty variable `CEPHEUS_<suffix>`, else its name from before the
/// rename.
pub fn env_var(suffix: &str) -> Option<OsString> {
    [format!("CEPHEUS_{suffix}"), format!("{LEGACY_ENV_PREFIX}{suffix}")]
        .into_iter()
        .find_map(|name| std::env::var_os(name).filter(|v| !v.is_empty()))
}

/// Both names of the variable `CEPHEUS_<suffix>`, for a child that must get
/// neither.
pub fn env_names(suffix: &str) -> [String; 2] {
    [format!("CEPHEUS_{suffix}"), format!("{LEGACY_ENV_PREFIX}{suffix}")]
}

fn named_env_path(suffix: &str) -> Option<PathBuf> {
    env_var(suffix).map(PathBuf::from)
}

/// `base/cepheus`, unless only an install from before the rename's folder
/// exists there.
fn named_dir(base: &Path) -> PathBuf {
    let dir = base.join(NAME);
    let legacy = base.join(LEGACY_NAME);
    if !dir.exists() && legacy.is_dir() {
        legacy
    } else {
        dir
    }
}

/// Per-user runtime directory holding the socket and the single-instance lock.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = env_path("XDG_RUNTIME_DIR") {
        return named_dir(&dir);
    }
    // SAFETY: getuid has no preconditions.
    named_dir(Path::new(&format!("/run/user/{}", unsafe { libc::getuid() })))
}

pub fn socket_path() -> PathBuf {
    named_env_path("SOCKET").unwrap_or_else(|| runtime_dir().join("deck.sock"))
}

/// Downloads the deck can fetch again, such as Dot's pet sheet.
pub fn cache_dir() -> PathBuf {
    named_dir(&env_path("XDG_CACHE_HOME").unwrap_or_else(|| home().join(".cache")))
}

/// Durable state: the session registry, Claude usage snapshot and service log.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = named_env_path("STATE_DIR") {
        return dir;
    }
    named_dir(&env_path("XDG_STATE_HOME").unwrap_or_else(|| home().join(".local/state")))
}

/// Where every agent client the deck starts gets its own temporary folder
/// (`client_temp.rs`): `CEPHEUS_TEMP_DIR`, else `/var/tmp/cepheus-<uid>`, on
/// disk rather than in the RAM most systems give `/tmp`.
pub fn client_temp_root() -> PathBuf {
    if let Some(dir) = named_env_path("TEMP_DIR") {
        return dir;
    }
    // A test binary's clients get theirs inside its own scratch.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("cepheus-clients-{}", std::process::id()));
    }
    // SAFETY: getuid has no preconditions.
    PathBuf::from(format!("/var/tmp/{NAME}-{}", unsafe { libc::getuid() }))
}

pub fn config_path() -> PathBuf {
    if let Some(path) = named_env_path("CONFIG") {
        return path;
    }
    named_dir(&env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config"))).join("config.json")
}

pub fn claude_dir() -> PathBuf {
    env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home().join(".claude"))
}

pub fn codex_dir() -> PathBuf {
    env_path("CODEX_HOME").unwrap_or_else(|| home().join(".codex"))
}

/// Antigravity CLI's state (`history.jsonl`, `conversations/`).
pub fn antigravity_dir() -> PathBuf {
    named_env_path("ANTIGRAVITY_DIR").unwrap_or_else(|| home().join(".gemini/antigravity-cli"))
}

/// Grok CLI's home (`sessions/`): `GROK_HOME`, else `~/.grok`.
pub fn grok_home() -> PathBuf {
    env_path("GROK_HOME").unwrap_or_else(|| home().join(".grok"))
}

/// opencode's session database, under the XDG data directory.
pub fn opencode_db() -> PathBuf {
    env_path("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local/share")).join("opencode/opencode.db")
}

pub fn usage_file() -> PathBuf {
    state_dir().join("claude-usage.json")
}

/// The order Super+↑ and Super+↓ gave the deck's sessions, as session ids.
pub fn order_file() -> PathBuf {
    state_dir().join("order.json")
}

/// The lines entered at the deck's `:` command line, for `↑` and `↓` there.
pub fn command_history_file() -> PathBuf {
    state_dir().join("command-history.json")
}

/// Files the phone attached to messages, a folder per session.
pub fn uploads_dir() -> PathBuf {
    state_dir().join("uploads")
}

/// What the deck keeps for the owner rather than for itself: security
/// scans, its skills, installed sources.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = named_env_path("DATA_DIR") {
        return dir;
    }
    named_dir(&env_path("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local/share")))
}

/// The folder of the config file: `init.lua`, `lua/` and `plugins/`.
pub fn config_dir() -> PathBuf {
    config_path().parent().map(PathBuf::from).unwrap_or_else(|| named_dir(&home().join(".config")))
}
