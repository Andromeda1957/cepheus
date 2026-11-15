use crate::actions::{spec, Args, Spec};
pub const COMMANDS: &[Spec] = &[spec(
    "ask",
    &[],
    "ask [/general|/web|/code|/claude|/codex|/agy] [question]",
    "a quick question, no session",
    Args::Text,
)];
