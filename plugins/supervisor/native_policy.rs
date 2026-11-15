//! Native one-request Read policy. Separate from shell/TUI attestation.
//! Core owns authenticated hook custody and repeats evaluation after durable
//! audit with fresh enablement, generation, process and conversation fences.
//! This module reads metadata only; no requested file contents are opened.
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Component, Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Unsupported,
    InvalidInput,
    InvalidRules,
    UnresolvedPath,
    OutsideWorktree,
    ForbiddenPath,
    NoRule,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Decline(Category),
    AllowOnce { rule_id: String },
}

fn text(value: &Value, max: usize) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control))
}

fn normalized(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && !path.as_os_str().as_encoded_bytes().windows(3).any(|w| w == b"/./")
        && !path.as_os_str().as_encoded_bytes().ends_with(b"/.")
}

fn valid_input(value: &Value) -> bool {
    let Some(object) = value.as_object() else { return false };
    if object.is_empty() || object.keys().any(|k| !["file_path", "offset", "limit"].contains(&k.as_str())) {
        return false;
    }
    let Some(path) = object.get("file_path").and_then(|v| text(v, 4096)) else { return false };
    if !normalized(Path::new(path)) || path.contains('\\') {
        return false;
    }
    [("offset", 10_000_000), ("limit", 10_000)]
        .iter()
        .all(|(name, max)| object.get(*name).is_none_or(|v| v.as_u64().is_some_and(|n| n > 0 && n <= *max)))
}

fn valid_rule(rule: &Value) -> bool {
    let Some(rule) = rule.as_object() else { return false };
    let Some(id) = rule.get("id").and_then(|v| text(v, 64)) else { return false };
    if rule.len() != 3
        || !id.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
        || rule.get("tool").and_then(Value::as_str) != Some("Read")
    {
        return false;
    }
    let Some(pattern) = rule.get("pattern").and_then(Value::as_object) else { return false };
    pattern.len() == 2
        && pattern.get("kind").and_then(Value::as_str) == Some("native_exact")
        && pattern.get("input").is_some_and(valid_input)
}

/// Validate only the native_exact partition. Core bounds all rule partitions
/// together and preserves ordinary exact/argv_glob terminal rules separately.
pub fn validate_rules(rules: &[Value]) -> bool {
    let mut ids = BTreeSet::new();
    rules.len() <= 32 && rules.iter().all(|r| valid_rule(r) && ids.insert(r["id"].as_str().unwrap()))
}

fn forbidden(path: &Path) -> bool {
    path.components().any(|part| {
        let Component::Normal(part) = part else { return false };
        let Some(part) = part.to_str() else { return true };
        let lower = part.to_ascii_lowercase();
        lower.starts_with('.')
            || lower.contains("://")
            || lower.ends_with(".pem")
            || lower.ends_with(".key")
            || lower.ends_with(".p12")
            || lower.ends_with(".pfx")
            || lower.starts_with("id_rsa")
            || lower.starts_with("id_ed25519")
            || lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| {
                [
                    "credential",
                    "credentials",
                    "secret",
                    "secrets",
                    "password",
                    "passwd",
                    "token",
                    "tokens",
                    "apikey",
                    "authorization",
                    "bearer",
                    "netrc",
                    "npmrc",
                    "pypirc",
                ]
                .contains(&word)
            })
    })
}

// These exact credential-store names remain ineligible as an authority root.
// Incidental hidden ancestors (.local/.worktrees) or account names containing a
// credential word do not make a registered Git worktree a credential store.
// Core additionally excludes its configured provider/account credential roots.
fn sensitive_store(path: &Path) -> bool {
    let parts: Vec<_> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(part) => part.to_str().map(str::to_ascii_lowercase),
            _ => None,
        })
        .collect();
    parts.iter().any(|part| {
        [".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker", ".pki", ".password-store", ".claude", ".codex"]
            .contains(&part.as_str())
    }) || parts.windows(2).any(|pair| {
        pair[0] == ".config" && ["gh", "gcloud", "opencode", "cepheus", "agent-deck"].contains(&pair[1].as_str())
        // legacy-name
    }) || parts.windows(3).any(|triple| {
        triple[0] == ".local" && triple[1] == "share" && ["cepheus", "agent-deck"].contains(&triple[2].as_str())
        // legacy-name
    })
}

/// Evaluate the full structured Read input with current canonical file proofs.
/// No shell command or executable/selected-option attestation is inferred.
pub fn evaluate(input: &Value, cwd: &Path, worktree: &Path, rules: &[Value]) -> Decision {
    use Category::*;
    let decline = Decision::Decline;
    if !valid_input(input) {
        return decline(InvalidInput);
    }
    if !validate_rules(rules) {
        return decline(InvalidRules);
    }
    let Ok(root) = worktree.canonicalize() else { return decline(UnresolvedPath) };
    let Ok(cwd_now) = cwd.canonicalize() else { return decline(UnresolvedPath) };
    if !normalized(worktree)
        || !normalized(cwd)
        || root != worktree
        || cwd_now != cwd
        || !root.is_dir()
        || !cwd_now.is_dir()
    {
        return decline(UnresolvedPath);
    }
    if !cwd_now.starts_with(&root) {
        return decline(OutsideWorktree);
    }
    if sensitive_store(&root) || sensitive_store(&cwd_now) {
        return decline(ForbiddenPath);
    }
    let requested = Path::new(input["file_path"].as_str().unwrap());
    if !requested.starts_with(&root) {
        return decline(OutsideWorktree);
    }
    // Native Read will later open its original pathname, not a descriptor we
    // can transfer. Keep this subset free of symlink aliases in every component.
    let Ok(relative) = requested.strip_prefix(&root) else { return decline(OutsideWorktree) };
    if forbidden(relative) {
        return decline(ForbiddenPath);
    }
    let mut cursor = root.clone();
    for part in relative.components() {
        cursor.push(part);
        let Ok(meta) = cursor.symlink_metadata() else { return decline(UnresolvedPath) };
        if meta.file_type().is_symlink() {
            return decline(UnresolvedPath);
        }
    }
    let Ok(resolved) = requested.canonicalize() else { return decline(UnresolvedPath) };
    if !resolved.starts_with(&root) {
        return decline(OutsideWorktree);
    }
    let Ok(resolved_relative) = resolved.strip_prefix(&root) else { return decline(OutsideWorktree) };
    if forbidden(resolved_relative) {
        return decline(ForbiddenPath);
    }
    let Ok(metadata) = resolved.metadata() else { return decline(UnresolvedPath) };
    if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
        return decline(Unsupported);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return decline(Unsupported);
        }
    }
    #[cfg(not(unix))]
    return decline(Unsupported);
    for rule in rules {
        if rule["pattern"]["input"] == *input {
            return Decision::AllowOnce { rule_id: rule["id"].as_str().unwrap().to_string() };
        }
    }
    decline(NoRule)
}

#[cfg(test)]
#[path = "../../src/tests/native_permission_policy.rs"]
mod tests;
