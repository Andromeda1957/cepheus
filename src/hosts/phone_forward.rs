//! Authenticated phone-content forwarding. Session control stays on Deck channels.
use crate::config::Config;
use crate::hosts::{
    self,
    wire::{HostReq, HostResp},
};
use crate::remote::{links::Links, uploads};
use anyhow::{bail, Result};
use std::path::Path;

pub const CAPABILITY: &str = "phone-forward-v1";

/// Reject before reading payloads or reaching another host. No generic RPC,
/// synchronization, login, update, process action or nested forwarding passes.
pub fn payload_len(request: &HostReq) -> Result<u64> {
    match request {
        HostReq::Chat { .. }
        | HostReq::Commands { .. }
        | HostReq::Search { .. }
        | HostReq::Image { .. }
        | HostReq::ScanReport { .. } => Ok(0),
        HostReq::PutFile { len, .. } if *len <= uploads::FILE_MAX => Ok(*len),
        _ => bail!("operation is not an allowed phone content request"),
    }
}

/// Fixed coordinator-native defaults/history/stats never gain another hop.
pub fn native_payload_len(request: &HostReq) -> Result<u64> {
    match request {
        HostReq::PhoneModels | HostReq::PhoneStats | HostReq::PhoneAskHistory => Ok(0),
        request => payload_len(request),
    }
}

/// One viewer channel retains native peer links; chat polling never creates a
/// fresh TLS connection for every request. Configuration/key changes replace it.
#[derive(Default)]
pub struct Relay {
    hosts: Vec<crate::config::HostConfig>,
    key: Option<String>,
    links: Option<Links>,
}

impl Relay {
    pub fn relay(
        &mut self,
        config: &Config,
        state: &Path,
        host: &str,
        request: &HostReq,
        payload: Option<&[u8]>,
    ) -> Result<HostResp> {
        let len = payload_len(request)?;
        if payload.map_or(0, |bytes| bytes.len() as u64) != len {
            bail!("phone content payload length differs");
        }
        // The viewer supplies no address, certificate, key or additional hop.
        let key = hosts::existing_hub_key(state);
        if self.links.is_none() || self.hosts != config.hosts || self.key != key {
            self.hosts = config.hosts.clone();
            self.key = key.clone();
            self.links = Some(Links::new(self.hosts.clone(), key));
        }
        self.links.as_ref().unwrap().ask(host, request, payload)
    }
}
