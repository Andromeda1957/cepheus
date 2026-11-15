//! Compatibility facade for the bundled provider adapters.

#[path = "../plugins/providers/activity.rs"]
mod implementation;
pub use implementation::*;
