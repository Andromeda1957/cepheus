//! One completion for a fleet broadcast, after every host has acknowledged it.
use super::*;
use crate::protocol::{host_id, split_id};

const DELIVERY_WAIT: Duration = Duration::from_secs(45);
/// A destination's reason for an agent that stopped working between its
/// selection and delivery (`messaging.rs`, `start_message`).
const STOPPED_WORKING: &str = "no longer working; queued broadcast was not sent";

#[derive(Default)]
pub(super) struct State {
    serial: u64,
    pub(super) pending: HashMap<(u64, u64), Pending>,
}

pub(super) struct Pending {
    until: Instant,
    parts: BTreeMap<u16, Part>,
}

type DeliveryResult = (Vec<u64>, Vec<(u64, String)>);

struct Part {
    label: String,
    ids: Option<Vec<u64>>,
    result: Option<DeliveryResult>,
    problem: Option<String>,
}

impl Service {
    pub(super) fn start_fleet_broadcast(&mut self, cid: u64, selected: &[u64], text: &str) {
        let problem = if text.trim().is_empty() {
            Some("Type a message to broadcast.")
        } else if text.contains('\0') {
            Some("The message contains an unsupported control character. Remove it and try again.")
        } else if Self::broadcast_text(text).len() > crate::remote::wire::SEND_MAX {
            Some("The message is too long. Shorten it and try again.")
        } else {
            None
        };
        if let Some(problem) = problem {
            self.refuse_fleet_broadcast(cid, selected, &format!("Sent to no session. {problem}"));
            return;
        }
        // An empty selection means the command's all-host scope. Each service
        // selects its own current recipients rather than a client's snapshot.
        let all = selected.is_empty();
        let ids = selected.to_vec();
        if ids.len() > 1024 || self.broadcasts.pending.len() >= 128 {
            self.refuse_fleet_broadcast(
                cid,
                selected,
                "Sent to no session; too many broadcast recipients or pending broadcasts.",
            );
            return;
        }
        let mut parts = BTreeMap::new();
        let mut seen = HashSet::new();
        for id in ids.into_iter().filter(|id| seen.insert(*id)) {
            let (tag, local) = split_id(id);
            let label = self.hosts.get(&tag).map(|h| h.label()).unwrap_or_else(|| {
                if tag == 0 {
                    self.opts.config.local_host_label()
                } else {
                    format!("Host {tag}")
                }
            });
            parts
                .entry(tag)
                .or_insert_with(|| Part { label, ids: Some(vec![]), result: None, problem: None })
                .ids
                .as_mut()
                .unwrap()
                .push(local);
        }
        // Ask each host to select from its own current sessions, even before
        // its first post-reconnect snapshot reaches the coordinator.
        if all {
            parts
                .insert(0, Part { label: self.opts.config.local_host_label(), ids: None, result: None, problem: None });
        }
        // Build hosts run no agents (host roles, 2026-11-08).
        for (&tag, host) in self.hosts.iter().filter(|(_, h)| h.config.runs_agents()) {
            if all {
                parts.entry(tag).or_insert_with(|| Part {
                    label: host.label(),
                    ids: None,
                    result: None,
                    problem: None,
                });
            }
        }
        for (&tag, part) in &mut parts {
            if tag == 0 {
                continue;
            }
            part.problem = match self.hosts.get(&tag) {
                None => Some("host is no longer configured; nothing sent".into()),
                Some(host) if host.out.is_none() => {
                    Some("host is offline; nothing sent. Reconnecting automatically".into())
                }
                Some(host)
                    if host
                        .welcome
                        .as_ref()
                        .is_none_or(|w| w.version < crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL) =>
                {
                    Some("update this host to protocol 19 for native owner messages; nothing sent".into())
                }
                _ => None,
            };
        }
        if parts.is_empty() {
            self.refuse_fleet_broadcast(cid, selected, "Sent to no session; no working agents on any connected host.");
            return;
        }
        self.broadcasts.serial = self.broadcasts.serial.wrapping_add(1);
        let request = self.broadcasts.serial;
        let sends: Vec<_> =
            parts.iter().filter(|(_, p)| p.problem.is_none()).map(|(&tag, p)| (tag, p.ids.clone())).collect();
        self.broadcasts.pending.insert((cid, request), Pending { until: Instant::now() + DELIVERY_WAIT, parts });
        for (tag, ids) in sends {
            if tag == 0 {
                self.begin_broadcast_part(cid, request, ids.as_deref(), text, true);
            } else {
                self.upstream_send(cid, tag, &ClientMsg::BroadcastPart { request, ids, text: text.into() });
            }
        }
        self.finish_fleet_broadcast(cid, request);
    }

