//! Thin service adapter for the login package's watcher and sanitized health.
use super::*;
use crate::hosts::logins::{self, LoginPaths};

fn paths(service: &Service) -> LoginPaths {
    LoginPaths { claude_dir: service.opts.claude_dir.clone(), codex_dir: service.opts.codex_dir.clone() }
}

pub(super) fn status(service: &Service) -> Result<Value, String> {
    let settings = logins::Settings::from_config(&service.opts.config).map_err(|e| e.to_string())?;
    let mut status = logins::health(&paths(service), &settings, logins::now_ms()).map_err(|e| e.to_string())?;
    status["enabled"] =
        Value::Bool(service.opts.config.sync_logins && service.opts.config.plugin_enabled("login-sync"));
    Ok(status)
}

pub(super) fn tick(service: &mut Service, now: Instant) {
    if !service.opts.config.sync_logins || !service.opts.config.plugin_enabled("login-sync") {
        service.native.logins = Default::default();
        return;
    }
    let settings = match logins::Settings::from_config(&service.opts.config) {
        Ok(settings) => {
            service.native.logins.configuration_error(None);
            settings
        }
        Err(error) => {
            if let Some(text) =
                service.native.logins.configuration_error(Some(format!("Login warning configuration: {error}")))
            {
                if service.opts.config.notifications {
                    external::notify("Login warning unavailable", &text);
                }
                service.announce_error(text);
            }
            withdraw(service);
            return;
        }
    };
    let paths = paths(service);
    for text in service.native.logins.poll(&paths, &settings, now, logins::now_ms()) {
        if service.opts.config.notifications {
            external::notify("Login needs attention", &text);
        }
        service.announce_error(text);
    }
    withdraw(service);
}

/// Take back the listed notices that no longer hold, such as a warning the
/// renewed login answered, from every deck's error list.
fn withdraw(service: &mut Service) {
    let listed: Vec<String> = service.errors.borrow().iter().map(|entry| entry.text.clone()).collect();
    let stale = service.native.logins.stale(listed.into_iter());
    service.withdraw_errors(&stale);
}

#[cfg(test)]
#[path = "../../src/tests/login_alerts.rs"]
mod tests;
