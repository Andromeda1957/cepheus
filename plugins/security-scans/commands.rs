use crate::actions::{spec, Args, Spec};
pub const COMMANDS: &[Spec] = &[
    spec("scan", &[], "scan [host] [--scope TEXT]", "Codex Security scan in a scratch worktree", Args::Host),
    spec("scans", &[], "scans", "the saved security scans", Args::None),
];
