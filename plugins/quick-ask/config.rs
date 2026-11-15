use serde::{Deserialize, Serialize};
/// Which program answers a quick question (`ask.rs`): ordinary questions go to `general`, lookups to `web`,
/// questions about the code to `code`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct AskConfig {
    pub general: AskRoute,
    pub web: AskRoute,
    pub code: AskRoute,
    /// Seconds an answer may take.
    pub timeout: u64,
    /// More words that mark a question about the code, such as a project's
    /// own names.
    pub code_words: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct AskRoute {
    /// `agy`, `claude` or `codex`.
    pub tool: String,
    /// The program's own model name; empty: its default.
    pub model: String,
}

impl Default for AskRoute {
    fn default() -> Self {
        AskRoute { tool: "agy".into(), model: "gemini-3.8-flash-high".into() }
    }
}

impl Default for AskConfig {
    fn default() -> Self {
        AskConfig {
            general: AskRoute::default(),
            web: AskRoute::default(),
            code: AskRoute { tool: "claude".into(), model: "sonnet".into() },
            timeout: 180,
            code_words: Vec::new(),
        }
    }
}
