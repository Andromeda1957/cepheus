//! Compatibility facade for the bundled provider adapters.

#[path = "../plugins/providers/statusline.rs"]
mod implementation;
pub use implementation::*;
