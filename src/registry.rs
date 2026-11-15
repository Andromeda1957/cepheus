//! The durable session list, so ended sessions can be reopened after the
//! service (or the computer) restarts.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::protocol::Tool;
use crate::util;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    /// The model and effort it was started with, which the phone's Clear
    /// starts its fresh session with (`service/native_clear.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub launch_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub id: u64,
    pub tool: Tool,
    pub conversation: Option<String>,
    pub title: String,
    /// The owner's name for the chat (`:rename`); absent from older lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub created_at: u64,
    #[serde(default)]
    pub ended_at: Option<u64>,
    /// Start it again with its conversation when the service next starts: the service stopped under it (a
    /// signal, a crash, the computer restarting) rather than the owner stopping it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub resume: bool,
    /// Its turn was running when last saved: it is told to carry on once
    /// started again.
    #[serde(default, skip_serializing_if = "is_false")]
    pub working: bool,
    /// A cold-start continuation is still awaiting delivery/progress.
    #[serde(default, skip_serializing_if = "is_false")]
    pub boot_pending: bool,
    /// Explicit owner interruption survives an abrupt shutdown.
    #[serde(default, skip_serializing_if = "is_false")]
    pub owner_paused: bool,
    /// Captured input and receipt boundaries, independent of any living PTY.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<crate::service::recovery::ClientRecovery>,
    /// The directory it started in when that is not the start directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// A scratch terminal's command; empty: the owner's shell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// What the deck runs it for (`SessionInfo::job`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Entry {
    pub fn recoverable(&self) -> bool {
        !self.launch_pending
            && self.tool.is_agent()
            && (self.conversation.is_some()
                || (!self.working && !self.boot_pending && self.recovery.as_ref().is_none_or(|r| r.pending.is_empty())))
    }

    /// A session as it is recorded when it starts.
    pub fn started(id: u64, tool: Tool, conversation: Option<String>, title: String, created_at: u64) -> Entry {
        Entry {
            start_model: None,
            start_effort: None,
            launch_key: None,
            launch_fingerprint: None,
            launch_pending: false,
            account_root: None,
            account: None,
            id,
            tool,
            conversation,
            title,
            name: None,
            created_at,
            ended_at: None,
            resume: false,
            working: false,
            boot_pending: false,
            owner_paused: false,
            recovery: None,
            cwd: None,
            command: Vec::new(),
            job: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Registry {
    pub next_id: u64,
    pub sessions: Vec<Entry>,
}

impl Registry {
    pub fn load(path: &Path) -> Registry {
        let mut reg: Registry = match fs::read(path).map(|bytes| serde_json::from_slice(&bytes)) {
            Err(_) => Registry::default(),
            Ok(Ok(reg)) => reg,
            Ok(Err(_)) => {
                // Keep an unreadable list for inspection instead of overwriting it.
                let _ = fs::rename(path, path.with_extension(format!("corrupt-{}", util::now_secs())));
                Registry::default()
            }
        };
        let max = reg.sessions.iter().map(|e| e.id).max().unwrap_or(0);
        reg.next_id = reg.next_id.max(max + 1).max(1);
        reg
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        util::write_atomic(path, &self.to_bytes()?)
    }

    /// The file's contents, for the service's registry writer.
    pub fn to_bytes(&self) -> std::io::Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).map_err(std::io::Error::other)
    }
}

#[cfg(test)]
#[path = "tests/registry.rs"]
mod tests;
