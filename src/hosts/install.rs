//! `cepheus hosts`: the machines this deck drives as a hub.
//!
//! `install` reaches a host over SSH once, the only time SSH is used: it
//! copies this build there, runs `cepheus peer setup` with the hub key
//! on standard input, and records the host and its certificate in this
//! deck's config. From then on the hub talks to the host's own service over
//! the peer listener, and a running hub picks the host up by itself.

use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

use crate::config::{valid_host_name, Config, HostConfig, PEER_PORT};
use crate::hosts::setup::SetupReport;
use crate::hosts::wire::{Channel, HostReq, HostResp};
use crate::protocol::{read_msg, write_msg, HostStats, MAX_HOST_TAG};
use crate::{hosts, install, paths, util};

const USAGE: &str = "\
usage:
  cepheus hosts                       list the hosts this deck drives and whether they answer
  cepheus hosts install <name> [--ssh <alias>] [--address <ip:port>]... [--label <label>] [--allow <cidr>]...
  cepheus hosts remove <name>         stop driving a host (its own service keeps running)";

pub fn run(args: &[String]) -> i32 {
    let result = match args.first().map(String::as_str) {
        None | Some("list") => list(),
        Some("install") => parse_install(&args[1..]).and_then(|a| install(&a)),
        Some("remove") => remove(&args[1..]),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(anyhow::anyhow!("unknown hosts command {other:?}\n{USAGE}")),
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("cepheus hosts: {err:#}");
            1
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct InstallArgs {
    pub name: String,
    pub ssh: Option<String>,
    pub addresses: Vec<String>,
    pub label: Option<String>,
    pub allow: Vec<String>,
}

pub fn parse_install(args: &[String]) -> Result<InstallArgs> {
    let mut out = InstallArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--ssh" => out.ssh = Some(value()?),
            "--address" => out.addresses.push(value()?),
            "--label" => out.label = Some(value()?),
            "--allow" => out.allow.push(value()?),
            flag if flag.starts_with('-') => bail!("unknown option {flag:?}\n{USAGE}"),
            name if out.name.is_empty() => out.name = name.to_string(),
            extra => bail!("unexpected {extra:?}\n{USAGE}"),
        }
    }
    if !valid_host_name(&out.name) {
        bail!("a host needs a short name of lowercase letters, digits and -, such as fedora\n{USAGE}");
    }
    if let Some(bad) = out.allow.iter().find(|a| hosts::parse_cidr(a).is_none()) {
        bail!("--allow {bad:?} is not an address or CIDR range");
    }
    Ok(out)
}

/// `name` with its first letter capitalized: the heading a new host gets.
pub fn default_label(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// The host name `ssh -G` resolves an alias to.
pub fn ssh_hostname(ssh_config: &str) -> Option<String> {
    ssh_config.lines().find_map(|line| line.strip_prefix("hostname ").map(|h| h.trim().to_string()))
}

/// The network of `ip` with `bits` leading bits, as `addr/bits`.
pub fn network(ip: IpAddr, bits: u8) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let bits = bits.min(32);
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - u32::from(bits)) };
            format!("{}/{bits}", Ipv4Addr::from(u32::from(v4) & mask))
        }
        IpAddr::V6(v6) => {
            let bits = bits.min(128);
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - u32::from(bits)) };
            format!("{}/{bits}", Ipv6Addr::from(u128::from(v6) & mask))
        }
    }
}

