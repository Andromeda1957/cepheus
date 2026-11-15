//! Fleet attention metadata. Bodies and rules are private requests.
#[path = "../../plugins/supervisor/native_policy.rs"]
pub mod native_policy;
#[path = "../../plugins/supervisor/policy.rs"]
pub mod policy;
pub mod store;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Queued,
    Held,
    Pasted,
    Submitted,
    Unknown,
    Canceled,
}
/// Captured before the first send attempt and retained unchanged on every retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedTarget {
    pub spawned_ms: u64,
    pub pid: Option<u32>,
    pub conversation: Option<String>,
}
impl ExpectedTarget {
    pub fn from_session(s: &crate::protocol::SessionInfo) -> Option<Self> {
        (s.alive && s.tool.is_agent() && s.spawned_ms != 0 && s.pid.is_some_and(|pid| pid > 1)).then(|| Self {
            spawned_ms: s.spawned_ms,
            pid: s.pid,
            conversation: s.conversation.clone(),
        })
    }
    pub fn matches(&self, target: &store::Target) -> bool {
        self.spawned_ms != 0
            && self.pid.is_some_and(|pid| pid > 1)
            && self.spawned_ms == target.spawned_ms
            && self.pid == target.pid
            && self.conversation == target.conversation
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageInfo {
    #[serde(default)]
    pub host: Option<String>,
    pub id: String,
    pub session: u64,
    pub state: DeliveryState,
    pub created_at: u64,
    pub updated_at: u64,
    pub reason: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorInfo {
    #[serde(default)]
    pub audit: Vec<Audit>,
    pub enabled: bool,
    pub revision: u64,
    pub rules: usize,
    pub pending: Vec<String>,
}
/// Public attention state. Older peers' `inbox`, `stalls` and `away` are
/// ignored; see `Serialize` for why the first two are still written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Snapshot {
    #[serde(default, deserialize_with = "session_counts")]
    pub queued: BTreeMap<u64, usize>,
    pub messages: Vec<MessageInfo>,
    pub supervisor: SupervisorInfo,
}
impl Serialize for Snapshot {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        // The phone app before the inbox and stall warnings were removed
        // (2026-11-08) requires both keys, so they stay, always empty.
        const NONE: &[u8] = &[];
        let mut out = s.serialize_struct("Snapshot", 5)?;
        out.serialize_field("inbox", NONE)?;
        out.serialize_field("stalls", NONE)?;
        out.serialize_field("queued", &self.queued)?;
        out.serialize_field("messages", &self.messages)?;
        out.serialize_field("supervisor", &self.supervisor)?;
        out.end()
    }
}
/// Explicitly configured attention settings. None of these authorize an action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub retention_days: u64,
    pub permission_adapters: BTreeMap<String, std::path::PathBuf>,
    pub prompt_avoidance: bool,
}
impl Settings {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!((1..=90).contains(&self.retention_days), "attention retention bounds invalid");
        anyhow::ensure!(
            self.permission_adapters.len() <= 8
                && self
                    .permission_adapters
                    .iter()
                    .all(|(tool, path)| matches!(tool.as_str(), "claude" | "codex") && path.is_absolute()),
            "invalid trusted permission adapter"
        );
        Ok(())
    }
}
impl Default for Settings {
    fn default() -> Self {
        Self { retention_days: 7, permission_adapters: Default::default(), prompt_avoidance: false }
    }
}
pub fn is_action(action: &str) -> bool {
    action.starts_with("attention-")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Audit {
    pub session: u64,
    pub at: u64,
    pub category: String,
    pub rule: Option<String>,
}

// Internally tagged ServerMsg buffers objects as serde Content. Numeric map
// keys must therefore be decoded explicitly, including full fleet u64 IDs.
fn session_counts<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<u64, usize>, D::Error> {
    struct Counts;
    impl<'de> serde::de::Visitor<'de> for Counts {
        type Value = BTreeMap<u64, usize>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an object of decimal session IDs and counts")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut out = BTreeMap::new();
            while let Some((key, count)) = map.next_entry::<String, usize>()? {
                let id: u64 = key.parse().map_err(serde::de::Error::custom)?;
                if id.to_string() != key || out.insert(id, count).is_some() {
                    return Err(serde::de::Error::custom("invalid or duplicate session ID"));
                }
            }
            Ok(out)
        }
    }
    d.deserialize_map(Counts)
}
