//! Bundled local agent policy upkeep. Each service maintains the Claude Enter policy for its own configured
//! consumers; it does not probe or modify another host.
use crate::{config::Config, hosts::logins::LoginPaths, providers::claude_keybindings};
use std::time::{Duration, Instant};

/// One service-local timer, independent of the number of remote host links.
#[derive(Default)]
pub struct LocalPolicy {
    checked: Option<Instant>,
    failure: Option<String>,
}
impl LocalPolicy {
    pub fn due(&self, now: Instant) -> bool {
        self.checked.is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(60))
    }

    /// Return only a changed failure. Recovery clears deduplication, not the
    /// owner's error history. All paths come from this service's own options.
    pub fn tick(&mut self, now: Instant, paths: &LoginPaths, loaded: &(Config, Option<String>)) -> Option<String> {
        if !self.due(now) {
            return None;
        }
        self.checked = Some(now);
        let result = if loaded.1.is_some() {
            Err(anyhow::anyhow!("local service configuration is unavailable"))
        } else if !loaded.0.plugin_enabled("host-bootstrap") {
            Ok(())
        } else {
            claude_keybindings::apply(paths, &loaded.0).and_then(|()| {
                anyhow::ensure!(claude_keybindings::compliant(paths, &loaded.0)?, "Claude Enter policy did not verify");
                Ok(())
            })
        };
        let failure = result.err().map(|error| format!("Claude Enter policy upkeep held: {error:#}"));
        if failure == self.failure {
            return None;
        }
        self.failure = failure.clone();
        failure
    }
}
