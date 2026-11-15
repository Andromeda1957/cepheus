//! Compatibility facade for the bundled provider adapters.

#[path = "../plugins/providers/codexpick.rs"]
mod implementation;
pub use implementation::*;
