//! Compatibility facade for the bundled native plugin.

#[path = "../../plugins/host-bootstrap/native.rs"]
mod implementation;
pub use implementation::*;
