//! Compatibility facade for the bundled native plugin.

#[path = "../plugins/agent-extensions/skills.rs"]
mod implementation;
pub use implementation::*;
