//! Compatibility facade for the bundled native plugin.

#[path = "../plugins/ai-titles/native.rs"]
mod implementation;
pub use implementation::*;
