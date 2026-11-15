//! The gateway's files in `~/.local/state/cepheus/remote/`, each readable
//! only by the owner: its TLS certificate and key, the paired phones (by the
//! SHA-256 of their keys, never the keys) and the one-time pairing code the
//! deck's pairing screen writes (by its SHA-256 too).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::remote::auth;
use crate::util;

const CERT: &str = "cert.der";
const KEY: &str = "key.der";
const DEVICES: &str = "devices.json";
const DEVICES_LOCK: &str = "devices.lock";
const PAIRING: &str = "pairing.json";
pub const LOCK: &str = "gateway.lock";
/// How long a pairing code works.
pub const PAIRING_SECS: u64 = 300;
/// Letters and digits that cannot be mistaken for one another.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
pub const CODE_LEN: usize = 8;

pub fn remote_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("remote")
}

/// The gateway's certificate and private key, both DER, and the SHA-256 of
/// the certificate that the phone pins.
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint: String,
}

/// Load the certificate, or make a self-signed one for `host` (an address
/// or name) on first start.
pub fn load_or_create_identity(dir: &Path, host: &str) -> Result<Identity> {
    util::ensure_private_dir(dir)?;
    let (cert_path, key_path) = (dir.join(CERT), dir.join(KEY));
    if let (Ok(cert_der), Ok(key_der)) = (fs::read(&cert_path), fs::read(&key_path)) {
        if !cert_der.is_empty() && !key_der.is_empty() {
            let fingerprint = auth::sha256_hex(&cert_der);
            return Ok(Identity { cert_der, key_der, fingerprint });
        }
    }
    let mut names = vec!["cepheus".to_string()];
    if !host.is_empty() && host != "cepheus" {
        names.push(host.to_string());
    }
    let made = rcgen::generate_simple_self_signed(names).context("making the gateway certificate")?;
    let cert_der = made.cert.der().to_vec();
    let key_der = made.signing_key.serialize_der();
    util::write_atomic(&key_path, &key_der)?;
    util::write_atomic(&cert_path, &cert_der)?;
    let fingerprint = auth::sha256_hex(&cert_der);
    Ok(Identity { cert_der, key_der, fingerprint })
}

/// The pinned fingerprint, for the pairing screen; none before the gateway
/// first ran.
pub fn fingerprint(dir: &Path) -> Option<String> {
    fs::read(dir.join(CERT)).ok().filter(|d| !d.is_empty()).map(|d| auth::sha256_hex(&d))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// SHA-256 of the device key, hex.
    pub key_sha256: String,
    pub added: u64,
    #[serde(default)]
    pub last_seen: Option<u64>,
}

#[derive(Serialize, Deserialize, Default)]
struct DeviceFile {
    devices: Vec<Device>,
}

pub fn load_devices(dir: &Path) -> Vec<Device> {
    fs::read(dir.join(DEVICES))
        .ok()
        .and_then(|b| serde_json::from_slice::<DeviceFile>(&b).ok())
        .map(|f| f.devices)
        .unwrap_or_default()
}

pub fn save_devices(dir: &Path, devices: &[Device]) -> io::Result<()> {
    util::ensure_private_dir(dir)?;
    let body = serde_json::to_vec_pretty(&DeviceFile { devices: devices.to_vec() }).map_err(io::Error::other)?;
    util::write_atomic(&dir.join(DEVICES), &body)
}

/// Change the device list as it is on disk now, under a lock the gateway and
/// the deck's pairing screen, two processes, both take: `change` sees every
/// earlier change and says whether to write the list back. The gateway once
/// saved its own copy on a sign-in and so restored a phone the deck had just
/// removed (ROOT-035). Returns the list as it now is.
pub fn update_devices(dir: &Path, change: impl FnOnce(&mut Vec<Device>) -> bool) -> io::Result<Vec<Device>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    util::ensure_private_dir(dir)?;
    let lock =
        fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join(DEVICES_LOCK))?;
    // SAFETY: flock uses a live descriptor owned by this call; closing it
    // when this returns releases the lock.
    while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    let mut devices = load_devices(dir);
    if change(&mut devices) {
        save_devices(dir, &devices)?;
    }
    Ok(devices)
}

/// When the device list last changed, to notice a phone removed in the deck.
pub fn devices_stamp(dir: &Path) -> Option<u64> {
    util::modified_ms(&dir.join(DEVICES))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pairing {
    pub code_sha256: String,
    pub expires: u64,
}

/// A new pairing code, `CODE_LEN` characters.
pub fn new_code() -> Result<String> {
    let bytes = auth::random_bytes(CODE_LEN)?;
    Ok(bytes.iter().map(|b| CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char).collect())
}

/// A code as typed: case and separators do not matter.
pub fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// Shown as two groups of four.
pub fn display_code(code: &str) -> String {
    let c = normalize_code(code);
    if c.len() == CODE_LEN {
        format!("{}-{}", &c[..4], &c[4..])
    } else {
        c
    }
}

pub fn write_pairing(dir: &Path, code: &str, now: u64) -> io::Result<()> {
    util::ensure_private_dir(dir)?;
    let pairing =
        Pairing { code_sha256: auth::sha256_hex(normalize_code(code).as_bytes()), expires: now + PAIRING_SECS };
    util::write_atomic(&dir.join(PAIRING), &serde_json::to_vec(&pairing).map_err(io::Error::other)?)
}

pub fn read_pairing(dir: &Path) -> Option<Pairing> {
    fs::read(dir.join(PAIRING)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

pub fn clear_pairing(dir: &Path) {
    let _ = fs::remove_file(dir.join(PAIRING));
}
