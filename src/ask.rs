//! Compatibility facade for the bundled native plugin.

#[path = "../plugins/quick-ask/native.rs"]
mod implementation;
pub use implementation::*;
