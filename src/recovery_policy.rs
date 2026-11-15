//! Configurable cold-start recovery policy. Session custody remains in core.
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub resume_interrupted: bool,
    pub restore_drafts: bool,
    pub startup_delay_secs: u64,
    pub launch_interval_ms: u64,
    pub retry_delay_secs: u64,
    pub launch_attempts: usize,
    pub ready_timeout_secs: u64,
    pub settle_ms: u64,
    pub checkpoint_interval_ms: u64,
    pub recover_clients: bool,
    pub client_attempts: usize,
    pub client_attempt_window_secs: u64,
    pub client_settle_ms: u64,
    pub providers: BTreeMap<String, Override>,
    pub sessions: BTreeMap<String, Override>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resume_interrupted: true,
            restore_drafts: true,
            startup_delay_secs: 0,
            launch_interval_ms: 250,
            retry_delay_secs: 10,
            launch_attempts: 5,
            ready_timeout_secs: 120,
            settle_ms: 3000,
            checkpoint_interval_ms: 250,
            recover_clients: true,
            client_attempts: 4,
            client_attempt_window_secs: 1800,
            client_settle_ms: 1500,
            providers: BTreeMap::new(),
            sessions: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Override {
    pub restore: Option<bool>,
    pub resume_interrupted: Option<bool>,
    pub message: Option<String>,
}

impl Settings {
    pub fn restore(&self, provider: &str, session: u64) -> bool {
        self.overrides(provider, session).filter_map(|rule| rule.restore).last().unwrap_or(true)
    }
    pub fn resume(&self, provider: &str, session: u64) -> bool {
        self.overrides(provider, session)
            .filter_map(|rule| rule.resume_interrupted)
            .last()
            .unwrap_or(self.resume_interrupted)
    }
    pub fn message<'a>(&'a self, provider: &str, session: u64, fallback: &'a str) -> &'a str {
        self.overrides(provider, session).filter_map(|rule| rule.message.as_deref()).last().unwrap_or(fallback)
    }
    fn overrides(&self, provider: &str, session: u64) -> impl Iterator<Item = &Override> {
        self.providers.get(provider).into_iter().chain(self.sessions.get(&session.to_string()))
    }
}
