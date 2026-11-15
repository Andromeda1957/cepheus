//! Bundled native service registrations. Each package owns its policy.
use super::*;
pub(crate) struct State {
    pub titles: Titles,
    pub dev: Option<DevProc>,
    pub dev_port_busy: bool,
    pub dev_port_checked: Option<Instant>,
    pub scan_jobs: Vec<jobs::ScanJob>,
    pub scans: Vec<ScanInfo>,
    pub scans_read: Option<Instant>,
    pub installs: usize,
    pub scan_preparing: usize,
    pub logins: crate::hosts::logins::Watcher,
    pub api_cost: cost::State,
}
impl State {
    pub(crate) fn new(opts: &ServiceOptions) -> Self {
        let titles = if opts.monitor && opts.config.short_titles && opts.config.plugin_enabled("ai-titles") {
            Titles::claude(&opts.config.claude_command, opts.state_dir.clone())
        } else {
            Titles::off()
        };
        Self {
            titles,
            dev: None,
            dev_port_busy: false,
            dev_port_checked: None,
            scan_jobs: Vec::new(),
            scans: Vec::new(),
            scans_read: None,
            installs: 0,
            scan_preparing: 0,
            logins: Default::default(),
            api_cost: Default::default(),
        }
    }
}

#[path = "agent-extensions/service_commands.rs"]
mod agent_extensions;
#[path = "api-cost/service.rs"]
mod cost;
#[path = "dev-services/service_commands.rs"]
mod dev;
#[path = "dot/service_commands.rs"]
mod dot;
#[path = "launch-settings/service_commands.rs"]
mod launch_settings;
#[path = "login-sync/service_commands.rs"]
mod logins;
#[path = "providers/peers_service.rs"]
mod peers;
#[path = "providers/service_controls.rs"]
mod provider_controls;
#[path = "providers/target.rs"]
pub(crate) mod provider_target;
#[path = "security-scans/service_commands.rs"]
mod scans;

pub(super) fn start(
    service: &mut Service,
    client: u64,
    request: u64,
    plugin: &str,
    command: &str,
    args: &Value,
) -> bool {
    dot::start(service, client, request, plugin, command, args)
        || provider_target::start(service, client, request, plugin, command, args)
        || provider_controls::start(service, client, request, plugin, command, args)
        || peers::start(service, client, request, plugin, command, args)
        || agent_extensions::start(service, client, request, plugin, command, args)
        || cost::start(service, client, request, plugin, command, args)
}
pub(super) fn argument_limit(config: &Config, plugin: &str, command: &str) -> usize {
    if (plugin, command) == ("provider-dot", "send") {
        crate::dot::api::REQUEST_MAX
    } else {
        provider_target::argument_limit(config, plugin, command)
    }
}
pub(super) fn command(
    service: &mut Service,
    plugin: &str,
    command: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    dev::command(service, plugin, command, args)
        .or_else(|| scans::command(service, plugin, command, args))
        .or_else(|| launch_settings::command(service, plugin, command, args))
        .or_else(|| match (plugin, command) {
            ("login-sync", "status") => Some(logins::status(service)),
            ("ai-titles", "status") => Some(Ok(serde_json::json!({"enabled":service.opts.config.short_titles}))),
            ("desktop", "status") => Some(Ok(serde_json::json!({"enabled":true,"adapters":["kitty","hyprland"]}))),
            _ => None,
        })
}

pub(crate) fn initialize(service: &mut Service) {
    if !service.opts.config.plugin_enabled("dev-services") {
        if let Some(process) = service.native.dev.as_mut() {
            process.stop();
        }
    }
    service.load_scan_jobs();
    if service.opts.monitor && service.opts.config.skills && service.opts.config.plugin_enabled("agent-extensions") {
        service.sync_skills(None);
    }
}
pub(crate) fn tick(service: &mut Service, now: Instant) {
    service.watch_dev_server(now);
    logins::tick(service, now);
}
pub(crate) fn collect(service: &mut Service) {
    service.native.titles.collect();
}
pub(crate) fn api_cost_done(service: &mut Service, result: Result<Value, String>) {
    cost::done(service, result);
}
