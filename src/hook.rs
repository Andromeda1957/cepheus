//! Compatibility facade for the bundled provider adapters.

#[path = "../plugins/providers/hook.rs"]
mod implementation;
pub use implementation::*;
