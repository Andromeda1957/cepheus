//! Compatibility facade for the bundled native plugin.

#[path = "../plugins/agent-extensions/native.rs"]
mod implementation;
pub use implementation::*;
