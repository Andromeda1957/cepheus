//! The hub side of other hosts (`hosts/mod.rs`): their links, their
//! sessions merged into this service's state, and every deck message that
//! names one of them passed to that host on the deck's own upstream.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Out, Service, StartModel};
use crate::config::{Config, HostConfig};
use crate::external;
use crate::handover::HostOutageHandover;
use crate::hosts::link::{self, HostEv};
use crate::hosts::logins::LoginPaths;
use crate::hosts::upkeep::HubCtx;
use crate::hosts::wire::Welcome;
use crate::hosts::{self, fed, peer};
use crate::protocol::{
    encode, split_id, AlertKind, ClientMsg, ClientRole, Conversation, DeckState, HostInfo, HostStats, ServerMsg,
    PROTOCOL_VERSION,
};

/// How often the configured hosts are compared with the running links.
const HOSTS_CHECK_INTERVAL: Duration = Duration::from_secs(10);
/// How long a merged conversation list waits for slow hosts.
const LIST_WAIT: Duration = Duration::from_secs(4);
/// How often the peer listener is looked for, and how long after a start
/// it is given before starting it again.
const PEER_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const PEER_RETRY: Duration = Duration::from_secs(30);
/// Allow the 30s peer supervisor retry, 15s link backoff and 6s TLS timeout
/// before adding a durable error for a continuously unreachable host.
const HOST_OFFLINE_GRACE: Duration = Duration::from_secs(60);

/// The errors that only say a host is not connected, after its label. They stop being true once it connects
/// again, so that removes them from the error list. An error saying what an outage did to a request, such as
/// "nothing was sent", stays.
const UNREACHABLE: &str = " is unreachable. Reconnecting automatically.\n";
const OFFLINE: &str = " is offline: ";

fn is_connection_error(text: &str, label: &str) -> bool {
    text.strip_prefix(label).is_some_and(|rest| rest.starts_with(UNREACHABLE) || rest.starts_with(OFFLINE))
}

#[derive(Default)]
pub(super) struct HostOutage {
    since: Option<Instant>,
    carried: Duration,
    reported: bool,
    reason: Option<String>,
}

impl HostOutage {
    fn elapsed(&self, now: Instant) -> Duration {
        self.since.map_or(Duration::ZERO, |at| self.carried.saturating_add(now.saturating_duration_since(at)))
    }
}

/// A configured host and what the hub knows of it now.
pub struct HostSlot {
    pub config: HostConfig,
    stop: Arc<AtomicBool>,
    /// The main channel while it is open.
    pub(super) out: Option<Sender<Vec<u8>>>,
    pub(super) welcome: Option<Welcome>,
    /// Its last state, with the hub's ids.
    state: Option<DeckState>,
    reason: Option<String>,
    stats: Option<HostStats>,
    /// A build host's host channel answered: it has no session channel.
    reached: bool,
    outage: HostOutage,
}

impl Drop for HostSlot {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl HostSlot {
    /// Its session channel is open, or for a build host its host channel.
    pub(super) fn connected(&self) -> bool {
        if self.config.runs_agents() {
            self.out.is_some()
        } else {
            self.reached
        }
    }

    pub(super) fn placement_state(&self) -> Option<&DeckState> {
        self.state
            .as_ref()
            .filter(|_| self.out.is_some() && self.welcome.as_ref().is_some_and(|welcome| welcome.version >= 14))
    }

    pub(super) fn label(&self) -> String {
        self.config.label()
    }
}

/// A conversation list being gathered from the hub and its hosts for one
/// deck: what has come, who is still to answer and until when.
pub struct PendingList {
    request: Option<u64>,
    query: String,
    items: Vec<Conversation>,
    waiting: HashSet<u16>,
    until: Instant,
}

impl Service {
    pub(super) fn host_outage_handover(&self, now: Instant) -> Vec<HostOutageHandover> {
        self.hosts
            .values()
            .map(|slot| (&slot.config, &slot.outage))
            .chain(self.restored_host_outages.iter().map(|(config, outage)| (config, outage)))
            .filter_map(|(config, outage)| {
                outage.since.map(|_| HostOutageHandover {
                    config: config.clone(),
                    down_for_ms: outage.elapsed(now).min(HOST_OFFLINE_GRACE).as_millis() as u64,
                    reported: outage.reported,
                    reason: outage.reason.clone(),
                })
            })
            .collect()
    }

    pub(super) fn restore_host_outages(&mut self, outages: Vec<HostOutageHandover>, now: Instant) {
        self.restored_host_outages = outages
            .into_iter()
            .map(|outage| {
                (
                    outage.config,
                    HostOutage {
                        since: Some(now),
                        carried: Duration::from_millis(outage.down_for_ms).min(HOST_OFFLINE_GRACE),
                        reported: outage.reported,
                        reason: outage.reason,
                    },
                )
            })
            .collect();
    }

    fn take_restored_host_outage(&mut self, config: &HostConfig) -> HostOutage {
        self.restored_host_outages
            .iter()
            .position(|(saved, _)| saved == config)
            .map(|index| self.restored_host_outages.remove(index).1)
            .unwrap_or_default()
    }

    /// Status changes immediately; only the durable outage error is delayed.
    pub(super) fn host_outages_due(&mut self, now: Instant) {
        let errors: Vec<String> = self
            .hosts
            .values_mut()
            .filter_map(|slot| {
                if slot.connected() || slot.outage.reported || slot.outage.elapsed(now) < HOST_OFFLINE_GRACE {
                    return None;
                }
                slot.outage.reported = true;
                Some(format!("{}{UNREACHABLE}{}", slot.label(), slot.reason.as_deref().unwrap_or_default()))
            })
            .collect();
        for error in &errors {
            self.log_error(error);
        }
        if !errors.is_empty() {
            self.state_changed();
        }
    }

    /// The host labelled `label` is connected: its connection errors leave
    /// the list, in every deck and phone that shows it. That includes one
    /// handed over by an earlier build or kept from an earlier outage.
    fn retire_connection_errors(&mut self, label: &str) {
        let retired = {
            let mut errors = self.errors.borrow_mut();
            let before = errors.len();
            errors.retain(|entry| !is_connection_error(&entry.text, label));
            errors.len() != before
        };
        if retired {
            self.state_changed();
        }
    }

    pub(super) fn sync_error_clear(&self, tag: u16) {
        if self.error_clears.is_empty() {
            return;
        }
        let Some(slot) = self.hosts.get(&tag) else { return };
        let Some(out) = &slot.out else { return };
        let message = if slot.welcome.as_ref().is_some_and(|w| w.version >= 14) {
            ClientMsg::ClearErrorsThrough { clears: self.error_clears.clone() }
        } else {
            ClientMsg::ClearErrors
        };
        let _ = out.send(encode(&message));
    }

    pub(super) fn clear_cached_host_errors(&mut self) {
        for slot in self.hosts.values_mut() {
            if let Some(state) = &mut slot.state {
                state.errors.clear();
            }
        }
        for tag in self.hosts.keys() {
            self.sync_error_clear(*tag);
        }
    }

