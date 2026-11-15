//! Bundled launch adapters. Keep CLI quirks here rather than in session hosting.
use super::Launch;
use crate::{
    codex,
    config::Config,
    other,
    protocol::Tool,
    service::{valid_effort, valid_model_name, StartModel},
    util,
};
use anyhow::Result;

pub fn model_flags(tool: Tool, start: &StartModel) -> Result<Vec<String>> {
    let model = start.model.as_deref().filter(|m| !m.is_empty());
    let effort = start.effort.as_deref().filter(|e| !e.is_empty());
    if let Some(m) = model {
        anyhow::ensure!(valid_model_name(m), "invalid model name");
    }
    if let Some(e) = effort {
        anyhow::ensure!(valid_effort(e), "invalid effort");
    }
    let mut flags = Vec::new();
    let mut push = |flag: &str, value: &str| {
        flags.push(flag.to_string());
        flags.push(value.to_string());
    };
    match tool {
        Tool::Claude | Tool::Antigravity => {
            if let Some(m) = model {
                push("--model", m);
            }
            if let Some(e) = effort {
                push("--effort", e);
            }
        }
        Tool::Codex | Tool::Shell => {}
        Tool::Dynamic(_) => return super::configured_model_flags(tool, start),
        Tool::Grok => {
            if let Some(m) = model {
                push("-m", m);
            }
            if let Some(e) = effort {
                push("--reasoning-effort", e);
            }
        }
        Tool::OpenCode => {
            if let Some(m) = model {
                let full = match effort {
                    Some(e) if !m.contains('#') => format!("{m}#{e}"),
                    _ => m.to_string(),
                };
                push("-m", &full);
            }
        }
    }
    Ok(flags)
}

pub fn launch(
    cfg: &Config,
    exe: &std::path::Path,
    codex_dir: &std::path::Path,
    tool: Tool,
    resume: Option<&str>,
) -> Result<Launch> {
    launch_with_pre_tool_use(cfg, exe, codex_dir, tool, resume, None)
}

/// Optional provider hook composition. Repository ownership and enablement
/// belong to the caller; None preserves the existing launch exactly.
pub fn launch_with_pre_tool_use(
    cfg: &Config,
    exe: &std::path::Path,
    codex_dir: &std::path::Path,
    tool: Tool,
    resume: Option<&str>,
    pre_tool_use: Option<&std::path::Path>,
) -> Result<Launch> {
    launch_with_hooks(cfg, exe, codex_dir, tool, resume, pre_tool_use, None)
}

/// Executable-loader inputs cleared both at hosted CLI startup and in the
/// higher-precedence native hook settings, preventing workspace reinjection.
pub const NATIVE_HOOK_ENV_PINS: &[&str] =
    &["LD_PRELOAD", "LD_LIBRARY_PATH", "LD_AUDIT", "GCONV_PATH", "LD_ORIGIN_PATH", "LD_PROFILE", "LD_DEBUG_OUTPUT"];

/// Qualified native Rust hook process, pinned by core for this hosted spawn.
#[derive(Clone, Copy)]
pub struct PermissionHook<'a> {
    pub interpreter: &'a std::path::Path,
    pub helper: &'a std::path::Path,
}

impl PermissionHook<'_> {
    pub fn argv(self) -> Vec<String> {
        vec![self.helper.to_string_lossy().into_owned(), "hook".into(), "permission-request".into()]
    }
}