/// `config` with `host` recorded: an existing entry of that name keeps its
/// tag, so its sessions keep their ids; a new one takes the lowest free tag.
pub fn with_host(config: Value, mut host: HostConfig) -> Result<(Value, HostConfig)> {
    let mut map = match config {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        _ => bail!("the config file is not a JSON object"),
    };
    let mut list: Vec<Value> = match map.remove("hosts") {
        Some(Value::Array(list)) => list,
        None | Some(Value::Null) => Vec::new(),
        Some(_) => bail!("\"hosts\" in the config file is not a list"),
    };
    let tag_of = |v: &Value| v.get("tag").and_then(Value::as_u64);
    let existing = list.iter().position(|v| v.get("name").and_then(Value::as_str) == Some(host.name.as_str()));
    host.tag = match existing.and_then(|i| tag_of(&list[i])) {
        Some(tag) if (1..=u64::from(MAX_HOST_TAG)).contains(&tag) => tag as u16,
        _ => {
            let used: Vec<u64> = list.iter().filter_map(tag_of).collect();
            (1..=MAX_HOST_TAG).find(|t| !used.contains(&u64::from(*t))).context("every host number is taken")?
        }
    };
    // Reinstalling keeps what the host is for.
    if let Some(role) = existing.and_then(|i| list[i].get("role")) {
        host.role =
            serde_json::from_value(role.clone()).context("the host's \"role\" is not \"agents\" or \"build\"")?;
    }
    let entry = serde_json::to_value(&host)?;
    match existing {
        Some(i) => list[i] = entry,
        None => list.push(entry),
    }
    map.insert("hosts".into(), Value::Array(list));
    Ok((Value::Object(map), host))
}

/// `config` without the host named `name`, and whether it was there.
pub fn without_host(config: Value, name: &str) -> (Value, bool) {
    let Value::Object(mut map) = config else { return (config, false) };
    let mut removed = false;
    if let Some(Value::Array(list)) = map.get_mut("hosts") {
        let before = list.len();
        list.retain(|v| v.get("name").and_then(Value::as_str) != Some(name));
        removed = list.len() != before;
    }
    (Value::Object(map), removed)
}

/// The recorded hosts as `{"hosts": [...]}`: from the hosts file beside the
/// config file, or the config file's own `hosts` an older build wrote.
fn read_config() -> Result<Value> {
    let path = paths::config_path();
    let hosts = crate::config::read_hosts(&path);
    Ok(serde_json::json!({ "hosts": hosts }))
}

/// Record the hosts in the hosts file (`config::write_hosts`); the owner's
/// config file is only ever touched to move an older `hosts` out of it.
fn write_config(config: &Value) -> Result<()> {
    let path = paths::config_path();
    let hosts: Vec<HostConfig> =
        serde_json::from_value(config.get("hosts").cloned().unwrap_or(Value::Array(Vec::new())))?;
    crate::config::write_hosts(&path, &hosts)?;
    if let Some(err) = Config::load().1 {
        bail!("{} no longer loads: {err}", path.display());
    }
    Ok(())
}

