//! A project's own naming conventions (`config::Conventions`): worktree task
//! names, task containers, review recognition, and the repository hooks that
//! take a deck behavior over.
use serde_json::Value;
use std::time::Duration;

use crate::config::RepositoryHook;
use crate::external::run_with_timeout;
use crate::protocol::{ContainerInfo, Tool};
use std::collections::HashMap;
use std::path::Path;

/// A repository's `.claude/settings.json` larger than this is not read.
const SETTINGS_LIMIT: u64 = 256 * 1024;
/// How many directories, from the session's own upwards, may hold the
/// repository a hook belongs to.
const MAX_ANCESTORS: usize = 128;

/// Containers whose `label` names a task, keyed by that task; none when
/// `label` is empty.
pub fn parse_podman_ps(json: &[u8], label: &str) -> HashMap<String, Vec<ContainerInfo>> {
    let mut out: HashMap<String, Vec<ContainerInfo>> = HashMap::new();
    if label.is_empty() {
        return out;
    }
    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(json) else { return out };
    for item in items {
        let Some(task) = item.get("Labels").and_then(|l| l.get(label)).and_then(Value::as_str) else {
            continue;
        };
        let name = match item.get("Names") {
            Some(Value::Array(names)) => names.first().and_then(Value::as_str).unwrap_or("").to_string(),
            Some(Value::String(name)) => name.clone(),
            _ => String::new(),
        };
        let field = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        out.entry(task.to_string()).or_default().push(ContainerInfo {
            name,
            state: field("State"),
            status: field("Status"),
        });
    }
    out
}

/// Podman's containers by the task `label` names; podman is not run when
/// `label` is empty.
pub fn containers(label: &str) -> HashMap<String, Vec<ContainerInfo>> {
    if label.is_empty() {
        return HashMap::new();
    }
    run_with_timeout("podman", &["ps", "--format", "json"], Duration::from_secs(5))
        .map(|out| parse_podman_ps(&out, label))
        .unwrap_or_default()
}

/// A linked worktree's task: its folder name after `marker`, or the whole
/// folder name when `marker` is empty. A folder without the marker has none.
pub fn task_of(path: &str, marker: &str) -> Option<String> {
    let name = Path::new(path).file_name()?.to_str()?;
    let task = if marker.is_empty() { name } else { name.split_once(marker)?.1 };
    (!task.is_empty()).then(|| task.to_string())
}

/// Whether a descendant of `pid` has an argument ending with one of
/// `commands`; an empty entry names nothing.
pub fn review(pid: u32, processes: &HashMap<u32, crate::procs::ProcInfo>, commands: &[String]) -> bool {
    let commands: Vec<&str> = commands.iter().map(String::as_str).filter(|c| !c.is_empty()).collect();
    if commands.is_empty() {
        return false;
    }
    crate::procs::descendants(processes, pid).into_iter().any(|child| {
        crate::procs::cmdline(child).iter().any(|arg| commands.iter().any(|command| arg.ends_with(*command)))
    })
}

/// A session's review state, and the containers of its worktree's task.
pub fn enrich(
    facts: &mut crate::monitor::SessionFacts,
    pid: Option<u32>,
    processes: &HashMap<u32, crate::procs::ProcInfo>,
    containers: &HashMap<String, Vec<ContainerInfo>>,
    review_commands: &[String],
) {
    facts.review = pid.is_some_and(|pid| review(pid, processes, review_commands));
    if let Some(task) = facts.worktree.as_ref().and_then(|w| w.task.as_ref()) {
        facts.containers = containers.get(task).cloned().unwrap_or_default();
    }
}

/// Whether the repository a Claude session works in has its own hooks for
/// what `hook` describes (`config::RepositoryHook`), so the deck leaves that
/// to them: in `cwd`, resolved, or one of its parents (at most
/// `MAX_ANCESTORS`), every one of `hook.files` exists and
/// `.claude/settings.json` has every one of `hook.events`. No hook, another
/// tool, a hook that names no event or command, or a directory that does not
/// resolve: not owned.
pub fn repository_owns(hook: Option<&RepositoryHook>, tool: Tool, cwd: &Path) -> bool {
    let Some(hook) = hook else { return false };
    if tool != Tool::Claude || hook.events.is_empty() || hook.command.is_empty() {
        return false;
    }
    let Ok(cwd) = cwd.canonicalize() else { return false };
    cwd.ancestors().take(MAX_ANCESTORS).any(|root| {
        hook.files.iter().all(|file| root.join(file).is_file())
            && claude_settings(root).is_some_and(|settings| has_hooks(&settings, hook))
    })
}

/// `<root>/.claude/settings.json`, read only when it is a regular file of at
/// most `SETTINGS_LIMIT` bytes: a symbolic link is not followed and a pipe
/// is not waited on.
fn claude_settings(root: &Path) -> Option<Value> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(".claude/settings.json"))
        .ok()?;
    if !file.metadata().is_ok_and(|m| m.is_file() && m.len() <= SETTINGS_LIMIT) {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(SETTINGS_LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > SETTINGS_LIMIT {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// Every one of `hook.events` has a group, whose matcher names
/// `hook.matcher` among its `|`-separated tools when one is given, with a
/// command hook whose command contains `hook.command`.
fn has_hooks(settings: &Value, hook: &RepositoryHook) -> bool {
    hook.events.iter().all(|event| {
        settings["hooks"][event.as_str()].as_array().is_some_and(|groups| {
            groups.iter().any(|group| {
                let matched = hook.matcher.as_deref().is_none_or(|wanted| {
                    group["matcher"].as_str().is_some_and(|names| names.split('|').any(|name| name == wanted))
                });
                matched
                    && group["hooks"].as_array().is_some_and(|hooks| {
                        hooks.iter().any(|entry| {
                            entry["type"] == "command"
                                && entry["command"].as_str().is_some_and(|command| command.contains(&hook.command))
                        })
                    })
            })
        })
    })
}