/// A native PermissionRequest command returns a single structured answer. It
/// never selects a terminal option and never grants standing permissions.
/// Enablement, installed helper trust and per-spawn credentials belong to core.
pub fn launch_with_hooks(
    cfg: &Config,
    exe: &std::path::Path,
    codex_dir: &std::path::Path,
    tool: Tool,
    resume: Option<&str>,
    pre_tool_use: Option<&std::path::Path>,
    permission_request: Option<PermissionHook<'_>>,
) -> Result<Launch> {
    for helper in [pre_tool_use, permission_request.map(|h| h.interpreter), permission_request.map(|h| h.helper)]
        .into_iter()
        .flatten()
    {
        anyhow::ensure!(
            helper.is_absolute() && helper.to_str().is_some_and(|s| !s.chars().any(char::is_control)),
            "provider hook helper must be an absolute path"
        );
    }
    if let Some(hook) = permission_request {
        anyhow::ensure!(
            [hook.interpreter, hook.helper].iter().all(|path| !path.to_string_lossy().contains("${")),
            "native permission hook paths must not contain provider templates"
        );
    }
    if tool != Tool::Shell && cfg.plugins.get(&format!("provider-{}", tool.key())) == Some(&false) {
        anyhow::bail!("Provider plugin {} is disabled", tool.key());
    }
    let exe_text = exe.to_string_lossy().into_owned();
    let (argv, conversation) = match tool {
        Tool::Claude => {
            let mut argv = cfg.claude_command.clone();
            let conversation = match resume {
                Some(id) => {
                    argv.extend(["--resume".to_string(), id.to_string()]);
                    id.to_string()
                }
                None => {
                    let id = util::uuid_v4()?;
                    argv.extend(["--session-id".to_string(), id.clone()]);
                    id
                }
            };
            let mut settings = serde_json::json!({});
            if cfg.claude_hooks {
                let exe = util::shell_quote(&exe_text);
                let hook =
                    serde_json::json!([{"hooks": [{"type": "command", "command": format!("{exe} hook claude")}]}]);
                // Stop ends the main agent's turn. It and SubagentStop list
                // the running sub-agents; SubagentStart names one as it starts.
                settings = serde_json::json!({
                    "statusLine": {"type": "command", "command": format!("{exe} statusline"), "padding": 0},
                    "hooks": {"Notification": hook, "Stop": hook, "SubagentStart": hook, "SubagentStop": hook}
                });
            }
            if let Some(helper) = pre_tool_use {
                settings["hooks"]["PreToolUse"] = serde_json::json!([{
                    "matcher": "Bash", "hooks": [{
                        "type": "command",
                        "command": format!("{} hook prompt-avoidance", util::shell_quote(&helper.to_string_lossy()))
                    }]
                }]);
            }
            if let Some(hook) = permission_request {
                let argv = hook.argv();
                // Measured Claude exec form bypasses shell startup; the path
                // check above excludes its template expansion. Flag settings
                // override repo loader environment without shell startup.
                for key in NATIVE_HOOK_ENV_PINS {
                    settings["env"][*key] = serde_json::json!("");
                }
                settings["hooks"]["PermissionRequest"] = serde_json::json!([{
                    "matcher": "Read", "hooks": [{
                        "type": "command", "command": argv[0], "args": argv[1..], "timeout": 3
                    }]
                }]);
            }
            if settings.as_object().is_some_and(|settings| !settings.is_empty()) {
                argv.extend(["--settings".to_string(), settings.to_string()]);
            }
            (argv, Some(conversation))
        }
        Tool::Codex => {
            let mut argv = cfg.codex_command.clone();
            // Codex's shared background server keeps a thread's writer lock for about 30 minutes after its last
            // client leaves; an embedded Codex then cannot write the conversation and shows "This conversation
            // is open in another app". Attach to the server instead, as the owner's own Codex did, and follow
            // the turn in its rollout.
            let attached = resume.is_some_and(|id| codex::thread_locked(codex_dir, id));
            if cfg.codex_notify && !attached {
                let notify = serde_json::to_string(&[exe_text.as_str(), "hook", "codex"])?;
                // A `-c` override makes Codex run without the shared
                // server and warn about it on every start; `--no-daemon`
                // asks for that, without the warning.
                argv.splice(1..1, ["--no-daemon".to_string(), "-c".to_string(), format!("notify={notify}")]);
            }
            if let Some(id) = resume {
                argv.extend(["resume".to_string(), id.to_string()]);
            }
            return Ok(Launch { argv, conversation: resume.map(str::to_string), attached });
        }
        Tool::Antigravity => {
            let mut argv = cfg.antigravity_command.clone();
            if let Some(id) = resume {
                argv.extend(["--conversation".to_string(), id.to_string()]);
            }
            (argv, resume.map(str::to_string))
        }
        // Grok and opencode take the id of a new conversation, so the
        // deck knows it from the start and a reopen resumes it.
        Tool::Grok => {
            let mut argv = cfg.grok_command.clone();
            let conversation = match resume {
                Some(id) => {
                    argv.extend(["--resume".to_string(), id.to_string()]);
                    id.to_string()
                }
                None => {
                    let id = util::uuid_v4()?;
                    argv.extend(["--session-id".to_string(), id.clone()]);
                    id
                }
            };
            (argv, Some(conversation))
        }
        Tool::OpenCode => {
            let mut argv = cfg.opencode_command.clone();
            let id = match resume {
                Some(id) => id.to_string(),
                None => other::new_opencode_session_id()?,
            };
            argv.extend(["--session".to_string(), id.clone()]);
            (argv, Some(id))
        }
        Tool::Shell => unreachable!("launch_for starts terminals"),
        Tool::Dynamic(_) => return super::configured_launch(cfg, tool, resume),
    };
    Ok(Launch { argv, conversation, attached: false })
}

#[cfg(test)]
#[path = "../../src/tests/provider_native.rs"]
mod tests;