fn install(args: &InstallArgs) -> Result<()> {
    let key = hosts::hub_key(&paths::state_dir())?;
    let ssh_alias = args.ssh.clone().unwrap_or_else(|| args.name.clone());
    let addresses = if args.addresses.is_empty() {
        let out = ssh_output(&["-G", &ssh_alias])?;
        let host = ssh_hostname(&out).context("ssh -G names no host name; pass --address")?;
        vec![format!("{host}:{PEER_PORT}")]
    } else {
        args.addresses.clone()
    };
    let port = addresses[0].rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).unwrap_or(PEER_PORT);
    if Config::load().0.reserved_ports.contains(&port) {
        bail!("port {port} is reserved by the local profile; choose another");
    }
    let allow = if args.allow.is_empty() { hub_networks(&addresses) } else { args.allow.clone() };
    if allow.is_empty() {
        bail!("cannot tell which of this machine's networks reach {}; pass --allow", addresses.join(", "));
    }
    let config = read_config()?;
    let known = config
        .get("hosts")
        .and_then(Value::as_array)
        .and_then(|l| l.iter().find(|v| v.get("name").and_then(Value::as_str) == Some(args.name.as_str())))
        .and_then(|v| v.get("label").and_then(Value::as_str))
        .map(str::to_string);
    let label = args.label.clone().or(known).unwrap_or_else(|| default_label(&args.name));

    println!("Copying this build to {label} ({ssh_alias})…");
    let program = fs::read(hosts::RUNNING_PROGRAM).context("reading this build")?;
    let built = hosts::this_build().1;
    let (name, old) = (install::PROGRAM, install::LEGACY_PROGRAM);
    // A host set up before the rename to Cepheus runs the old path, from its
    // unit and its sessions' hooks: it becomes a link to the new program,
    // swapped in atomically as `install::link_legacy` does here.
    let link = if name == old {
        String::new()
    } else {
        format!(
            "; o=\"$d/{old}\"; if [ -f \"$o\" ] && [ ! -L \"$o\" ]; then \
             l=\"$d/.{old}.link.$$\"; ln -sfn {name} \"$l\"; mv -f \"$l\" \"$o\"; fi"
        )
    };
    let script = format!(
        "set -e; d=\"$HOME/.local/bin\"; mkdir -p \"$d\"; t=\"$d/.{name}.install.$$\"; \
         trap 'rm -f \"$t\"' EXIT; cat > \"$t\"; chmod 755 \"$t\"; touch -d @{built} \"$t\"; \
         \"$t\" --version >/dev/null; mv -f \"$t\" \"$d/{name}\"{link}"
    );
    ssh_run(&ssh_alias, &format!("sh -c {}", util::shell_quote(&script)), &program)?;

    println!("Turning on {label}'s peer listener on port {port}, for {}…", allow.join(", "));
    let mut setup = format!(
        "\"$HOME/.local/bin/cepheus\" peer setup --label {} --listen {}",
        util::shell_quote(&label),
        util::shell_quote(&format!("0.0.0.0:{port}"))
    );
    for cidr in &allow {
        setup.push_str(&format!(" --allow {}", util::shell_quote(cidr)));
    }
    let out = ssh_run(&ssh_alias, &format!("bash -lc {}", util::shell_quote(&setup)), key.as_bytes())?;
    let report: SetupReport = out
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
        .with_context(|| format!("{label} did not report its setup: {}", util::one_line(&out)))?;

    let host = HostConfig {
        role: Default::default(),
        name: args.name.clone(),
        label: Some(label.clone()),
        tag: 0,
        addresses,
        ssh: Some(ssh_alias),
        fingerprint: report.fingerprint.clone(),
    };
    let (updated, host) = with_host(config, host)?;
    write_config(&updated)?;
    println!(
        "Recorded {label} in {} as host {}.",
        crate::config::hosts_path(&paths::config_path()).display(),
        host.tag
    );

    println!("Waiting for {label} to answer…");
    let deadline = Instant::now() + Duration::from_secs(45);
    let stats = loop {
        match probe(&host, &key) {
            Ok(stats) => break stats,
            Err(err) if Instant::now() > deadline => return Err(err.context(format!("{label} does not answer yet"))),
            Err(_) => thread::sleep(Duration::from_secs(2)),
        }
    };
    println!("{label} is part of this deck: {}; sessions start in {}.", summary(&stats), report.start_dir);
    println!("A running deck lists it within ten seconds.");
    Ok(())
}

fn list() -> Result<()> {
    let (config, err) = Config::load();
    if let Some(err) = err {
        bail!("config: {err}");
    }
    if config.hosts.is_empty() {
        println!("This deck drives no other hosts. Add one with `cepheus hosts install <name> --ssh <alias>`.");
        return Ok(());
    }
    let key = hosts::existing_hub_key(&paths::state_dir());
    for host in &config.hosts {
        let status = match &key {
            None => "no hub key on this machine".to_string(),
            Some(key) => match probe(host, key) {
                Ok(stats) => format!("online · {}", summary(&stats)),
                Err(err) => format!("offline · {}", util::one_line(&format!("{err:#}"))),
            },
        };
        println!("{:<12} {:<12} {:<24} {status}", host.name, host.label(), host.addresses.join(", "));
    }
    Ok(())
}

fn remove(args: &[String]) -> Result<()> {
    let [name] = args else { bail!("{USAGE}") };
    let (updated, removed) = without_host(read_config()?, name);
    if !removed {
        bail!("this deck drives no host named {name:?}");
    }
    write_config(&updated)?;
    println!("This deck no longer drives {name}. Its own service keeps running there;");
    println!("on {name}, `systemctl --user disable --now cepheus` stops it.");
    Ok(())
}

/// One reading of the host's stats over its host channel.
fn probe(host: &HostConfig, key: &str) -> Result<HostStats> {
    let (mut stream, _) = hosts::open(host, key, Channel::Host)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    write_msg(&mut stream, &HostReq::Stats { resources_only: false })?;
    match read_msg::<_, HostResp>(&mut stream)? {
        Some(HostResp::Stats { stats }) => Ok(stats),
        Some(HostResp::Error { text }) => bail!("{text}"),
        _ => bail!("no stats in the answer"),
    }
}

