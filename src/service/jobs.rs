//! Compatibility import for the bundled plugin; implementation lives with its package.

#[path = "../../plugins/security-scans/native.rs"]
mod implementation;
pub use implementation::*;
