//! Receiver-local Claude Enter policy. Nothing crosses the fleet wire;
//! these paths and the owner's other bindings stay on their own host.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};

use crate::{config::Config, hosts::logins::LoginPaths, protocol::Tool};

const LIMIT: u64 = 256 * 1024;

struct Edit {
    source: PathBuf,
    target: PathBuf,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
}

/// Inspect every configured local consumer without creating files or directories.
/// An unavailable mirror is a readiness error, never an empty Windows mount.
pub fn compliant(paths: &LoginPaths, config: &Config) -> Result<bool> {
    Ok(plan(paths, config)?.is_empty())
}

/// A Send now checks only the actual hosted session's Claude directory. Other
/// profiles or mirrors cannot authorize its Enter, or hold a compliant target.
pub fn compliant_target(root: &Path) -> Result<bool> {
    let source = directory(root, false)?.join("keybindings.json");
    Ok(merged(read(&target(&source)?)?.as_deref())?.is_none())
}

/// Validate all consumers before the first mutation. Malformed owner files and
/// missing mirrors therefore leave even an otherwise valid default home alone.
pub fn apply(paths: &LoginPaths, config: &Config) -> Result<()> {
    let edits = plan(paths, config)?;
    for edit in &edits {
        ensure!(
            target(&edit.source)? == edit.target && read(&edit.target)? == edit.before,
            "Claude keybindings changed during policy preparation"
        );
    }
    for edit in edits {
        crate::util::write_atomic(&edit.target, &edit.after).context("saving Claude keybindings")?;
    }
    Ok(())
}

fn plan(paths: &LoginPaths, config: &Config) -> Result<Vec<Edit>> {
    let settings = crate::hosts::logins::Settings::from_config(config)?;
    let mut roots = vec![(paths.claude_dir.clone(), false)];
    roots.extend(
        settings
            .accounts
            .profiles
            .values()
            .filter(|p| p.provider == Tool::Claude)
            .map(|p| (p.directory.clone(), false)),
    );
    roots.extend(settings.credential_mirrors.iter().map(|p| (p.claude_dir.clone(), true)));
    let mut seen = BTreeSet::new();
    let mut edits = Vec::new();
    for (root, required) in roots {
        let source = directory(&root, required)?.join("keybindings.json");
        let resolved = target(&source)?;
        if !seen.insert(resolved.clone()) {
            continue;
        }
        let before = read(&resolved)?;
        if let Some(after) = merged(before.as_deref())? {
            edits.push(Edit { source, target: resolved, before, after });
        }
    }
    Ok(edits)
}

/// Resolve existing ancestors without creating them. A configured mirror must
/// already be a directory, so an unmounted consumer cannot be synthesized here.
fn directory(path: &Path, required: bool) -> Result<PathBuf> {
    ensure!(
        path.is_absolute() && path.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "Claude configuration requires a normalized absolute directory"
    );
    let mut ancestor = path;
    let mut tail = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let mut resolved = ancestor.canonicalize().context("resolving Claude configuration directory")?;
                ensure!(resolved.is_dir(), "Claude configuration root is not a directory");
                for name in tail.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
                tail.push(ancestor.file_name().context("Claude configuration has no existing ancestor")?.to_owned());
                ancestor = ancestor.parent().context("Claude configuration has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("Claude configuration consumer is unavailable"),
        }
    }
}

fn target(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_symlink() => path.canonicalize().context("resolving Claude keybindings link"),
        Ok(_) => Ok(path.into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.into()),
        Err(error) => Err(error).context("inspecting Claude keybindings"),
    }
}

fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("opening Claude keybindings"),
    };
    let meta = file.metadata()?;
    ensure!(meta.is_file() && meta.len() <= LIMIT, "Claude keybindings must be a bounded regular file");
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= LIMIT, "Claude keybindings exceed the file limit");
    Ok(Some(bytes))
}