fn summary(stats: &HostStats) -> String {
    let mut parts = Vec::new();
    if let Some(cpu) = stats.cpu {
        parts.push(format!("CPU {cpu:.0}%"));
    }
    if let Some(memory) = stats.memory {
        parts.push(format!("RAM {memory:.0}%"));
    }
    if let (Some(branch), Some(head)) = (&stats.branch, &stats.head) {
        parts.push(format!("{branch} at {}", &head[..head.len().min(9)]));
    }
    if parts.is_empty() {
        "answering".to_string()
    } else {
        parts.join(", ")
    }
}

/// The ranges a host admits for the addresses this machine reaches it from
/// ([`hub_range`]).
fn hub_networks(addresses: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for address in addresses {
        let Ok(found) = address.to_socket_addrs() else { continue };
        for target in found {
            let range = source_ip(target).and_then(|ip| {
                let (prefix, point_to_point) = interface_of(ip)?;
                Some(hub_range(ip, prefix, point_to_point))
            });
            if let Some(cidr) = range {
                if !out.contains(&cidr) {
                    out.push(cidr);
                }
            }
        }
    }
    out
}

/// What a host admits for hub address `ip` on an interface of `prefix` bits.
/// A point-to-point link, such as the VPN's `tun0`, admits only `ip`: the
/// VPN runs client-to-client, so its other peers reach the host too, which is
/// why the phone gateway admits only the phone. A LAN admits its range, so a
/// changing DHCP address stays admitted.
fn hub_range(ip: IpAddr, prefix: u8, point_to_point: bool) -> String {
    let whole = if ip.is_ipv4() { 32 } else { 128 };
    network(ip, if point_to_point { whole } else { prefix })
}

/// The address this machine sends from to reach `target`.
fn source_ip(target: SocketAddr) -> Option<IpAddr> {
    let bind = if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(target).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// The prefix length of the interface holding `ip`, and whether that
/// interface is a point-to-point link.
fn interface_of(ip: IpAddr) -> Option<(u8, bool)> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list this function frees.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: `cur` is a node of the list getifaddrs returned.
        let ifa = unsafe { &*cur };
        // SAFETY: both pointers come from getifaddrs and are checked for null.
        let pair = unsafe { (sockaddr_ip(ifa.ifa_addr), sockaddr_ip(ifa.ifa_netmask)) };
        if let (Some(addr), Some(mask)) = pair {
            if addr == ip {
                let prefix = match mask {
                    IpAddr::V4(m) => u32::from(m).count_ones() as u8,
                    IpAddr::V6(m) => u128::from(m).count_ones() as u8,
                };
                found = Some((prefix, (ifa.ifa_flags & libc::IFF_POINTOPOINT as libc::c_uint) != 0));
                break;
            }
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: `head` came from getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(head) };
    found
}

/// # Safety
/// `sa` is null or points to a socket address of its family's size.
unsafe fn sockaddr_ip(sa: *const libc::sockaddr) -> Option<IpAddr> {
    if sa.is_null() {
        return None;
    }
    match i32::from((*sa).sa_family) {
        libc::AF_INET => {
            let sin = &*(sa as *const libc::sockaddr_in);
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))))
        }
        libc::AF_INET6 => {
            let sin6 = &*(sa as *const libc::sockaddr_in6);
            Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

const SSH_OPTIONS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"];

fn ssh_output(args: &[&str]) -> Result<String> {
    let out = Command::new("ssh").args(args).output().context("running ssh")?;
    if !out.status.success() {
        bail!("ssh {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `command` on `alias`, feeding it `input`, and return what it printed.
fn ssh_run(alias: &str, command: &str, input: &[u8]) -> Result<String> {
    let mut child = Command::new("ssh")
        .args(SSH_OPTIONS)
        .arg("--")
        .arg(alias)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    let mut stdin = child.stdin.take().context("ssh input")?;
    let input = input.to_vec();
    let feeder = thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().context("waiting for ssh")?;
    let _ = feeder.join();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
        bail!("on {alias}: {}", last[last.len().saturating_sub(3)..].join(" / "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
#[path = "../tests/hosts_install.rs"]
mod tests;
