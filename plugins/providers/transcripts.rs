//! Native transcript sources, with PTY fallback for additional providers.
use crate::protocol::{SessionInfo, Tool};
use crate::remote::chat::{Dirs, Source};
use crate::{antigravity, claude, codex};
use std::fs;

pub fn source(info: &SessionInfo, dirs: &Dirs) -> Option<Source> {
    let conv = info.conversation.as_deref().filter(|c| crate::service::valid_conversation_id(c));
    match info.tool {
        Tool::Claude => {
            let claude_root = info.account_root.as_ref().map(std::path::Path::new).unwrap_or(&dirs.claude);
            let id = conv?;
            let path = claude::transcript_path(claude_root, &dirs.start_dir.to_string_lossy(), id);
            if path.is_file() {
                return Some(Source::Claude(path));
            }
            // A session moved in from another folder keeps its own project.
            let name = format!("{id}.jsonl");
            fs::read_dir(claude_root.join("projects"))
                .ok()?
                .flatten()
                .map(|e| e.path().join(&name))
                .find(|p| p.is_file())
                .map(Source::Claude)
        }
        Tool::Codex => Some(
            conv.and_then(|id| {
                codex::find_rollout(info.account_root.as_ref().map(std::path::Path::new).unwrap_or(&dirs.codex), id)
            })
            .map_or(Source::Screen, Source::Codex),
        ),
        Tool::OpenCode => conv.map(|c| Source::OpenCode(c.to_string())),
        // Until its first prompt names the conversation, agy's screen is all
        // there is to show.
        Tool::Antigravity => Some(
            conv.map(|id| antigravity::conversation_db(&dirs.antigravity, id))
                .filter(|db| db.is_file())
                .map_or(Source::Screen, Source::Antigravity),
        ),
        Tool::Dynamic(_) => Some(
            crate::providers::get(info.tool)
                .and_then(|provider| provider.chat_snapshot)
                .filter(|path| path.is_file())
                .map_or(Source::Screen, Source::Snapshot),
        ),
        Tool::Grok | Tool::Shell => Some(Source::Screen),
    }
}