    /// Compatibility adapter for existing repository-mapping fixtures.
    #[cfg(test)]
    pub(super) fn scheduler_host(
        &self,
        cwd: Option<&std::path::Path>,
        preferred: &str,
    ) -> anyhow::Result<(Option<String>, std::path::PathBuf)> {
        self.workspace_host(cwd, preferred, None)
    }
    pub(super) fn scheduler_host_min(
        &self,
        cwd: Option<&std::path::Path>,
        preferred: &str,
        require_jobs: bool,
        require_work_queue: bool,
    ) -> anyhow::Result<(Option<String>, std::path::PathBuf)> {
        if !matches!(preferred, "auto" | "local") {
            if let Some(slot) = self.hosts.values().find(|s| s.config.name == preferred) {
                anyhow::ensure!(
                    slot.welcome.as_ref().is_none_or(|w| w.version >= 14),
                    "Update needed: selected host requires protocol 14 work queues"
                );
            }
        }
        let requested = cwd.unwrap_or(&self.opts.start_dir);
        let canonical;
        let cwd = if preferred == "local" {
            requested
        } else {
            anyhow::ensure!(requested.is_absolute(), "placement cwd must be an absolute directory");
            canonical = requested.canonicalize().context("placement cwd is unavailable")?;
            anyhow::ensure!(canonical.is_dir(), "placement cwd must be a directory");
            canonical.as_path()
        };
        let mut candidates = Vec::new();
        // Keep job/queue ranking independent of interactive placement policy.
        let score = |headroom: &crate::scheduler::Headroom| {
            let memory = headroom
                .available_mib
                .zip(headroom.total_mib)
                .map_or(0, |(available, total)| available.saturating_sub(2048.max(total.saturating_mul(15) / 100)));
            let cpu = (headroom.cpus.max(1) as f64
                * (100.0 - headroom.cpu_pressure.unwrap_or(0.0).clamp(0.0, 100.0))
                * 100.0) as u64;
            (memory, cpu)
        };
        let local = self.scheduler.settings.probe.read();
        if matches!(preferred, "auto" | "local") && local.admits() {
            let (memory, cpu) = score(&local);
            candidates.push((memory, cpu, None, cwd.to_path_buf()));
        }
        if preferred == "local" {
            return candidates
                .pop()
                .map(|(_, _, host, path)| (host, path))
                .context("local host has insufficient headroom");
        }
        if let Some((id, root)) = self.scheduler.repository_for(cwd) {
            let relative = cwd.strip_prefix(&root).context("placement cwd escaped its repository")?;
            for slot in self.hosts.values() {
                if preferred != "auto" && preferred != slot.config.name {
                    continue;
                }
                let Some(state) = slot.state.as_ref().filter(|state| {
                    slot.out.is_some()
                        && slot.welcome.as_ref().is_some_and(|w| w.version >= 14)
                        && (!require_jobs || slot.state.as_ref().is_some_and(|s| s.job_capable))
                        && (!require_work_queue || state.work_queue_capable)
                }) else {
                    continue;
                };
                let snapshot = &state.scheduler;
                let h = &snapshot.headroom;
                if !h.admits() || crate::util::now_secs().saturating_sub(h.at) > 30 {
                    continue;
                }
                let Some(path) = snapshot.repositories.get(&id) else { continue };
                let path = std::path::Path::new(path);
                if !path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                    continue;
                }
                let (memory, cpu) = score(h);
                candidates.push((
                    memory,
                    cpu,
                    Some(slot.config.name.clone()),
                    std::path::PathBuf::from(path).join(relative),
                ));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        candidates
            .into_iter()
            .next()
            .map(|(_, _, host, path)| (host, path))
            .context("no online host has this repository and resource headroom")
    }

    pub(super) fn scheduler_remote_start(
        &self,
        host: &str,
        request: u64,
        args: serde_json::Value,
    ) -> anyhow::Result<()> {
        let slot = self.hosts.values().find(|s| s.config.name == host).context("host is no longer configured")?;
        anyhow::ensure!(
            slot.welcome.as_ref().is_some_and(|w| w.version >= 14),
            "Update needed: scheduler requires protocol 14"
        );
        slot.out
            .as_ref()
            .context("host is offline")?
            .send(encode(&ClientMsg::Fleet { request, action: "start".into(), args }))
            .context("host link closed")
    }

    pub(super) fn job_peer(&self, name: &str) -> Option<&DeckState> {
        self.hosts.values().find(|h| h.config.name == name).and_then(|h| h.state.as_ref())
    }
    pub(super) fn job_peer_send(
        &self,
        host: &str,
        request: u64,
        action: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<()> {
        let slot = self.hosts.values().find(|h| h.config.name == host).context("job executor no longer configured")?;
        anyhow::ensure!(
            slot.welcome.as_ref().is_some_and(|w| w.version >= 14),
            "Update needed: jobs require protocol 14"
        );
        slot.out
            .as_ref()
            .context("job executor offline; placement remains pinned")?
            .send(encode(&ClientMsg::Fleet { request: request | (1u64 << 63), action: action.into(), args }))
            .context("job executor disconnected")
    }

    pub(super) fn fail_job_routes(&mut self, cid: Option<u64>, tag: Option<u16>, reason: &str) {
        let keys: Vec<_> = self
            .job_routes
            .iter()
            .filter(|((client, _), route)| cid.is_none_or(|id| id == *client) && tag.is_none_or(|id| id == route.host))
            .map(|(key, _)| *key)
            .collect();
        for (client, request) in keys {
            self.job_routes.remove(&(client, request));
            self.send(
                client,
                &ServerMsg::FleetResult { request, data: serde_json::Value::Null, error: Some(reason.into()) },
            );
        }
    }
    pub(super) fn expire_job_routes(&mut self) {
        let keys: Vec<_> = self
            .job_routes
            .iter()
            .filter(|(_, route)| route.at.elapsed() >= Duration::from_secs(30))
            .map(|(key, _)| *key)
            .collect();
        for (client, request) in keys {
            self.job_routes.remove(&(client, request));
            self.send(
                client,
                &ServerMsg::FleetResult {
                    request,
                    data: serde_json::Value::Null,
                    error: Some("Remote job request timed out; acceptance unknown; retry the same key".into()),
                },
            );
        }
    }

    /// The configuration as it is now: re-read from its file when there is
    /// one, so `hosts install` takes effect without a restart.
    fn current_config(&self) -> Config {
        match &self.opts.config_file {
            Some(path) => Config::load_from(path).0,
            None => self.opts.config.clone(),
        }
    }

    /// Start a link to every configured host that has none, and end the
    /// links of hosts no longer configured or changed.
    pub(super) fn sync_hosts(&mut self, now: Instant) {
        if self.shutdown_at.is_some()
            || self.hosts_checked.is_some_and(|at| now.duration_since(at) < HOSTS_CHECK_INTERVAL)
        {
            return;
        }
        self.hosts_checked = Some(now);
        let mut config = self.current_config();
        // A physical viewer exposes only its native sessions upstream. Its
        // public clients attach to the coordinator; reverse federation would
        // otherwise recursively import the shared registry under another tag.
        if self
            .opts
            .config_file
            .as_ref()
            .is_some_and(|path| !matches!(crate::client::coordinator::selected_from(path, &config), Ok(None)))
        {
            config.hosts.clear();
        }
        if config.plugin_enabled("git-mirror")
            && !self.mirror_watching
            && config.hosts.iter().any(HostConfig::runs_agents)
        {
            // One watcher for the service's life; links read its generation.
            self.mirror_watching = true;
            if let Err(error) = hosts::mirror::watch(&self.opts.start_dir, Arc::clone(&self.mirror_refs)) {
                self.log_error(&format!("The repository mirror cannot watch this checkout's refs: {error:#}"));
            }
        }
        let hub = Arc::new(HubCtx {
            logins: LoginPaths { claude_dir: self.opts.claude_dir.clone(), codex_dir: self.opts.codex_dir.clone() },
            sync_logins: config.sync_logins,
            refs: Arc::clone(&self.mirror_refs),
        });
        let wanted = config.hosts;
        self.restored_host_outages.retain(|(saved, _)| wanted.contains(saved));
        let before = self.hosts.len();
        self.hosts.retain(|tag, slot| wanted.iter().any(|h| h.tag == *tag && *h == slot.config));
        let mut changed = self.hosts.len() != before;
        let missing: Vec<HostConfig> = wanted.into_iter().filter(|h| !self.hosts.contains_key(&h.tag)).collect();
        if !missing.is_empty() {
            let key = match hosts::hub_key(&self.opts.state_dir) {
                Ok(key) => key,
                Err(err) => {
                    self.log_error(&format!("Could not read the hub key: {err:#}"));
                    return;
                }
            };
            for host in missing {
                let stop = Arc::new(AtomicBool::new(false));
                link::spawn(host.clone(), key.clone(), self.tx.clone(), Arc::clone(&stop), Arc::clone(&hub));
                let outage = self.take_restored_host_outage(&host);
                self.hosts.insert(
                    host.tag,
                    HostSlot {
                        config: host,
                        stop,
                        out: None,
                        welcome: None,
                        state: None,
                        reason: outage.reason.clone().or_else(|| Some("connecting".into())),
                        stats: None,
                        reached: false,
                        outage,
                    },
                );
                changed = true;
            }
        }
        if changed {
            for c in self.clients.values_mut() {
                c.upstreams.retain(|tag, _| self.hosts.contains_key(tag));
            }
            self.hosts_changed();
            self.state_changed();
        }
    }

    /// Keep this host's peer listener running while `peer_listen` is set.
    pub(super) fn supervise_peer(&mut self, now: Instant) {
        if self.opts.config_file.is_none()
            || self.shutdown_at.is_some()
            || self.updating.is_some()
            || self.peer_checked.is_some_and(|at| now.duration_since(at) < PEER_CHECK_INTERVAL)
        {
            return;
        }
        self.peer_checked = Some(now);
        if crate::client::coordinator::listener(&self.current_config()).is_none() || peer::running(&self.opts.state_dir)
        {
            return;
        }
        if self.peer_started.is_some_and(|at| now.duration_since(at) < PEER_RETRY) {
            return;
        }
        self.peer_started = Some(now);
        if let Err(err) = crate::remote::spawn_detached_as(&super::current_exe_clean(), &self.opts.state_dir, "peer") {
            self.log_error(&format!("Could not start the peer listener: {err:#}"));
        }
    }

    /// The hosts as decks see them.
    pub(super) fn host_infos(&self) -> Vec<HostInfo> {
        self.hosts
            .values()
            .map(|slot| HostInfo {
                name: slot.config.name.clone(),
                label: slot.label(),
                online: slot.connected(),
                reason: if slot.connected() { None } else { slot.reason.clone() },
                start_dir: slot.welcome.as_ref().map(|w| w.start_dir.clone()).unwrap_or_default(),
                stats: slot.stats.clone(),
                role: slot.config.role,
            })
            .collect()
    }

    fn hosts_msg(&self) -> Vec<u8> {
        encode(&ServerMsg::Hosts { hosts: self.host_infos() })
    }

    /// Send the hosts to deck `cid` if it can read them.
    pub(super) fn send_hosts(&self, cid: u64) {
        if self.hosts.is_empty() {
            return;
        }
        if let Some(c) = self.clients.get(&cid).filter(|c| c.version.is_some_and(|v| v >= 7)) {
            let _ = c.out.send(Out::Msg(self.hosts_msg()));
        }
    }

    /// Send every deck that reads them the hosts, when they changed.
    fn hosts_changed(&mut self) {
        let buf = self.hosts_msg();
        if buf == self.last_hosts {
            return;
        }
        self.last_hosts = buf.clone();
        for c in self
            .clients
            .values()
            .filter(|c| c.version.is_some_and(|v| v >= 7) && !matches!(c.role, ClientRole::Hook | ClientRole::Peer))
        {
            let _ = c.out.send(Out::Msg(buf.clone()));
        }
    }

    /// Send the merged state now if it changed.
    pub(super) fn state_changed(&mut self) {
        self.state_builds += 1;
        let state = self.build_state();
        if state != *self.last_state {
            let state = Arc::new(state);
            self.last_state = state.clone();
            self.broadcast_state(&state);
        }
    }

    /// Add every reachable host's sessions, outside processes, usage and
    /// errors to the hub's own.
    pub(super) fn merge_hosts(&self, state: &mut DeckState) {
        let mut errors = Vec::new();
        for slot in self.hosts.values() {
            let Some(host) = slot.state.as_ref().filter(|_| slot.out.is_some()) else { continue };
            state.host_schedulers.insert(slot.config.name.clone(), host.scheduler.clone());
            state.providers.extend(host.providers.iter().take(64).cloned());
            state.attention.messages.extend(host.attention.messages.iter().cloned());
            state.attention.queued.extend(host.attention.queued.iter().map(|(id, n)| (*id, *n)));
            state.attention.supervisor.audit.extend(host.attention.supervisor.audit.iter().cloned());
            state.sessions.extend(host.sessions.iter().cloned());
            for job in &host.jobs {
                let local = crate::protocol::split_id(job.id).1;
                if !self.job_sessions.as_ref().is_some_and(|j| {
                    j.records.get(&local).is_some_and(|r| {
                        r.executor.as_ref().is_some_and(|e| e.host.as_deref() == Some(slot.config.name.as_str()))
                    })
                }) {
                    state.jobs.push(job.clone());
                }
            }
            state.outside.extend(host.outside.iter().cloned());
            state.scans.extend(host.scans.iter().take(super::jobs::MAX_LISTED).cloned());
            fed::merge_usage(&mut state.usage, &host.usage);
            errors.push((slot.label(), host.errors.clone()));
        }
        state.attention.supervisor.audit.sort_by_key(|entry| (entry.at, entry.session));
        let excess = state.attention.supervisor.audit.len().saturating_sub(4096);
        state.attention.supervisor.audit.drain(..excess);
        state.scans.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
        if !errors.is_empty() {
            state.errors = fed::merge_errors(std::mem::take(&mut state.errors), &errors, super::ERROR_LOG_LEN);
        }
    }

    pub(super) fn tag_of(&self, name: &str) -> Option<u16> {
        self.hosts.values().find(|s| s.config.name == name).map(|s| s.config.tag)
    }

    /// Send `msg` to host `tag` on deck `cid`'s own upstream, opening it
    /// first when needed.
    pub(super) fn upstream_send(&mut self, cid: u64, tag: u16, msg: &ClientMsg) {
        let Some(slot) = self.hosts.get(&tag) else {
            self.fail_job_routes(Some(cid), Some(tag), "Job executor no longer configured");
            if let ClientMsg::SendMessage { id, request, .. } = msg {
                self.send(
                    cid,
                    &ServerMsg::MessageSent {
                        id: crate::protocol::host_id(tag, *id),
                        request: *request,
                        error: Some("That host is no longer configured.".into()),
                        delivered: None,
                    },
                );
            } else {
                self.notice(cid, "That host is no longer configured.", true);
            }
            return;
        };
        let native_new = matches!(msg, ClientMsg::Create { tool: crate::protocol::Tool::Codex, .. });
        if native_new && slot.welcome.as_ref().is_none_or(|welcome| welcome.version < 19) {
            self.notice(cid, "Update needed: native Codex creation requires a protocol-19 source host", true);
            return;
        }
        if slot.out.is_none() {
            let why = slot.reason.clone().unwrap_or_else(|| "unreachable".into());
            let label = slot.label();
            let error = format!("{label}{OFFLINE}{why}");
            self.fail_job_routes(Some(cid), Some(tag), "Job executor offline; acceptance unknown");
            if let ClientMsg::SendMessage { id, request, .. } = msg {
                self.send(
                    cid,
                    &ServerMsg::MessageSent {
                        id: crate::protocol::host_id(tag, *id),
                        request: *request,
                        error: Some(error),
                        delivered: None,
                    },
                );
            } else {
                self.notice(cid, error, true);
            }
            return;
        }
        let config = slot.config.clone();
        let Some(c) = self.clients.get_mut(&cid) else { return };
        let out = c.upstreams.entry(tag).or_insert_with(|| {
            let role = if c.role == ClientRole::Tui { ClientRole::Tui } else { ClientRole::Control };
            let key = hosts::existing_hub_key(&self.opts.state_dir).unwrap_or_default();
            link::upstream(cid, config, key, (role, c.version.unwrap_or(0)), self.tx.clone())
        });
        let failed = out.send(encode(msg)).is_err();
        if failed {
            self.fail_job_routes(Some(cid), Some(tag), "Job executor link closed; acceptance unknown");
        }
    }

    /// Send `msg` on every upstream deck `cid` already has.
    fn upstreams_all(&self, cid: u64, msg: &ClientMsg) {
        if let Some(c) = self.clients.get(&cid) {
            let buf = encode(msg);
            for out in c.upstreams.values() {
                let _ = out.send(buf.clone());
            }
        }
    }

    /// Pass what deck `cid` sent to the host it names; the message comes
    /// back when the hub's own service handles it too.
    /// Whether the host named `name` answers on a build older than protocol
    /// `version`.
    fn host_older_than(&self, name: &str, version: u32) -> bool {
        self.tag_of(name)
            .and_then(|tag| self.hosts.get(&tag))
            .and_then(|s| s.welcome.as_ref())
            .is_some_and(|w| w.version < version)
    }

    pub(super) fn route_client(&mut self, cid: u64, mut msg: ClientMsg) -> Option<ClientMsg> {
        if let ClientMsg::Plugin { request, plugin, command, args } = &msg {
            if plugin.starts_with("provider-")
                && ((command == "target" && matches!(args["operation"].as_str(), Some("send" | "broadcast" | "clear")))
                    || (plugin == "provider-codex" && command == "control"))
            {
                if let Some(name) = args["host"].as_str() {
                    let slot = self.tag_of(name).and_then(|tag| self.hosts.get(&tag));
                    let correlated =
                        command == "target" && args["operation"] == "send" && args["client_id"].is_string();
                    let error = match slot {
                        Some(slot) if correlated && slot.welcome.as_ref().is_none_or(|welcome| welcome.version < 21) => Some("Update needed: source host must support protocol 21 steering receipts; nothing was sent"),
                        None => Some("The target host is no longer configured; nothing was sent"),
                        Some(slot) if slot.out.is_none() => Some("The target host is offline; nothing was sent"),
                        Some(slot) if slot.welcome.as_ref().is_none_or(|welcome| welcome.version < crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL) => Some("Update needed: source host must support protocol 19 native owner messages; nothing was sent"),
                        _ => None,
                    };
                    if let Some(error) = error {
                        self.send(
                            cid,
                            &ServerMsg::PluginResult {
                                request: *request,
                                data: serde_json::Value::Null,
                                error: Some(error.into()),
                            },
                        );
                        return None;
                    }
                }
            }
        }
        let native_target = match &msg {
            ClientMsg::SendMessage { id, .. } | ClientMsg::SendNow { id } => Some(*id),
            _ => None,
        };
        if let Some(id) = native_target {
            let tag = split_id(id).0;
            if tag != 0
                && self
                    .hosts
                    .get(&tag)
                    .and_then(|h| h.welcome.as_ref())
                    .is_none_or(|w| w.version < crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL)
            {
                let error = Some(
                    "Update needed: source host must support protocol 19 native owner messages; nothing was sent"
                        .into(),
                );
                let reply = match &msg {
                    ClientMsg::SendMessage { request, .. } => {
                        ServerMsg::MessageSent { id, request: *request, error, delivered: None }
                    }
                    _ => ServerMsg::SentNow { id, error },
                };
                self.send(cid, &reply);
                return None;
            }
        }
        if let ClientMsg::History { id, request, epoch, cols, rows } = &msg {
            let client_version = self.clients.get(&cid).and_then(|c| c.version).unwrap_or(0);
            let (tag, _) = split_id(*id);
            let source_version = if tag == 0 {
                PROTOCOL_VERSION
            } else {
                self.hosts.get(&tag).and_then(|s| s.welcome.as_ref()).map_or(0, |w| w.version)
            };
            if client_version < crate::screen::HISTORY_PROTOCOL || source_version < crate::screen::HISTORY_PROTOCOL {
                let error = "Local history needs protocol 18 on both the deck and source host".to_string();
                if client_version >= crate::screen::HISTORY_PROTOCOL {
                    self.send(
                        cid,
                        &ServerMsg::History {
                            id: *id,
                            request: *request,
                            epoch: *epoch,
                            cols: *cols,
                            rows: *rows,
                            snapshot: None,
                            error: Some(error),
                        },
                    );
                } else {
                    self.notice(cid, error, true);
                }
                return None;
            }
        }
        if let ClientMsg::Fleet { request, action, args } = &msg {
            if crate::work_queue::is_action(action) && args.get("host").is_some() {
                self.send(cid,&ServerMsg::FleetResult{request:*request,data:serde_json::Value::Null,error:Some("Queues, sources and chores belong to the connected endpoint; connect directly to their originating service".into())});
                return None;
            }
        }
        if let ClientMsg::Fleet { request, action, args } = &mut msg {
            if crate::attention::is_action(action) {
                if let Err(error) = self.attention_route_target(action, args) {
                    self.send(
                        cid,
                        &ServerMsg::FleetResult {
                            request: *request,
                            data: serde_json::Value::Null,
                            error: Some(error.to_string()),
                        },
                    );
                    return None;
                }
            }
        }
        if let ClientMsg::Fleet { request, action, args } = &mut msg {
            if action.starts_with("job-") {
                if let Some(original) = args["job"].as_u64() {
                    let (tag, local) = split_id(original);
                    let target = if tag != 0 {
                        self.hosts.get(&tag).map(|s| s.config.name.clone())
                    } else if action == "job-log" {
                        self.job_sessions
                            .as_ref()
                            .and_then(|j| j.records.get(&local))
                            .and_then(|r| r.executor.as_ref())
                            .and_then(|e| e.host.clone())
                    } else {
                        None
                    };
                    if tag != 0 && target.is_none() {
                        self.send(
                            cid,
                            &ServerMsg::FleetResult {
                                request: *request,
                                data: serde_json::Value::Null,
                                error: Some("Job belongs to an unknown host".into()),
                            },
                        );
                        return None;
                    }
                    if let Some(host) = target {
                        if args["host"].as_str().is_some_and(|name| name != host) || self.host_older_than(&host, 14) {
                            self.send(
                                cid,
                                &ServerMsg::FleetResult {
                                    request: *request,
                                    data: serde_json::Value::Null,
                                    error: Some("Update needed: jobs require protocol 14 on the selected host".into()),
                                },
                            );
                            return None;
                        }
                        if self.job_routes.len() >= 4096 {
                            self.send(
                                cid,
                                &ServerMsg::FleetResult {
                                    request: *request,
                                    data: serde_json::Value::Null,
                                    error: Some("Too many pending job requests".into()),
                                },
                            );
                            return None;
                        }
                        let tag = self.tag_of(&host).unwrap();
                        if self.hosts[&tag].out.is_none() {
                            self.send(
                                cid,
                                &ServerMsg::FleetResult {
                                    request: *request,
                                    data: serde_json::Value::Null,
                                    error: Some("Job executor offline; retry the same request after reconnect".into()),
                                },
                            );
                            return None;
                        }
                        self.job_routes
                            .insert((cid, *request), super::JobRoute { original, host: tag, at: Instant::now() });
                        args["host"] = serde_json::json!(host);
                        args["job"] = serde_json::json!(local);
                    }
                }
            }
        }
        if let ClientMsg::Create { host, .. } = &mut msg {
            // "@here" names this machine explicitly.
            if host.as_deref() == Some("@here") {
                *host = None;
            }
        }
        // A start sent on to another host runs with this deck's launch
        // settings, as one started here does.
        if let ClientMsg::Create { tool, model, effort, host: Some(_), .. } = &mut msg {
            if tool.is_agent() {
                let asked = StartModel { model: model.take(), effort: effort.take() };
                match self.launch_start(*tool, &asked, None) {
                    Ok(start) => (*model, *effort) = (start.model, start.effort),
                    Err(error) => {
                        self.notice(cid, format!("Could not start {}: {error:#}", tool.label()), true);
                        return None;
                    }
                }
            }
        }
        if let ClientMsg::Fleet { request, args, .. } = &mut msg {
            if let Some(session) = args["session"].as_u64() {
                let (tag, local) = split_id(session);
                if tag != 0 {
                    let selected = args["host"].as_str().and_then(|name| self.tag_of(name));
                    let valid = selected.is_none_or(|selected| selected == tag) && self.hosts.contains_key(&tag);
                    if !valid {
                        self.send(
                            cid,
                            &ServerMsg::FleetResult {
                                request: *request,
                                data: serde_json::Value::Null,
                                error: Some("Session belongs to another host".into()),
                            },
                        );
                        return None;
                    }
                    args["session"] = serde_json::json!(local);
                    if args["host"].is_null() {
                        args["host"] = serde_json::json!(self.hosts[&tag].config.name);
                    }
                }
            }
        }
        match msg {
            ClientMsg::Hello { .. } | ClientMsg::Hook { .. } => Some(msg),
            ClientMsg::Fleet { request, action, args }
                if args.get("host").and_then(|v| v.as_str()).is_some_and(|h| self.host_older_than(h, 14)) =>
            {
                self.send(
                    cid,
                    &ServerMsg::FleetResult {
                        request,
                        data: serde_json::Value::Null,
                        error: Some("Update needed: remote scheduler requires protocol 14".into()),
                    },
                );
                let _ = action;
                None
            }
            ClientMsg::Attach { id, cols, rows } => {
                let target = id.map(split_id).filter(|(tag, _)| *tag != 0);
                if let Some(c) = self.clients.get_mut(&cid) {
                    c.pending_frame = None;
                }
                let tags: Vec<u16> =
                    self.clients.get(&cid).map(|c| c.upstreams.keys().copied().collect()).unwrap_or_default();
                for tag in tags.into_iter().filter(|t| target.is_none_or(|(want, _)| want != *t)) {
                    self.upstream_send(cid, tag, &ClientMsg::Attach { id: None, cols, rows });
                }
                if let Some((tag, local)) = target {
                    self.upstream_send(cid, tag, &ClientMsg::Attach { id: Some(local), cols, rows });
                }
                Some(ClientMsg::Attach { id, cols, rows })
            }
            ClientMsg::Resize { .. } | ClientMsg::Focus { .. } => {
                self.upstreams_all(cid, &msg);
                Some(msg)
            }
            // : each host that can restart in place (protocol 3) does, then this one.
            ClientMsg::Restart | ClientMsg::SyncSkills => {
                let since = if matches!(msg, ClientMsg::Restart) { 3 } else { 9 };
                let buf = encode(&msg);
                let mut skipped = Vec::new();
                for slot in self.hosts.values() {
                    match (slot.out.as_ref(), slot.welcome.as_ref()) {
                        (Some(out), Some(w)) if w.version >= since => {
                            let _ = out.send(buf.clone());
                        }
                        (Some(_), Some(_)) => skipped.push(slot.label()),
                        _ => {}
                    }
                }
                if !skipped.is_empty() {
                    let what = if since == 3 { "restart in place" } else { "link the deck's skills" };
                    self.notice(cid, format!("{} run an older build that cannot {what}.", skipped.join(", ")), true);
                }
                Some(msg)
            }
            // A host on an older build would drop the link on a message it
            // cannot read.
            ClientMsg::Install { host: Some(ref name), .. } | ClientMsg::SecurityScan { host: Some(ref name), .. }
                if self.host_older_than(name, 9) =>
            {
                self.notice(cid, format!("{name} runs an older build; update it to install or scan there."), true);
                None
            }
            ClientMsg::ResizeTerminal { id, .. }
                if self.hosts.get(&split_id(id).0).and_then(|s| s.welcome.as_ref()).is_some_and(|w| w.version < 10) =>
            {
                self.notice(cid, "That host runs an older build; update it to resize a shell separately.", true);
                None
            }
            ClientMsg::ChangeDirectory { id, .. }
                if self.hosts.get(&split_id(id).0).and_then(|s| s.welcome.as_ref()).is_some_and(|w| w.version < 14) =>
            {
                self.send(
                    cid,
                    &ServerMsg::DirectoryChanged {
                        id,
                        cwd: None,
                        error: Some("That host needs an updated deck service to change directories.".into()),
                    },
                );
                None
            }
            ClientMsg::Broadcast { ids, text, running_only } => {
                if running_only {
                    self.start_fleet_broadcast(cid, &ids, &text);
                    return None;
                }
                let mut local = None;
                for (tag, ids) in fed::split_ids(&ids) {
                    if tag == 0 {
                        local = Some(ClientMsg::Broadcast { running_only, ids, text: text.clone() });
                    } else {
                        if self
                            .hosts
                            .get(&tag)
                            .and_then(|slot| slot.welcome.as_ref())
                            .is_none_or(|welcome| welcome.version < crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL)
                        {
                            self.notice(cid, "Update needed: source host must support protocol 19 native owner messages; nothing was sent to that host", true);
                            continue;
                        }
                        let message = ClientMsg::Broadcast { running_only, ids: ids.clone(), text: text.clone() };
                        self.upstream_send(cid, tag, &message);
                    }
                }
                local
            }
            ClientMsg::ListConversations { query, request } => {
                if !query.trim().is_empty() {
                    if let Err(error) = crate::remote::search::words(&query) {
                        self.notice(cid, error.to_string(), true);
                        self.send(cid, &ServerMsg::Conversations { request, query: Some(query), items: Vec::new() });
                        return None;
                    }
                }
                let online: HashSet<u16> =
                    self.hosts.iter().filter(|(_, s)| s.out.is_some()).map(|(tag, _)| *tag).collect();
                for &tag in &online {
                    self.upstream_send(cid, tag, &ClientMsg::ListConversations { request, query: query.clone() });
                }
                let mut waiting = online;
                waiting.insert(0);
                self.pending_lists.insert(
                    cid,
                    PendingList {
                        request,
                        query: query.clone(),
                        items: Vec::new(),
                        waiting,
                        until: Instant::now() + LIST_WAIT,
                    },
                );
                Some(ClientMsg::ListConversations { query, request })
            }
            // A host on an older build would drop the link on a message it
            // cannot read.
            ClientMsg::PasteFile { id, .. }
                if self.hosts.get(&split_id(id).0).and_then(|s| s.welcome.as_ref()).is_some_and(|w| w.version < 8) =>
            {
                let label = self.hosts.get(&split_id(id).0).map(HostSlot::label).unwrap_or_default();
                self.notice(cid, format!("{label} runs an older build that cannot take a pasted image."), true);
                None
            }
            ClientMsg::SendMessage { id, request, .. }
                if self.hosts.get(&split_id(id).0).and_then(|s| s.welcome.as_ref()).is_some_and(|w| w.version < 14) =>
            {
                self.send(
                    cid,
                    &ServerMsg::MessageSent {
                        id,
                        request,
                        error: Some("Update that host before sending a message with delivery receipts".into()),
                        delivered: None,
                    },
                );
                None
            }
            ClientMsg::SendNow { id }
                if self.hosts.get(&split_id(id).0).and_then(|s| s.welcome.as_ref()).is_some_and(|w| w.version < 10) =>
            {
                let label = self.hosts.get(&split_id(id).0).map(HostSlot::label).unwrap_or_default();
                let error = Some(format!("{label} runs an older build that cannot send a message now"));
                self.send(cid, &ServerMsg::SentNow { id, error });
                None
            }
            other => match fed::route(other, |name| self.tag_of(name)) {
                fed::Route::Local(msg) => Some(msg),
                fed::Route::Host(tag, msg) => {
                    self.upstream_send(cid, tag, &msg);
                    None
                }
                fed::Route::UnknownHost(name) => {
                    self.notice(cid, format!("No host is named \u{201c}{name}\u{201d}."), true);
                    None
                }
            },
        }
    }

    /// A part of deck `cid`'s conversation list, from the hub (tag 0) or a
    /// host; the whole list goes out once every part is in.
    pub(super) fn conversations_part(
        &mut self,
        cid: u64,
        tag: u16,
        query: Option<String>,
        request: Option<u64>,
        items: Vec<Conversation>,
    ) {
        let Some(pending) = self.pending_lists.get_mut(&cid) else {
            if tag == 0 {
                self.send(cid, &ServerMsg::Conversations { query, request, items });
            }
            return;
        };
        if request.is_some_and(|request| Some(request) != pending.request)
            || query.as_ref().is_some_and(|query| query != &pending.query)
        {
            return;
        }
        pending.items.extend(items);
        pending.waiting.remove(&tag);
        if pending.waiting.is_empty() || (!pending.waiting.contains(&0) && Instant::now() >= pending.until) {
            self.finish_list(cid);
        }
    }

    fn finish_list(&mut self, cid: u64) {
        if let Some(mut pending) = self.pending_lists.remove(&cid) {
            pending.items.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
            self.send(
                cid,
                &ServerMsg::Conversations {
                    request: pending.request,
                    query: Some(pending.query),
                    items: pending.items,
                },
            );
        }
    }

    /// Send the lists whose slow hosts ran out of time.
    pub(super) fn lists_due(&mut self, now: Instant) {
        let due: Vec<u64> = self
            .pending_lists
            .iter()
            .filter(|(_, p)| now >= p.until && !p.waiting.contains(&0))
            .map(|(cid, _)| *cid)
            .collect();
        for cid in due {
            self.finish_list(cid);
        }
    }

    /// Frames from hosts that waited for their deck to take the last one.
    pub(super) fn pump_host_frames(&mut self) {
        for c in self.clients.values_mut() {
            if c.pending_frame.is_some() && !c.frame_busy.load(Ordering::SeqCst) {
                if let Some(buf) = c.pending_frame.take() {
                    c.frame_busy.store(true, Ordering::SeqCst);
                    let _ = c.out.send(Out::Frame(buf));
                }
            }
        }
    }

    pub(super) fn on_host(&mut self, ev: HostEv) {
        self.on_host_at(ev, Instant::now());
    }

    fn on_host_at(&mut self, ev: HostEv, now: Instant) {
        match ev {
            HostEv::Up { tag, out, welcome } => {
                let Some(slot) = self.hosts.get_mut(&tag) else { return };
                let recovered = slot.outage.reported;
                slot.reason = (welcome.version < PROTOCOL_VERSION).then(|| {
                    format!(
                        "runs an older build (protocol {}); reinstall it with `cepheus hosts install`",
                        welcome.version
                    )
                });
                slot.out = Some(out);
                slot.outage = HostOutage::default();
                slot.welcome = Some(welcome);
                let label = slot.label();
                if recovered {
                    self.broadcast(&ServerMsg::Notice { text: format!("{label} reconnected."), error: false });
                }
                self.sync_error_clear(tag);
                self.hosts_changed();
                self.retire_connection_errors(&label);
            }
            HostEv::Down { tag, reason } => {
                let Some(slot) = self.hosts.get_mut(&tag) else { return };
                slot.out = None;
                slot.reached = false;
                slot.state = None;
                if !reason.is_empty() {
                    slot.reason = Some(reason);
                }
                slot.outage.reason = slot.reason.clone();
                // Repeated failed attempts must not postpone the first loss.
                slot.outage.since.get_or_insert(now);
                self.hosts_changed();
                self.state_changed();
            }
            HostEv::Stats { tag, stats } => {
                let Some(slot) = self.hosts.get_mut(&tag) else { return };
                slot.stats = Some(stats);
                if !slot.config.runs_agents() && !slot.reached {
                    let recovered = slot.outage.reported;
                    let label = slot.label();
                    slot.reached = true;
                    slot.reason = None;
                    slot.outage = HostOutage::default();
                    if recovered {
                        self.broadcast(&ServerMsg::Notice { text: format!("{label} reconnected."), error: false });
                    }
                    self.sync_error_clear(tag);
                    self.retire_connection_errors(&label);
                }
                self.hosts_changed();
            }
            HostEv::Msg { tag, msg } => self.on_host_msg(tag, msg),
            HostEv::UpMsg { cid, tag, msg } => self.on_upstream_msg(cid, tag, msg),
            HostEv::UpFrame { cid, tag, buf } => {
                let Some(c) = self.clients.get_mut(&cid) else { return };
                if c.attached.map(split_id).is_none_or(|(t, _)| t != tag) {
                    return;
                }
                if c.frame_busy.load(Ordering::SeqCst) {
                    c.pending_frame = Some(buf);
                } else {
                    // A newer frame replaces any older one queued while busy.
                    c.pending_frame = None;
                    c.frame_busy.store(true, Ordering::SeqCst);
                    let _ = c.out.send(Out::Frame(buf));
                }
            }
            HostEv::Report { tag, text } => {
                if self.hosts.contains_key(&tag) {
                    self.announce_error(text);
                }
            }
            HostEv::Withdraw { tag, text } => {
                if self.hosts.contains_key(&tag) {
                    self.withdraw_errors(&[text]);
                }
            }
            HostEv::UpGone { cid, tag, reason } => {
                let broadcast_failed = self.fail_broadcast_host(
                    cid,
                    tag,
                    "connection lost; delivery unconfirmed. Check the agents before retrying",
                );
                self.fail_job_routes(Some(cid), Some(tag), "Job executor disconnected; acceptance unknown");
                let had = self.clients.get_mut(&cid).and_then(|c| c.upstreams.remove(&tag)).is_some();
                if let Some(pending) = self.pending_lists.get_mut(&cid) {
                    pending.waiting.remove(&tag);
                    if pending.waiting.is_empty() {
                        self.finish_list(cid);
                    }
                }
                if had && !reason.is_empty() && !broadcast_failed {
                    let label = self.hosts.get(&tag).map_or_else(|| "A host".to_string(), HostSlot::label);
                    self.notice(cid, format!("{label}: {reason}"), true);
                }
            }
        }
    }

    fn on_host_msg(&mut self, tag: u16, msg: ServerMsg) {
        if !self.hosts.contains_key(&tag) {
            return;
        }
        if let ServerMsg::State(state) = &msg {
            self.clear_errors(Some(state.error_clears.clone()));
        }
        let Some(slot) = self.hosts.get_mut(&tag) else { return };
        let (name, label) = (slot.config.name.clone(), slot.label());
        if let ServerMsg::FleetResult { request, data, error } = &msg {
            if request & (1u64 << 60) != 0
                && self.attention_decision_peer_result(&name, *request, data, error.as_deref())
            {
                return;
            }
        }
        let Some(slot) = self.hosts.get_mut(&tag) else { return };
        match fed::inbound(tag, &name, &label, msg) {
            ServerMsg::State(mut state) => {
                if slot.welcome.as_ref().is_some_and(|w| w.version >= 14)
                    && !super::errors::includes(&state.error_clears, &self.error_clears)
                {
                    state.errors.clear();
                    self.sync_error_clear(tag);
                }
                let slot = self.hosts.get_mut(&tag).unwrap();
                slot.state = Some(state);
                self.state_changed();
            }
            ServerMsg::Alert(alert) => {
                // A refused owner message also marks the phone's own copy,
                // so it is relayed even where the host's error list is merged.
                if alert.kind == AlertKind::Error
                    && alert.message.is_none()
                    && slot.welcome.as_ref().is_some_and(|w| w.version >= 14)
                    && !slot
                        .state
                        .as_ref()
                        .is_some_and(|state| super::errors::includes(&state.error_clears, &self.error_clears))
                {
                    return;
                }
                let who = alert
                    .id
                    .and_then(|id| slot.state.as_ref()?.sessions.iter().find(|s| s.id == id))
                    .map_or_else(|| label.clone(), |s| format!("{} on {label}", s.tool.label()));
                self.alert_remote_for(alert.kind, alert.id, &alert.title, &alert.reason, alert.message.as_deref());
                match alert.kind {
                    AlertKind::NeedsYou if self.opts.config.notifications => {
                        external::notify(&format!("{who} needs you"), &format!("{}\n{}", alert.title, alert.reason));
                    }
                    AlertKind::Finished if self.opts.config.notifications => {
                        external::notify(&format!("{who} finished"), &alert.title);
                    }
                    AlertKind::Error => {
                        self.broadcast(&ServerMsg::Notice { text: format!("{label}: {}", alert.reason), error: true });
                    }
                    _ => {}
                }
            }
            ServerMsg::FleetResult { request, data, error } => {
                if self.attention_peer_result(&name, request, &data, error.as_deref()) {
                    return;
                }
                if self.work_peer_result(&name, tag, request, &data, error.as_deref()) {
                    return;
                }
                if self.job_peer_result(&name, request, &data, error.as_deref()) {
                    return;
                }
                if let Some(item) = self
                    .scheduler
                    .starts
                    .iter_mut()
                    .find(|s| s.id == request && s.target.as_deref() == Some(name.as_str()))
                {
                    if let Some(error) = error {
                        item.reason = error;
                        item.attempted_at = Some(crate::util::now_secs());
                    } else if let Some(id) = data["session"].as_u64() {
                        item.session = Some(crate::protocol::host_id(tag, id));
                        item.reason = "started".into();
                        item.reset = None;
                    } else if data["held"].as_bool() == Some(true) {
                        item.reason = data["reason"].as_str().unwrap_or("held on peer").into();
                        item.reset = data["reset"].as_u64();
                        item.attempted_at = Some(crate::util::now_secs());
                    }
                    let _ = self.scheduler.save();
                    self.state_changed();
                }
            }
            // Notices also reach the deck that caused them, on its upstream.
            _ => {}
        }
    }

    fn on_upstream_msg(&mut self, cid: u64, tag: u16, msg: ServerMsg) {
        if let ServerMsg::BroadcastDone { request, sent, failed } = msg {
            self.broadcast_part_done(cid, request, tag, sent, failed);
            return;
        }
        let Some(slot) = self.hosts.get(&tag) else { return };
        let (name, label) = (slot.config.name.clone(), slot.label());
        match fed::inbound(tag, &name, &label, msg) {
            ServerMsg::Conversations { query, request, items } => {
                self.conversations_part(cid, tag, query, request, items)
            }
            ServerMsg::State(_)
            | ServerMsg::StatePatch(_)
            | ServerMsg::Welcome { .. }
            | ServerMsg::Alert(_)
            | ServerMsg::Hosts { .. } => {}
            ServerMsg::FleetResult { request, mut data, error } => {
                if let Some(route) = self.job_routes.remove(&(cid, request)) {
                    let original = route.original;
                    if data["job"].is_number() {
                        data["job"] = serde_json::json!(original);
                    } else if data["job"].is_object() {
                        data["job"]["id"] = serde_json::json!(original);
                    }
                    if let Some(jobs) = data["jobs"].as_array_mut() {
                        for job in jobs {
                            job["id"] = serde_json::json!(original);
                        }
                    }
                }
                self.send(cid, &ServerMsg::FleetResult { request, data, error });
            }
            other => self.send(cid, &other),
        }
    }
}

/// A private frozen authenticated endpoint and its explicitly registered repository mapping.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct AttentionRepositoryPeer {
    pub host: HostConfig,
    pub repository: String,
    pub directory: PathBuf,
}
impl AttentionRepositoryPeer {
    pub(super) fn global_session(&self, local: u64) -> anyhow::Result<u64> {
        anyhow::ensure!(local != 0 && split_id(local).0 == 0, "invalid endpoint-local session");
        anyhow::ensure!((1..=crate::protocol::MAX_HOST_TAG).contains(&self.host.tag), "invalid peer tag");
        Ok(crate::protocol::host_id(self.host.tag, local))
    }
}

// One immutable request per host and coordinator revision. The private journal survives
// reconnect/restart; a lost ACK never creates another authority-bearing request identity.
#[derive(Clone, Serialize, Deserialize)]
struct AttentionFleetOff {
    origin: String,
    generation: u64,
    peers: BTreeMap<String, AttentionOffPeer>,
}
#[derive(Clone, Serialize, Deserialize)]
struct AttentionOffPeer {
    host: HostConfig,
    request: u64,
    acked: bool,
}
impl AttentionFleetOff {
    fn validate(&self, origin: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.origin == origin && self.generation > 0 && self.generation < (1u64 << 50),
            "invalid fleet off generation"
        );
        anyhow::ensure!(self.peers.len() <= usize::from(crate::protocol::MAX_HOST_TAG), "too many fleet off peers");
        let mut tags = HashSet::new();
        for (name, peer) in &self.peers {
            anyhow::ensure!(
                name == &peer.host.name
                    && (1..=crate::protocol::MAX_HOST_TAG).contains(&peer.host.tag)
                    && tags.insert(peer.host.tag)
                    && peer.request == ((1u64 << 61) | (self.generation << 10) | u64::from(peer.host.tag)),
                "invalid fleet off binding"
            );
        }
        Ok(())
    }
    fn pending(&self) -> Vec<String> {
        self.peers.iter().filter(|(_, peer)| !peer.acked).map(|(name, _)| name.clone()).collect()
    }
}

impl Service {
    pub(super) fn attention_fleet_snapshot(&self) -> crate::attention::Snapshot {
        // own_state uses the LOCAL snapshot, so this does not recurse through build_state.
        self.build_state().attention
    }

