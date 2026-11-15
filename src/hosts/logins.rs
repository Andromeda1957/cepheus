//! Compatibility facade for the bundled native plugin.

#[path = "../../plugins/login-sync/native.rs"]
mod implementation;
pub use implementation::*;