    fn refuse_fleet_broadcast(&self, cid: u64, selected: &[u64], text: &str) {
        // The phone matches single-host notices by that host's label, even
        // when validation fails at the coordinator before dispatch.
        let host = selected
            .first()
            .map(|id| split_id(*id).0)
            .filter(|tag| *tag != 0 && selected.iter().all(|id| split_id(*id).0 == *tag))
            .and_then(|tag| self.hosts.get(&tag));
        let text = host.map_or_else(|| text.to_string(), |host| format!("{}: {text}", host.label()));
        self.notice(cid, text, true);
    }

    pub(super) fn broadcast_part_done(
        &mut self,
        cid: u64,
        request: u64,
        tag: u16,
        sent: Vec<u64>,
        failed: Vec<(u64, String)>,
    ) {
        let Some(part) = self.broadcasts.pending.get_mut(&(cid, request)).and_then(|p| p.parts.get_mut(&tag)) else {
            return;
        };
        if part.result.is_some() || part.problem.is_some() {
            return;
        }
        // A source connection older than protocol 19 refuses a whole
        // all-agents part with one entry that names no session (`link.rs`).
        if part.ids.is_none() && sent.is_empty() && matches!(&failed[..], [(0, _)]) {
            part.problem = failed.into_iter().next().map(|(_, reason)| reason);
            self.finish_fleet_broadcast(cid, request);
            return;
        }
        let mut seen = HashSet::new();
        let valid = sent.len() + failed.len() <= 1024
            && sent.iter().chain(failed.iter().map(|(id, _)| id)).all(|id| {
                split_id(*id).0 == 0 && part.ids.as_ref().is_none_or(|ids| ids.contains(id)) && seen.insert(*id)
            })
            && part.ids.as_ref().is_none_or(|ids| seen.len() == ids.len());
        if valid {
            part.result = Some((sent, failed));
        } else {
            part.problem = Some("incomplete delivery receipt; check the agents before retrying".into());
        }
        self.finish_fleet_broadcast(cid, request);
    }

    pub(super) fn fail_broadcast_host(&mut self, cid: u64, tag: u16, reason: &str) -> bool {
        let requests: Vec<_> = self
            .broadcasts
            .pending
            .iter_mut()
            .filter_map(|(&(client, request), p)| {
                let part = (client == cid).then(|| p.parts.get_mut(&tag)).flatten()?;
                if part.result.is_some() || part.problem.is_some() {
                    return None;
                }
                part.problem = Some(reason.into());
                Some(request)
            })
            .collect();
        for &request in &requests {
            self.finish_fleet_broadcast(cid, request);
        }
        !requests.is_empty()
    }

    pub(super) fn broadcasts_due(&mut self, now: Instant) {
        let due: Vec<_> = self
            .broadcasts
            .pending
            .iter_mut()
            .filter_map(|(&key, p)| {
                if now < p.until {
                    return None;
                }
                for part in p.parts.values_mut().filter(|p| p.result.is_none() && p.problem.is_none()) {
                    part.problem = Some("delivery confirmation timed out; check the agents before retrying".into());
                }
                Some(key)
            })
            .collect();
        for (cid, request) in due {
            self.finish_fleet_broadcast(cid, request);
        }
    }

