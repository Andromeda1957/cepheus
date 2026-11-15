//! Compatibility facade for installed desktop and repository integration adapters.

#[path = "../plugins/desktop/native.rs"]
mod implementation;
pub use crate::extensions::conventions::containers;
pub use implementation::*;