fn merged(before: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
    let mut value = match before {
        Some(bytes) => parse(bytes)?,
        None => json!({
            "$schema":"https://www.schemastore.org/claude-code-keybindings.json",
            "$docs":"https://code.claude.com/docs/en/keybindings",
            "bindings":[]
        }),
    };
    let blocks = value
        .as_object_mut()
        .and_then(|o| o.get_mut("bindings"))
        .and_then(Value::as_array_mut)
        .context("Claude keybindings require an object with a bindings array")?;
    for block in blocks.iter() {
        let bindings = block
            .as_object()
            .filter(|o| o.get("context").is_some_and(Value::is_string))
            .and_then(|o| o.get("bindings"))
            .and_then(Value::as_object)
            .context("invalid Claude keybinding block")?;
        ensure!(bindings.values().all(|v| v.is_string() || v.is_null()), "invalid Claude keybinding action");
    }
    enter_sends(blocks)?;
    let policy = json!({"context":"Chat","bindings":{"enter":"chat:sendNow","ctrl+x ctrl+s":null}});
    if blocks.last() == Some(&policy) {
        // Claude's loader uses JSON.parse, so JSONC accepted as repair input
        // still needs strict JSON output. Already valid JSON keeps its bytes.
        if before.is_some_and(|bytes| serde_json::from_slice::<Value>(bytes).is_ok()) {
            return Ok(None);
        }
    } else {
        blocks.push(policy);
    }
    let mut after = serde_json::to_vec_pretty(&value)?;
    after.push(b'\n');
    if after.len() as u64 > LIMIT {
        bail!("Claude keybindings policy exceeds the file limit");
    }
    Ok(Some(after))
}

/// `SEND_NOW_KEYS` is one carriage return, which Claude reads as a plain
/// Enter (never Ctrl+S, its `chat:stash` default). Claude appends the owner's
/// blocks to its defaults, and the policy block is last, so its Chat Enter
/// wins. A block in another context can outrank Chat, and a longer chord that
/// starts with Enter makes Enter wait for the rest. Refuse both, without
/// rewriting the owner's file.
fn enter_sends(blocks: &[Value]) -> Result<()> {
    let mut chords = std::collections::BTreeMap::new();
    for bindings in blocks.iter().filter_map(|block| block.get("bindings").and_then(Value::as_object)) {
        for (keys, action) in bindings {
            let strokes = chord(keys);
            ensure!(
                action.as_str() != Some("chat:stash") || !strokes.iter().any(|stroke| stroke == "enter"),
                "Claude keybindings bind Enter to chat:stash ({keys}); Send now would stash the draft"
            );
            if strokes.len() > 1 && strokes[0] == "enter" {
                chords.insert(strokes, (keys, action));
            }
        }
    }
    // Claude keeps a chord's last binding; a later null unbinds it.
    if let Some((keys, _)) = chords.values().find(|(_, action)| !action.is_null()) {
        bail!("Claude keybindings start a chord with Enter ({keys}); Enter would wait instead of sending");
    }
    Ok(())
}

/// A key sequence as Claude's loader normalizes it: strokes split on
/// whitespace, modifier aliases sorted, `return` and `esc` renamed.
fn chord(keys: &str) -> Vec<String> {
    if keys == " " {
        return vec!["space".into()];
    }
    keys.split_whitespace()
        .map(|stroke| {
            let mut modifiers = Vec::new();
            let mut key = String::new();
            for part in stroke.split('+').map(|part| part.trim().to_lowercase()) {
                match part.as_str() {
                    "ctrl" | "control" => modifiers.push("ctrl"),
                    "alt" | "opt" | "option" | "meta" => modifiers.push("alt"),
                    "cmd" | "command" | "super" | "win" => modifiers.push("cmd"),
                    "shift" => modifiers.push("shift"),
                    "esc" => key = "escape".into(),
                    "return" => key = "enter".into(),
                    "del" => key = "delete".into(),
                    _ => key = part,
                }
            }
            modifiers.sort_unstable();
            modifiers.into_iter().chain([key.as_str()]).collect::<Vec<_>>().join("+")
        })
        .collect()
}

fn parse(bytes: &[u8]) -> Result<Value> {
    // The shared JSONC stripper tolerates an unfinished block comment. Repair
    // input must be complete before we replace any of the owner's files.
    let mut i = 0;
    let mut string = false;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1)) {
            (b'\\', _) if string => i += 2,
            (b'"', _) => {
                string = !string;
                i += 1;
            }
            (b'/', Some(b'/')) if !string => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            (b'/', Some(b'*')) if !string => {
                let end = bytes[i + 2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .context("unfinished Claude keybindings comment")?;
                i += end + 4;
            }
            _ => i += 1,
        }
    }
    serde_json::from_slice(&crate::config::strip_comments(bytes)).context("invalid Claude keybindings JSON")
}

#[cfg(test)]
#[path = "../../src/tests/claude_keybindings.rs"]
mod tests;