    fn attention_route_target(&self, action: &str, args: &mut serde_json::Value) -> anyhow::Result<()> {
        anyhow::ensure!(
            !matches!(action, "attention-message" | "attention-cancel") || args.get("session").is_none(),
            "Message requests require an origin and UUID, not a session selector"
        );
        let explicit = match args.get("host") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(host)) => Some(host.clone()),
            _ => anyhow::bail!("Invalid attention origin"),
        };
        let local_origin = explicit.as_deref() == Some("@here");
        if let Some(host) = explicit.as_deref().filter(|_| !local_origin) {
            anyhow::ensure!(self.tag_of(host).is_some(), "Attention origin is no longer configured");
        }
        if let Some(id) = args.get("session").and_then(serde_json::Value::as_u64) {
            let (tag, local) = split_id(id);
            if tag != 0 {
                let slot = self.hosts.get(&tag).context("Session belongs to an unknown host")?;
                anyhow::ensure!(
                    !local_origin && explicit.as_ref().is_none_or(|host| host == &slot.config.name),
                    "Session belongs to another host"
                );
                // Validate the complete identity before stripping any host bits.
                args["host"] = serde_json::json!(slot.config.name);
                args["session"] = serde_json::json!(local);
            }
        } else if matches!(action, "attention-message" | "attention-cancel") {
            let id = args["id"].as_str().context("Message identity required")?;
            if explicit.is_none() {
                let local = self.attention.as_ref().is_some_and(|a| a.store.messages.contains_key(id));
                let peer_collision = self.hosts.values().any(|slot| {
                    slot.state
                        .as_ref()
                        .is_some_and(|state| state.attention.messages.iter().any(|message| message.id == id))
                });
                anyhow::ensure!(
                    local && !peer_collision,
                    "Message origin required; retry the same host and message identity"
                );
            }
        }
        if local_origin {
            if let Some(object) = args.as_object_mut() {
                object.remove("host");
            }
        }
        if let Some(host) = args["host"].as_str() {
            let slot = self
                .tag_of(host)
                .and_then(|tag| self.hosts.get(&tag))
                .context("Attention origin is no longer configured")?;
            let minimum = if action == "attention-send" {
                17
            } else if action == "attention-supervisor-rules"
                && args["rules"]
                    .as_array()
                    .is_some_and(|rules| rules.iter().any(|rule| rule["pattern"]["kind"] == "native_exact"))
            {
                16
            } else {
                15
            };
            anyhow::ensure!(
                slot.out.is_some() && slot.welcome.as_ref().is_some_and(|welcome| welcome.version >= minimum),
                "Update needed or attention target offline; reconnect to the same executor"
            );
        }
        Ok(())
    }

    pub(super) fn attention_repository_peers(&self, cwd: &Path) -> anyhow::Result<Vec<AttentionRepositoryPeer>> {
        anyhow::ensure!(self.scheduler.error.is_none(), "scheduler recovery required for repository discovery");
        anyhow::ensure!(
            cwd.is_absolute() && cwd.canonicalize().is_ok_and(|path| path.is_dir()),
            "repository directory unavailable"
        );
        let Some((repository, _)) = self.scheduler.repository_for(cwd) else { return Ok(Vec::new()) };
        let local_root =
            self.scheduler.settings.repositories.get(&repository).context("repository no longer registered")?;
        self.scheduler.placed_directory(&repository, local_root)?;
        let now = crate::util::now_secs();
        let mut peers = Vec::new();
        for slot in self.hosts.values() {
            // Unknown/offline/stale registration is not proof of an empty recipient set.
            let state =
                slot.state.as_ref().context("peer repository discovery unavailable; retry same decision key")?;
            anyhow::ensure!(
                slot.out.is_some()
                    && state.scheduler.headroom.at <= now
                    && now.saturating_sub(state.scheduler.headroom.at) <= 30,
                "peer repository discovery offline or stale; retry same decision key"
            );
            let Some(directory) = state.scheduler.repositories.get(&repository) else { continue };
            anyhow::ensure!(
                slot.welcome.as_ref().is_some_and(|welcome| welcome.version >= 15) && state.attention_capable,
                "Update needed: registered peer attention unavailable"
            );
            let directory = PathBuf::from(directory);
            anyhow::ensure!(
                directory.is_absolute()
                    && directory.components().all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
                "invalid registered peer repository"
            );
            anyhow::ensure!((1..=crate::protocol::MAX_HOST_TAG).contains(&slot.config.tag), "invalid peer tag");
            peers.push(AttentionRepositoryPeer {
                host: slot.config.clone(),
                repository: repository.clone(),
                directory,
            });
        }
        peers.sort_by(|a, b| a.host.name.cmp(&b.host.name));
        Ok(peers)
    }

    pub(super) fn attention_peer_send(
        &self,
        peer: &AttentionRepositoryPeer,
        request: u64,
        action: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            request & (1u64 << 60) != 0 && request & (7u64 << 61) == 0,
            "invalid private attention request"
        );
        anyhow::ensure!(
            matches!(action, "attention-recipients" | "attention-send" | "attention-message" | "attention-cancel"),
            "unsupported private attention action"
        );
        let slot = self.hosts.get(&peer.host.tag).context("peer no longer configured")?;
        anyhow::ensure!(slot.config == peer.host, "peer binding changed; original endpoint required");
        let local_root =
            self.scheduler.settings.repositories.get(&peer.repository).context("repository no longer registered")?;
        self.scheduler.placed_directory(&peer.repository, local_root)?;
        let now = crate::util::now_secs();
        let state = slot.state.as_ref().context("peer state unavailable")?;
        anyhow::ensure!(
            slot.out.is_some()
                && slot
                    .welcome
                    .as_ref()
                    .is_some_and(|welcome| welcome.version >= if action == "attention-send" { 17 } else { 15 })
                && state.attention_capable
                && state.scheduler.headroom.at <= now
                && now.saturating_sub(state.scheduler.headroom.at) <= 30,
            "peer offline, stale or update needed; retry same endpoint"
        );
        anyhow::ensure!(
            state.scheduler.repositories.get(&peer.repository).is_some_and(|path| Path::new(path) == peer.directory),
            "registered peer repository mapping changed"
        );
        anyhow::ensure!(args["repository"].as_str() == Some(peer.repository.as_str()), "repository binding required");
        let message = ClientMsg::Fleet { request, action: action.into(), args };
        let encoded = encode(&message);
        anyhow::ensure!(encoded.len() <= 262144, "private attention request too large");
        slot.out.as_ref().context("peer offline")?.send(encoded).context("peer link closed")
    }

    pub(super) fn attention_fleet_off(&mut self) -> anyhow::Result<()> {
        let a = self.attention.as_mut().context("attention unavailable")?;
        let generation = a.store.saved.supervisor.revision;
        anyhow::ensure!(!a.store.saved.supervisor.enabled, "disable locally before fleet off");
        let path = a.store.root.join("fleet-off.json");
        if let Some(prior) = crate::job_sessions::store::optional::<AttentionFleetOff>(&path)? {
            prior.validate(&self.native_identity)?;
            anyhow::ensure!(generation >= prior.generation, "fleet off revision is older than its journal");
            if generation == prior.generation {
                // Reconcile the original immutable operation, including confirmed peers.
                a.store.saved.supervisor.pending = prior.pending();
                a.store.save()?;
                a.rpc_last = None;
                self.attention_sync_fleet_off();
                return Ok(());
            }
        }
        let operation = AttentionFleetOff {
            origin: self.native_identity.clone(),
            generation,
            peers: self
                .hosts
                .values()
                .map(|slot| {
                    (
                        slot.config.name.clone(),
                        AttentionOffPeer {
                            host: slot.config.clone(),
                            request: (1u64 << 61) | (generation << 10) | u64::from(slot.config.tag),
                            acked: false,
                        },
                    )
                })
                .collect(),
        };
        operation.validate(&self.native_identity)?;
        crate::attention::store::publish(&path, &operation)?;
        a.pending_rpc.clear();
        a.rpc_last = None;
        a.store.saved.supervisor.pending = operation.pending();
        a.store.save()?;
        self.attention_sync_fleet_off();
        Ok(())
    }
    pub(super) fn attention_sync_fleet_off(&mut self) {
        let Some(a) = &mut self.attention else { return };
        if a.rpc_last.is_some_and(|at| at.elapsed() < Duration::from_secs(2)) {
            return;
        }
        a.rpc_last = Some(Instant::now());
        let Ok(Some(operation)) =
            crate::job_sessions::store::optional::<AttentionFleetOff>(&a.store.root.join("fleet-off.json"))
        else {
            a.pending_rpc.clear();
            return;
        };
        if operation.validate(&self.native_identity).is_err()
            || operation.generation != a.store.saved.supervisor.revision
            || a.store.saved.supervisor.enabled
        {
            // A local enable/rule revision revokes old unsent retry authority. Keep unresolved status truthful.
            a.pending_rpc.clear();
            return;
        }
        a.store.saved.supervisor.pending = operation.pending();
        if a.store.save().is_err() {
            return;
        }
        a.pending_rpc.retain(|request, host| {
            operation.peers.get(host).is_some_and(|peer| !peer.acked && peer.request == *request)
        });
        for (host, peer) in &operation.peers {
            if peer.acked {
                continue;
            }
            let Some(slot) = self.hosts.get(&peer.host.tag).filter(|slot| {
                slot.config == peer.host && slot.welcome.as_ref().is_some_and(|welcome| welcome.version >= 15)
            }) else {
                continue;
            };
            let Some(out) = &slot.out else { continue };
            a.pending_rpc.insert(peer.request, host.clone());
            let _ = out.send(encode(&ClientMsg::Fleet {
                request: peer.request,
                action: "attention-supervisor-set".into(),
                args: serde_json::json!({"enabled":false,"origin":operation.origin,"generation":operation.generation}),
            }));
        }
    }
    fn attention_peer_result(
        &mut self,
        host: &str,
        request: u64,
        data: &serde_json::Value,
        error: Option<&str>,
    ) -> bool {
        let Some(a) = &mut self.attention else { return false };
        // An answer on the wrong authenticated link cannot consume another peer's pending request.
        if a.pending_rpc.get(&request).is_none_or(|expected| expected != host) {
            return false;
        }
        let path = a.store.root.join("fleet-off.json");
        let Ok(mut operation) = crate::job_sessions::store::read::<AttentionFleetOff>(&path) else { return true };
        if operation.validate(&self.native_identity).is_err()
            || operation.generation != a.store.saved.supervisor.revision
            || a.store.saved.supervisor.enabled
        {
            return true;
        }
        let Some(peer) = operation.peers.get_mut(host) else { return true };
        let valid_endpoint =
            self.hosts.get(&peer.host.tag).is_some_and(|slot| slot.config == peer.host && slot.out.is_some());
        if !valid_endpoint
            || peer.request != request
            || error.is_some()
            || data["supervisor"]["enabled"].as_bool() != Some(false)
            || data["off"]["origin"].as_str() != Some(operation.origin.as_str())
            || data["off"]["generation"].as_u64() != Some(operation.generation)
        {
            return true;
        }
        peer.acked = true;
        if crate::attention::store::publish(&path, &operation).is_err() {
            return true;
        }
        a.pending_rpc.remove(&request);
        a.store.saved.supervisor.pending = operation.pending();
        let _ = a.store.save();
        self.state_changed();
        true
    }
}

#[cfg(test)]
#[path = "../tests/service_federation.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/scheduler_federation.rs"]
mod scheduler_tests;

impl Service {
    pub(super) fn work_remote_send(
        &self,
        host: &str,
        id: u64,
        action: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<()> {
        let slot = self.hosts.values().find(|s| s.config.name == host).context("host unavailable")?;
        anyhow::ensure!(
            slot.welcome.as_ref().is_some_and(|w| w.version >= 14),
            "Update needed: work queue interactive launches require protocol 14"
        );
        slot.out
            .as_ref()
            .context("host offline")?
            .send(encode(&ClientMsg::Fleet { request: (1u64 << 62) | id, action: action.into(), args }))
            .context("peer link closed")
    }
}

#[cfg(test)]
#[path = "../tests/work_queue/federation.rs"]
mod work_queue_tests;

#[cfg(test)]
#[path = "../tests/attention/federation.rs"]
mod attention_tests;

#[cfg(test)]
#[path = "../tests/session_history.rs"]
mod session_history_tests;
