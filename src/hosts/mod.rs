//! Manage local and remote agent sessions through the same deck.
//!
//! Each host runs its own service and `cepheus peer` (`peer.rs`). The
//! hub's service keeps a TLS connection to every configured host's service
//! (`link.rs`) and merges their sessions into its own state under ids that
//! carry the host's tag (`fed.rs`, `protocol::host_id`), so the deck, the
//! phone gateway and `ctl` drive them with the messages they already send.

pub mod bootstrap;
pub mod fed;
pub mod install;
pub mod link;
pub mod logins;
pub mod mirror;
pub mod peer;
pub mod setup;
pub mod tls;
pub mod upkeep;
pub mod wire;

use std::fs;
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};

use crate::config::HostConfig;
use crate::protocol::{read_msg, write_msg, MAX_MESSAGE, PROTOCOL_VERSION};
use crate::remote::auth;
use crate::{prompt, util};
use wire::{Channel, Hello, Welcome, PEER_VERSION};

/// An address range a hub may connect from: an address and how many of its
/// leading bits must match.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cidr {
    pub addr: IpAddr,
    pub bits: u8,
}

/// `192.0.2.0/24`, `198.51.100.102` (all bits) or an IPv6 range; anything
/// else is left out.
pub fn parse_cidr(text: &str) -> Option<Cidr> {
    let text = text.trim();
    let (addr, bits) = match text.split_once('/') {
        Some((a, b)) => (a.parse::<IpAddr>().ok()?, Some(b.parse::<u8>().ok()?)),
        None => (text.parse::<IpAddr>().ok()?, None),
    };
    let max = if addr.is_ipv4() { 32 } else { 128 };
    let bits = bits.unwrap_or(max);
    (bits <= max).then_some(Cidr { addr, bits })
}

pub fn parse_cidrs(list: &[String]) -> Vec<Cidr> {
    list.iter().filter_map(|t| parse_cidr(t)).collect()
}

/// Whether `ip` is in one of `ranges`.
pub fn cidr_allows(ranges: &[Cidr], ip: IpAddr) -> bool {
    let ip = auth::plain_ip(ip);
    ranges.iter().any(|r| match (r.addr, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) => {
            let mask = if r.bits == 0 { 0 } else { u32::MAX << (32 - u32::from(r.bits)) };
            u32::from(net) & mask == u32::from(ip) & mask
        }
        (IpAddr::V6(net), IpAddr::V6(ip)) => {
            let mask = if r.bits == 0 { 0 } else { u128::MAX << (128 - u32::from(r.bits)) };
            u128::from(net) & mask == u128::from(ip) & mask
        }
        _ => false,
    })
}

/// `git -C dir args` output, trimmed, when it succeeds.
pub fn git_text(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output();
    out.ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The running program itself, whatever has since replaced its file.
pub const RUNNING_PROGRAM: &str = "/proc/self/exe";

/// This running program's SHA-256 and the time its file was made, read once.
pub fn this_build() -> &'static (String, u64) {
    static BUILD: OnceLock<(String, u64)> = OnceLock::new();
    BUILD.get_or_init(|| {
        let path = Path::new(RUNNING_PROGRAM);
        let hash = fs::read(path).map(|bytes| auth::sha256_hex(&bytes)).unwrap_or_default();
        (hash, modified_secs(path))
    })
}

pub fn modified_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Whether a host should take this hub's program. Protocol order wins over
/// install timestamps, which can change when a binary is copied or restored.
/// Timestamps order different builds only when their protocols match; a host
/// on a newer protocol is never downgraded.
pub fn host_needs_update(ours: &(String, u64), version: u32, build: &str, built: u64) -> bool {
    !ours.0.is_empty()
        && !build.is_empty()
        && build != ours.0
        && (version < PROTOCOL_VERSION || (version == PROTOCOL_VERSION && built < ours.1))
}

/// Where the hub keeps its key.
pub fn hub_key_path(state_dir: &Path) -> PathBuf {
    state_dir.join("hub").join("key")
}

/// The hub's key, hex, made on first use; only this machine's owner can
/// read it, and hosts keep only its SHA-256.
pub fn hub_key(state_dir: &Path) -> Result<String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(key) = existing_hub_key(state_dir) {
        return Ok(key);
    }
    let path = hub_key_path(state_dir);
    util::ensure_private_dir(path.parent().context("key path")?)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path.with_file_name("key.lock"))?;
    // SAFETY: flock uses a live descriptor owned by this call. Concurrent
    // viewer discovery must all authorize the same machine-local identity.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error()).context("locking local viewer identity");
    }
    if let Some(key) = existing_hub_key(state_dir) {
        return Ok(key);
    }
    let key = prompt::hex(&auth::random_bytes(32)?);
    util::write_atomic(&path, key.as_bytes())?;
    Ok(key)
}

/// The hub key when one exists, without making one.
pub fn existing_hub_key(state_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(hub_key_path(state_dir)).ok()?;
    let text = text.trim();
    (text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| text.to_string())
}

/// Public identity for this machine's existing local key, made on first use.
/// Enrollment moves this digest only; the key stays in its native state path.
pub fn hub_key_hash(state_dir: &Path) -> Result<String> {
    Ok(auth::sha256_hex(hub_key(state_dir)?.as_bytes()))
}

/// Connect to `host`, prove this hub and open `channel`; the returned
/// socket then carries that channel's messages in the clear.
pub fn open(host: &HostConfig, key: &str, channel: Channel) -> Result<(UnixStream, Welcome)> {
    let (stream, welcome, _) = open_with_local_addr(host, key, channel)?;
    Ok((stream, welcome))
}

/// The native installer also needs the actual source IP used to reach a peer,
/// so its return coordinator endpoint cannot accidentally name another machine.
pub fn open_with_local_addr(
    host: &HostConfig,
    key: &str,
    channel: Channel,
) -> Result<(UnixStream, Welcome, SocketAddr)> {
    let (conn, tcp) = tls::connect(&host.addresses, &host.fingerprint)?;
    let source = tcp.local_addr()?;
    let mut local = tls::bridged(conn, tcp)?;
    write_msg(&mut local, &Hello { v: PEER_VERSION, key: key.to_string(), channel })?;
    local.set_read_timeout(Some(tls::CONNECT_TIMEOUT))?;
    let welcome: Welcome = read_msg(&mut local)
        .map_err(|e| anyhow::anyhow!("no answer to the hello: {e}"))?
        .context("the host closed the connection")?;
    if !welcome.ok {
        bail!("{}", welcome.error.unwrap_or_else(|| "refused".to_string()));
    }
    local.set_read_timeout(None)?;
    Ok((local, welcome, source))
}

/// One length-prefixed message body, unparsed.
pub fn read_raw<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

#[cfg(test)]
#[path = "../tests/hosts.rs"]
mod tests;