    fn finish_fleet_broadcast(&mut self, cid: u64, request: u64) {
        let Some(pending) = self.broadcasts.pending.get(&(cid, request)) else { return };
        if pending.parts.values().any(|p| p.result.is_none() && p.problem.is_none()) {
            return;
        }
        let pending = self.broadcasts.pending.remove(&(cid, request)).unwrap();
        let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let results = || pending.parts.values().filter_map(|p| p.result.as_ref());
        let stopped_on =
            |failed: &[(u64, String)]| failed.iter().filter(|(_, reason)| reason == STOPPED_WORKING).count();
        let delivered: usize = results().map(|(sent, _)| sent.len()).sum();
        let stopped: usize = results().map(|(_, failed)| stopped_on(failed)).sum();
        let undelivered: usize = results().map(|(_, failed)| failed.len()).sum::<usize>() - stopped;
        let hosts = results().filter(|(sent, _)| !sent.is_empty()).count();
        let unconfirmed = pending.parts.values().filter(|p| p.problem.is_some()).count();
        // The first line is the summary the footer and phone show; it names
        // any shortfall, and the lines after it give each host's outcome.
        let mut text = if delivered == 0 {
            "Sent to no session".to_string()
        } else {
            format!("Sent to {} across {}", plural(delivered, "agent", "agents"), plural(hosts, "host", "hosts"))
        };
        let mut shortfall = Vec::new();
        if undelivered > 0 {
            shortfall.push(format!("{} not delivered", plural(undelivered, "agent", "agents")));
        }
        if unconfirmed > 0 {
            shortfall.push(format!("{} did not confirm delivery", plural(unconfirmed, "host", "hosts")));
        }
        let error = delivered == 0 || !shortfall.is_empty();
        if !shortfall.is_empty() {
            text.push_str(&format!("; {}", shortfall.join("; ")));
        }
        // Leaving an agent that finished first alone is the requested
        // outcome, not a failed delivery.
        if stopped > 0 {
            let verb = if stopped == 1 { "was" } else { "were" };
            text.push_str(&format!(
                "; {} stopped working before delivery and {verb} left alone",
                plural(stopped, "agent", "agents")
            ));
        } else if delivered == 0 && shortfall.is_empty() {
            text.push_str("; no agent is working on any connected host");
        }
        // Preserve the established phone gateway's single-host reply matching.
        if pending.parts.len() == 1 {
            if let Some((&tag, part)) = pending.parts.first_key_value() {
                if tag != 0 {
                    text = format!("{}: {text}", part.label);
                }
            }
        }
        // Without a shortfall, the summary is the whole story for one host or
        // when nothing was delivered.
        if shortfall.is_empty() && (pending.parts.len() == 1 || delivered == 0) {
            self.notice(cid, text, error);
            return;
        }
        let sessions = if undelivered > 0 { self.build_state().sessions } else { Vec::new() };
        // Each host with something to report, then its failed agents grouped
        // by reason. A host with no working agent adds nothing.
        for (tag, part) in pending.parts {
            if let Some(problem) = part.problem {
                text.push_str(&format!("\n{}: {problem}.", part.label));
                continue;
            }
            let Some((sent, failed)) = part.result else { continue };
            if sent.is_empty() && failed.is_empty() {
                continue;
            }
            let stopped = stopped_on(&failed);
            let mut outcome = vec![format!("{} delivered", sent.len())];
            if failed.len() > stopped {
                outcome.push(format!("{} not delivered", failed.len() - stopped));
            }
            if stopped > 0 {
                outcome.push(format!("{stopped} stopped working first"));
            }
            text.push_str(&format!("\n{}: {}", part.label, outcome.join(", ")));
            let mut reasons: Vec<(String, Vec<String>)> = Vec::new();
            for (id, reason) in failed.into_iter().filter(|(_, reason)| reason != STOPPED_WORKING) {
                let id = host_id(tag, id);
                let who = sessions
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| session_label(s.tool, &s.title))
                    .unwrap_or_else(|| format!("session {id}"));
                match reasons.iter_mut().find(|(seen, _)| *seen == reason) {
                    Some((_, names)) => names.push(who),
                    None => reasons.push((reason, vec![who])),
                }
            }
            for (reason, names) in reasons {
                text.push_str(&format!("\n  {}: {reason}", names.join(", ")));
            }
        }
        self.notice(cid, text, error);
    }
}
