//! Compatibility facade for the bundled native plugin.

#[path = "../../plugins/git-mirror/native.rs"]
mod implementation;
pub use implementation::*;
