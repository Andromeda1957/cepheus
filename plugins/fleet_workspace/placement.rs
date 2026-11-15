//! Placement policy shared by direct and queued starts. No probes run here.
use crate::scheduler::Headroom;

/// A short reservation covers starts not yet visible in a host's memory sample.
pub const RESERVATION_SECONDS: u64 = 30;
pub const START_MIB: u64 = 512;

pub fn score(headroom: &Headroom, now: u64, pending: usize, desktop: bool) -> Option<u64> {
    if now.saturating_sub(headroom.at) > 30 || !headroom.admits() {
        return None;
    }
    let available = headroom.available_mib?;
    let reserve = headroom.total_mib.unwrap_or(0).saturating_mul(15) / 100;
    let reserve = reserve.max(if desktop { 2560 } else { 2048 });
    let free = available.checked_sub(reserve.saturating_add((pending as u64).saturating_mul(START_MIB)))?;
    if free < START_MIB {
        return None;
    }
    let cpus = headroom.cpus.max(1) as f64;
    let load = headroom.load.unwrap_or(cpus) / cpus;
    if !load.is_finite() || load > 1.5 {
        return None;
    }
    let pressure = headroom.cpu_pressure.unwrap_or(0.0);
    if !pressure.is_finite() {
        return None;
    }
    let cpu = (1.5 - load).max(0.0) * (100.0 - pressure).max(0.0) * cpus;
    Some(free.saturating_add((cpu * 0.16) as u64))
}

/// Executable presence is a cheap launch prerequisite; operation profiles perform
/// the deeper tool/emulator checks. Commands remain trusted host configuration.
pub fn available_tools(config: &crate::config::Config) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |command: &[String]| -> bool {
        let Some(name) = command.first() else {
            return false;
        };
        let path = std::path::Path::new(name);
        let check =
            |p: std::path::PathBuf| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
        if path.is_absolute() {
            return check(path.to_path_buf());
        }
        if name.contains('/') {
            return false;
        }
        std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| check(dir.join(name))))
    };
    let mut result = Vec::new();
    for (name, command) in [
        ("claude", &config.claude_command),
        ("codex", &config.codex_command),
        ("agy", &config.antigravity_command),
        ("grok", &config.grok_command),
        ("opencode", &config.opencode_command),
    ] {
        if config.plugins.get(&format!("provider-{name}")) != Some(&false) && executable(command) {
            result.push(name.to_string());
        }
    }
    for provider in &config.providers {
        if config.plugins.get(&format!("provider-{}", provider.id)) != Some(&false) && executable(&provider.command) {
            result.push(provider.id.clone());
        }
    }
    result
}

#[cfg(test)]
#[path = "../../src/tests/scheduler/placement.rs"]
mod tests;
