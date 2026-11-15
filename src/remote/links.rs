//! The gateway's own `host` channels to the hosts the hub drives (`hosts/`), for what the hub's service does
//! not carry: a remote session's chat, its skills and commands, and the files the phone attaches to it.
//! Everything else about a remote session (its state, screen, typing, answers, stops) goes through the service
//! as for a local one.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::config::{Config, HostConfig};
use crate::hosts::{
    self,
    wire::{Channel, HostReq, HostResp},
};
use crate::protocol::{read_msg, write_msg};
use std::path::Path;

/// After a host could not be reached, how long before trying again: an
/// open chat on a host that is down must not hold up the others each poll.
const RETRY: Duration = Duration::from_secs(10);
/// How long a host may take to answer.
const ANSWER: Duration = Duration::from_secs(30);

#[derive(Hash, Eq, PartialEq)]
enum Target {
    Native(String),
    Shared { coordinator: String, source: Option<String> },
}

#[derive(Default)]
struct Slot {
    stream: Option<UnixStream>,
    failed: Option<Instant>,
}

pub struct Links {
    hosts: Vec<HostConfig>,
    /// The hub's key (`hosts::hub_key`); none on a deck that drives no host.
    key: Option<String>,
    /// A selected shared coordinator, or a configuration error that fails closed.
    coordinator: std::result::Result<Option<HostConfig>, String>,
    follow_profile: bool,
    slots: Mutex<HashMap<Target, Arc<Mutex<Slot>>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Links {
    pub fn new(hosts: Vec<HostConfig>, key: Option<String>) -> Links {
        Links { hosts, key, coordinator: Ok(None), follow_profile: false, slots: Mutex::new(HashMap::new()) }
    }

    pub fn gateway(hosts: Vec<HostConfig>, key: Option<String>, socket: &Path, config_file: Option<&Path>) -> Links {
        let mut links = Self::new(hosts, key);
        // Explicit socket overrides belong to isolated/native gateways.
        if socket == crate::paths::socket_path() {
            links.follow_profile = true;
            let path = config_file.map(Path::to_path_buf).unwrap_or_else(crate::paths::config_path);
            let (config, error) = Config::load_from(&path);
            links.coordinator = if let Some(error) = error {
                Err(error)
            } else {
                crate::client::coordinator::selected_from(&path, &config).map_err(|error| format!("{error:#}"))
            };
        }
        links
    }

    pub fn is_shared(&self) -> bool {
        !matches!(self.coordinator, Ok(None))
    }

    pub fn profile_changed(&self, path: &Path, config: &Config) -> bool {
        self.follow_profile
            && crate::client::coordinator::selected_from(path, config).map_err(|error| format!("{error:#}"))
                != self.coordinator
    }

    pub fn coordinator_label(&self) -> Option<String> {
        self.coordinator.as_ref().ok()?.as_ref().map(HostConfig::label)
    }

    #[cfg(test)]
    pub fn via(mut self, coordinator: HostConfig) -> Self {
        self.coordinator = Ok(Some(coordinator));
        self
    }

    /// `None` names the shared coordinator's native source, never the viewer.
    pub fn ask_source(&self, source: Option<&str>, request: &HostReq, payload: Option<&[u8]>) -> Result<HostResp> {
        let coordinator = self.coordinator.as_ref().map_err(|error| anyhow::anyhow!("shared coordinator: {error}"))?;
        if let Some(host) = coordinator {
            let request =
                HostReq::PhoneForward { host: source.map(str::to_string), request: Box::new(request.clone()) };
            return self.ask_host(host, &request, payload);
        }
        let name = source.context("native content is served by the local gateway")?;
        let host =
            self.hosts.iter().find(|host| host.name == name).with_context(|| format!("no host {name:?} here"))?;
        self.ask_host(host, request, payload)
    }

    /// Ask host `name`, with `payload` sent after the request. One request
    /// at a time per host; a broken channel is opened again once.
    pub fn ask(&self, name: &str, req: &HostReq, payload: Option<&[u8]>) -> Result<HostResp> {
        self.ask_source(Some(name), req, payload)
    }

    fn ask_host(&self, host: &HostConfig, req: &HostReq, payload: Option<&[u8]>) -> Result<HostResp> {
        let key = self.key.as_deref().context("this deck has no hub key")?;
        // A stalled source must not occupy another source's coordinator
        // channel. Each source still reuses its own authenticated connection.
        let target = match req {
            HostReq::PhoneForward { host: source, .. } => {
                Target::Shared { coordinator: host.name.clone(), source: source.clone() }
            }
            _ => Target::Native(host.name.clone()),
        };
        let slot = Arc::clone(lock(&self.slots).entry(target).or_default());
        let mut slot = lock(&slot);
        let mut tries = 0;
        loop {
            if slot.stream.is_none() {
                if slot.failed.is_some_and(|at| at.elapsed() < RETRY) {
                    bail!("{} is out of reach", host.label());
                }
                match hosts::open(host, key, Channel::Host) {
                    Ok((stream, welcome)) => {
                        if matches!(req, HostReq::PhoneForward { .. })
                            && !welcome.capabilities.iter().any(|cap| cap == "phone-forward-v1")
                        {
                            bail!("shared coordinator does not support phone content forwarding; update its deck");
                        }
                        stream.set_read_timeout(Some(ANSWER))?;
                        slot.stream = Some(stream);
                        slot.failed = None;
                    }
                    Err(err) => {
                        slot.failed = Some(Instant::now());
                        return Err(err.context(format!("could not reach {}", host.label())));
                    }
                }
            }
            let stream = slot.stream.as_mut().context("no channel")?;
            match exchange(stream, req, payload) {
                Ok(HostResp::Error { text }) => bail!("{}: {text}", host.label()),
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    slot.stream = None;
                    tries += 1;
                    if tries == 2 {
                        return Err(err.context(format!("{} stopped answering", host.label())));
                    }
                }
            }
        }
    }
}

fn exchange(stream: &mut UnixStream, req: &HostReq, payload: Option<&[u8]>) -> Result<HostResp> {
    write_msg(stream, req)?;
    if let Some(bytes) = payload {
        stream.write_all(bytes)?;
    }
    read_msg::<_, HostResp>(stream)?.context("the host closed the channel")
}

/// The variant an answer was, never its contents.
pub fn unexpected(resp: &HostResp) -> anyhow::Error {
    let kind =
        serde_json::to_value(resp).ok().and_then(|v| v.get("t")?.as_str().map(str::to_string)).unwrap_or_default();
    anyhow::anyhow!("unexpected answer {kind:?} from the host")
}
