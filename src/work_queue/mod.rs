//! Durable owner-enabled work queues, source observations and fenced scheduled chores.
//! Payloads are private; only these metadata types enter fleet state.
use crate::protocol::Tool;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod chores;
pub mod refresh;
pub mod schedule;
pub mod sources;
pub mod store;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dispatch {
    pub kind: String,
    pub id: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceRef {
    #[serde(default)]
    pub source_id: String,
    pub item_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dependency {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub condition: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Spec {
    pub key: String,
    pub title: String,
    pub kind: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub provider: Option<Tool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default = "automatic")]
    pub placement: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub budget: Option<f64>,
    #[serde(default)]
    pub source: Option<SourceRef>,
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub env_clear: bool,
    #[serde(default)]
    pub env_allowlist: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub guard: Vec<String>,
    #[serde(default)]
    pub chore: Option<chores::Context>,
}
fn automatic() -> String {
    "auto".into()
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Info {
    pub kind: String,
    pub id: u64,
    pub title: String,
    pub state: String,
    pub priority: i32,
    pub created_at: u64,
    pub updated_at: u64,
    pub reason: String,
    pub dispatch: Option<Dispatch>,
    pub dependencies: Vec<Dependency>,
    pub source: Option<SourceRef>,
    pub budget: Option<f64>,
    pub cancelable: bool,
    pub provider: Option<Tool>,
    pub model: Option<String>,
    pub placement: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceInfo {
    pub id: String,
    pub owner: String,
    pub repository: String,
    pub host: Option<String>,
    pub enabled: bool,
    pub updated_at: Option<u64>,
    pub fresh: bool,
    pub error: Option<String>,
    /// Rows a deck's state leaves out (`work_state`); `source-list` has them all.
    #[serde(default)]
    pub omitted: usize,
}
/// A source-native saved conversation, opened only after an explicit UI choice.
/// Its endpoint comes from SourceInfo.host, never from provider-controlled metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskTarget {
    pub tool: String,
    pub conversation: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Task {
    #[serde(default)]
    pub source_id: String,
    pub id: String,
    pub title: String,
    pub state: String,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub next_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TaskTarget>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Schedule {
    pub cron: String,
    pub timezone: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimInfo {
    pub day: String,
    pub generation: u64,
    pub session: String,
    pub claimed_at: u64,
    pub stale_at: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChoreInfo {
    pub owner_live: bool,
    pub placement: String,
    pub guarded: bool,
    pub provider: Option<Tool>,
    pub id: String,
    pub owner: String,
    pub repository: String,
    pub title: String,
    pub registered: bool,
    pub enabled: bool,
    pub effective_enabled: bool,
    pub target: String,
    pub schedule: Schedule,
    pub next_run: Option<u64>,
    pub claim: Option<ClaimInfo>,
    pub outcome: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub enabled: bool,
    #[serde(default)]
    pub pause_pending: Vec<String>,
    #[serde(default)]
    pub pause_effective: bool,
    pub items: Vec<Info>,
    pub sources: Vec<SourceInfo>,
    pub tasks: Vec<Task>,
    pub chores: Vec<ChoreInfo>,
}

pub fn is_action(action: &str) -> bool {
    ["queue-", "source-", "chore-", "work-"].iter().any(|prefix| action.starts_with(prefix))
}
