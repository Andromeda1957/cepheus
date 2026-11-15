//! The `api-cost` service commands. A view asks for the total; a collector
//! pass runs on a thread of its own when the last one started at least 30
//! seconds ago, one at a time, and every request waiting on it gets its result.
use super::*;
use crate::api_cost::{Collector, Roots};

const SPACING: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct State {
    collector: Option<Arc<Mutex<Collector>>>,
    running: bool,
    started: Option<Instant>,
    last: Option<Result<Value, String>>,
    waiting: Vec<(u64, u64)>,
}

pub(super) fn start(
    service: &mut Service,
    client: u64,
    request: u64,
    plugin: &str,
    command: &str,
    _args: &Value,
) -> bool {
    if plugin != "api-cost" || !matches!(command, "summary" | "refresh") {
        return false;
    }
    let state = &mut service.native.api_cost;
    let due = command == "refresh" || state.started.map_or(true, |at| at.elapsed() >= SPACING);
    if state.running || !due {
        match (&state.last, command == "summary" || !state.running) {
            (Some(last), true) => {
                let (data, error) = reply(last);
                service.plugin_reply(client, request, data, error);
            }
            _ => state.waiting.push((client, request)),
        }
        return true;
    }
    let collector = state
        .collector
        .get_or_insert_with(|| {
            Arc::new(Mutex::new(Collector::new(service.opts.state_dir.join("api-cost"), Roots::from_env())))
        })
        .clone();
    let hosts = service.opts.config.hosts.clone();
    let key = crate::hosts::existing_hub_key(&service.opts.state_dir);
    let tx = service.tx.clone();
    let spawned = thread::Builder::new().name("api-cost".into()).spawn(move || {
        let mut collector = collector.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = collector.pass_over_links(&hosts, key).map_err(|error| format!("{error:#}"));
        let _ = tx.send(Ev::ApiCost(result));
    });
    match spawned {
        Ok(_) => {
            state.running = true;
            state.started = Some(Instant::now());
            state.waiting.push((client, request));
        }
        Err(error) => service.plugin_reply(client, request, Value::Null, Some(format!("API cost: {error}"))),
    }
    true
}

/// A pass ended: keep its result and answer everyone waiting on it.
pub(super) fn done(service: &mut Service, result: Result<Value, String>) {
    let state = &mut service.native.api_cost;
    state.running = false;
    let waiting = std::mem::take(&mut state.waiting);
    let (data, error) = reply(&result);
    state.last = Some(result);
    for (client, request) in waiting {
        service.plugin_reply(client, request, data.clone(), error.clone());
    }
}

fn reply(result: &Result<Value, String>) -> (Value, Option<String>) {
    match result {
        Ok(summary) => (summary.clone(), None),
        Err(error) => (Value::Null, Some(error.clone())),
    }
}
