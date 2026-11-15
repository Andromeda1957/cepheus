//! Hosted hook entrypoints. Claude/Codex event forwarding is silent and neutral;
//! native permission replies and advisory rewrites have their own bounded contracts.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::Value;

use crate::paths;
use crate::protocol::{write_msg, ClientMsg, ClientRole, Tool, PROTOCOL_VERSION};

#[path = "claude_input_hook.rs"]
mod owner_input;
#[path = "permission_hook.rs"]
mod permission_hook;
#[path = "prompt_avoidance.rs"]
mod prompt_avoidance;

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("permission-request") => return permission_hook::run(),
        Some("prompt-avoidance") => return prompt_avoidance::run(),
        Some("owner-input") => return owner_input::run(&args[1..]),
        _ => {}
    }
    let tool = match args.first().map(String::as_str) {
        Some("claude") => Tool::Claude,
        Some("codex") => Tool::Codex,
        _ => return 0,
    };
    let payload: Value = match tool {
        Tool::Claude => {
            let mut buf = Vec::new();
            let _ = std::io::stdin().take(1 << 20).read_to_end(&mut buf);
            let mut payload = serde_json::from_slice(&buf).unwrap_or(Value::Null);
            // A sub-agent's whole final reply rides on SubagentStop; the
            // service reads none of it.
            if let Some(fields) = payload.as_object_mut() {
                fields.remove("last_assistant_message");
            }
            payload
        }
        Tool::Codex => args.get(1).and_then(|a| serde_json::from_str(a).ok()).unwrap_or(Value::Null),
        // The deck gives Antigravity, Grok and opencode no hook.
        Tool::Antigravity | Tool::Grok | Tool::OpenCode | Tool::Shell | Tool::Dynamic(_) => return 0,
    };
    let Some(session) = paths::env_var("SESSION").and_then(|s| s.to_str()?.parse::<u64>().ok()) else {
        return 0;
    };
    if let Ok(mut stream) = UnixStream::connect(paths::socket_path()) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let _ = write_msg(&mut stream, &ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Hook });
        let _ = write_msg(&mut stream, &ClientMsg::Hook { session, tool, payload });
    }
    0
}
