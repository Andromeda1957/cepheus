use crate::actions::{spec, Args, Spec};
pub const COMMANDS: &[Spec] = &[
    spec(
        "install",
        &["add"],
        "install <owner/repo|url|path|npm:pkg|mcp:name=cmd> [--agents claude,codex,agy] [--host NAME|all] [--yes]",
        "install a plugin, MCP server or skill for the agents",
        Args::Install,
    ),
    spec(
        "uninstall",
        &["remove"],
        "uninstall <source> [--agents …] [--host NAME|all] [--yes]",
        "remove what install added",
        Args::Install,
    ),
    spec("skills", &[], "skills [sync]", "the deck's skills; sync links them on every host", Args::Words(&["sync"])),
];
