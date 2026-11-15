//! Who may use the gateway: an allowed address, then a paired phone's key or
//! a one-time pairing code, with repeated failures locking the address out.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ring::rand::SecureRandom;

use crate::remote::store::{self, Device};

/// Failures that lock an address out, within `FAILURE_WINDOW`.
const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(600);
const LOCKOUT: Duration = Duration::from_secs(900);
/// Device keys are 256 random bits.
pub const KEY_BYTES: usize = 32;

pub fn sha256_hex(data: &[u8]) -> String {
    crate::prompt::hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; n];
    ring::rand::SystemRandom::new().fill(&mut out).map_err(|_| anyhow::anyhow!("no system randomness"))?;
    Ok(out)
}

/// Equal without an early exit, so timing tells nothing of where they differ.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// An IPv4 peer may arrive as an IPv4-mapped IPv6 address.
pub fn plain_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

pub fn allowed(allow: &[IpAddr], ip: IpAddr) -> bool {
    allow.contains(&plain_ip(ip))
}

#[derive(Default)]
pub struct Limiter {
    failures: HashMap<IpAddr, Vec<Instant>>,
    locked: HashMap<IpAddr, Instant>,
}

impl Limiter {
    /// Whether `ip` may try now.
    pub fn open(&mut self, ip: IpAddr, now: Instant) -> bool {
        match self.locked.get(&ip) {
            Some(until) if now < *until => false,
            Some(_) => {
                self.locked.remove(&ip);
                self.failures.remove(&ip);
                true
            }
            None => true,
        }
    }

    pub fn fail(&mut self, ip: IpAddr, now: Instant) {
        let list = self.failures.entry(ip).or_default();
        list.retain(|at| now.duration_since(*at) < FAILURE_WINDOW);
        list.push(now);
        if list.len() >= MAX_FAILURES {
            self.locked.insert(ip, now + LOCKOUT);
        }
    }

    pub fn succeed(&mut self, ip: IpAddr) {
        self.failures.remove(&ip);
    }
}

/// The paired device `id` whose key is `key_hex`.
pub fn find_device(devices: &[Device], id: &str, key_hex: &str) -> Option<usize> {
    let key = decode_hex(key_hex).filter(|k| k.len() == KEY_BYTES)?;
    let hash = sha256_hex(&key);
    let mut found = None;
    for (i, d) in devices.iter().enumerate() {
        // Compare every entry so the time taken does not depend on which matched.
        if ct_eq(d.id.as_bytes(), id.as_bytes()) & ct_eq(d.key_sha256.as_bytes(), hash.as_bytes()) {
            found = Some(i);
        }
    }
    found
}

pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || s.len() > 256 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Use up the pairing code the deck shows: it must match, be unexpired, and
/// works once (the file goes whether or not it matched, so a wrong guess
/// cannot be followed by another against the same code).
pub fn redeem_code(dir: &Path, code: &str, now_secs: u64) -> Result<()> {
    let Some(pairing) = store::read_pairing(dir) else { bail!("no pairing code is open; use :pair in the deck") };
    store::clear_pairing(dir);
    if now_secs >= pairing.expires {
        bail!("the pairing code has expired");
    }
    let given = sha256_hex(store::normalize_code(code).as_bytes());
    if !ct_eq(given.as_bytes(), pairing.code_sha256.as_bytes()) {
        bail!("wrong pairing code");
    }
    Ok(())
}

/// A new device's id and key (hex), and the record kept of it.
pub fn new_device(name: &str, now_secs: u64) -> Result<(Device, String)> {
    let key = random_bytes(KEY_BYTES)?;
    let id = format!("d-{}", crate::prompt::hex(&random_bytes(6)?));
    let name: String = name.chars().filter(|c| !c.is_control()).take(60).collect();
    let device = Device {
        id,
        name: if name.trim().is_empty() { "Phone".into() } else { name.trim().to_string() },
        key_sha256: sha256_hex(&key),
        added: now_secs,
        last_seen: Some(now_secs),
    };
    Ok((device, crate::prompt::hex(&key)))
}
