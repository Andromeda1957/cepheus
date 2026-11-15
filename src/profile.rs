//! Local installation defaults, kept out of the distributable core.
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

/// Config values take precedence. A missing profile keeps portable defaults.
pub fn merge(config_path: &Path, config: Value) -> Result<Value> {
    let path = config_path.with_file_name("profile.json");
    if !path.exists() {
        return Ok(config);
    }
    let text = std::fs::read_to_string(&path).context("read local deck profile")?;
    if text.len() > 1024 * 1024 {
        anyhow::bail!("local deck profile exceeds 1 MiB");
    }
    let mut profile: Value = serde_json::from_str(&text).context("parse local deck profile")?;
    if !profile.is_object() || !config.is_object() {
        anyhow::bail!("deck configuration and profile must be objects");
    }
    overlay(&mut profile, config);
    Ok(profile)
}
fn overlay(base: &mut Value, update: Value) {
    match (base, update) {
        (Value::Object(base), Value::Object(update)) => {
            for (key, value) in update {
                match base.get_mut(&key) {
                    Some(previous) => overlay(previous, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, update) => *base = update,
    }
}
