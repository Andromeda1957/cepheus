//! Which build host to use: the hosts this deck drives, each asked over SSH for its cores, load, free memory
//! and free offload slots (`slots_sh`), and ranked. This machine is never one of them; a host is used only when
//! a request names it or `auto` finds it answering with half its cores free of its own work.

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::{remote_command, slots_sh, ssh_args};

/// One machine work can be sent to.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildHost {
    /// The deck's name for it, or the destination when one was named directly.
    pub name: String,
    /// The SSH destination: its recorded alias, else its name.
    pub ssh: String,
}

/// What a host reported when asked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Load {
    pub cpus: u32,
    /// The one-minute load average.
    pub load1: f64,
    pub mem_available_kib: u64,
    /// The host's offload slots: half its cores, at least one.
    pub cap: usize,
    /// Slots no offloaded process holds.
    pub slots_free: usize,
}

/// Memory each concurrent shard is given, so a host with little free memory
/// runs fewer of them.
const SLOT_MEMORY_KIB: u64 = 1024 * 1024;

impl Load {
    /// Cores not already busy: CPUs less the one-minute load, never below 0.
    pub fn free_cores(&self) -> f64 {
        (self.cpus as f64 - self.load1).max(0.0)
    }

    /// The load that is not offloaded work: the load less one per held slot.
    pub fn own_load(&self) -> f64 {
        (self.load1 - self.cap.saturating_sub(self.slots_free) as f64).max(0.0)
    }

    /// Whether `auto` may send work here: its own work leaves at least half
    /// its cores free. Offloaded work already there does not count; new work
    /// waits for its slots.
    pub fn eligible(&self) -> bool {
        self.own_load() < self.cap as f64
    }

    /// How many shard processes the host takes now: its free slots, no more
    /// than the cores left in its half (`cap` less the whole load), nor than
    /// its free memory holds at a gibibyte each.
    pub fn slots(&self) -> usize {
        let cores = (self.cap as f64 - self.load1).max(0.0).floor() as usize;
        let memory = (self.mem_available_kib / SLOT_MEMORY_KIB) as usize;
        self.slots_free.min(cores).min(memory)
    }
}

/// Configured build hosts from `hosts.json`.
pub fn configured() -> Vec<BuildHost> {
    crate::config::read_hosts(&crate::paths::config_path())
        .into_iter()
        .map(|h| BuildHost {
            ssh: h.ssh.clone().filter(|s| !s.trim().is_empty()).unwrap_or(h.name.clone()),
            name: h.name,
        })
        .collect()
}

/// The hosts `spec` asks for: `auto` is every configured host; otherwise a
/// comma-separated list where each entry is a configured host's name or SSH
/// alias (any case), or else an SSH destination used as given.
pub fn choose(spec: &str, configured: &[BuildHost]) -> Result<Vec<BuildHost>> {
    if spec == "auto" {
        if configured.is_empty() {
            bail!("this deck drives no hosts (hosts.json); name one with --host <ssh destination>");
        }
        return Ok(configured.to_vec());
    }
    let mut chosen: Vec<BuildHost> = Vec::new();
    for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let host = configured
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(entry) || h.ssh.eq_ignore_ascii_case(entry))
            .cloned()
            .unwrap_or_else(|| BuildHost { name: entry.to_string(), ssh: entry.to_string() });
        if !chosen.contains(&host) {
            chosen.push(host);
        }
    }
    if chosen.is_empty() {
        bail!("--host names no host");
    }
    Ok(chosen)
}

/// Read the probe's four lines: `nproc`, `/proc/loadavg`, the
/// `MemAvailable` line of `/proc/meminfo` and `slots CAP FREE`.
pub fn parse_probe(out: &str) -> Option<Load> {
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    let cpus = lines.next()?.parse::<u32>().ok().filter(|c| *c > 0)?;
    let load1 = lines.next()?.split_whitespace().next()?.parse::<f64>().ok().filter(|l| l.is_finite() && *l >= 0.0)?;
    let mem = lines.next()?.strip_prefix("MemAvailable:")?.split_whitespace().next()?.parse::<u64>().ok()?;
    let mut slots = lines.next()?.strip_prefix("slots ")?.split_whitespace();
    let cap = slots.next()?.parse::<usize>().ok().filter(|c| *c > 0)?;
    let slots_free = slots.next()?.parse::<usize>().ok().filter(|f| *f <= cap)?;
    Some(Load { cpus, load1, mem_available_kib: mem, cap, slots_free })
}

/// Counts free slots by taking and dropping each at once.
const PROBE: &str = concat!(
    "set -u\n",
    slots_sh!(),
    r#"nproc; cat /proc/loadavg; grep '^MemAvailable:' /proc/meminfo
free=0
for ((i = 0; i < cap; i++)); do flock -n -x "$slot_dir/$i" true && free=$((free + 1)); done
echo "slots $cap $free""#
);
/// How long a host has to answer before it counts as unreachable.
const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// Ask every host at once; each answers with its load or why it could not.
pub fn probe(hosts: &[BuildHost]) -> Vec<(BuildHost, std::result::Result<Load, String>)> {
    let (tx, rx) = mpsc::channel();
    let mut children = Vec::new();
    for (index, host) in hosts.iter().enumerate() {
        let mut command = Command::new("ssh");
        command.args(ssh_args()).arg(&host.ssh).arg(remote_command(PROBE, &[]));
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        match command.spawn() {
            Ok(child) => {
                let tx = tx.clone();
                children.push((index, child.id()));
                thread::spawn(move || {
                    let answer = child.wait_with_output().map_err(|e| e.to_string()).and_then(|out| {
                        let text = String::from_utf8_lossy(&out.stdout);
                        match parse_probe(&text) {
                            Some(load) if out.status.success() => Ok(load),
                            _ => Err(last_line(&String::from_utf8_lossy(&out.stderr))
                                .unwrap_or_else(|| format!("no usable answer ({})", out.status))),
                        }
                    });
                    let _ = tx.send((index, answer));
                });
            }
            Err(err) => {
                let _ = tx.send((index, Err(format!("ssh: {err}"))));
            }
        }
    }
    drop(tx);
    let mut answers: Vec<Option<std::result::Result<Load, String>>> = vec![None; hosts.len()];
    let deadline = Instant::now() + PROBE_LIMIT;
    while answers.iter().any(Option::is_none) {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
        match rx.recv_timeout(left) {
            Ok((index, answer)) => answers[index] = Some(answer),
            Err(_) => break,
        }
    }
    // A host that has not answered by now is not used; end its probe. Its
    // thread has not reaped the child yet, so the pid is still that ssh.
    for (index, pid) in children {
        if answers[index].is_none() {
            // SAFETY: kill has no memory preconditions.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }
    hosts
        .iter()
        .cloned()
        .zip(answers)
        .map(|(host, answer)| (host, answer.unwrap_or_else(|| Err("did not answer in time".to_string()))))
        .collect()
}

fn last_line(text: &str) -> Option<String> {
    text.lines().rev().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
}

/// Hosts `auto` may use first, then ones with no offloaded work running,
/// then most free cores, then most free memory, then the order asked for.
pub fn rank(mut hosts: Vec<(BuildHost, Load)>) -> Vec<(BuildHost, Load)> {
    // A stable sort keeps the requested order for equal hosts.
    hosts.sort_by(|a, b| {
        b.1.eligible()
            .cmp(&a.1.eligible())
            .then((b.1.slots_free == b.1.cap).cmp(&(a.1.slots_free == a.1.cap)))
            .then(b.1.free_cores().partial_cmp(&a.1.free_cores()).unwrap_or(std::cmp::Ordering::Equal))
            .then(b.1.mem_available_kib.cmp(&a.1.mem_available_kib))
    });
    hosts
}

/// The host for one command: the first ranked, but for `auto` only one whose
/// own work leaves half its cores free.
pub fn pick(ranked: Vec<(BuildHost, Load)>, named: bool) -> Result<(BuildHost, Load)> {
    if named {
        return ranked.into_iter().next().context("no build host answered");
    }
    let busy: Vec<String> = ranked.iter().map(|(host, load)| describe(host, load)).collect();
    ranked.into_iter().find(|(_, load)| load.eligible()).with_context(|| {
        format!("every build host has half its cores or more busy with its own work: {}", busy.join("; "))
    })
}

/// Probe `hosts`, report the ones that did not answer on standard error,
/// and rank the rest. Fails when none answered.
pub fn ranked(hosts: &[BuildHost], tag: &str) -> Result<Vec<(BuildHost, Load)>> {
    let mut up = Vec::new();
    for (host, answer) in probe(hosts) {
        match answer {
            Ok(load) => up.push((host, load)),
            Err(why) => eprintln!("[{tag}] {} skipped: {why}", host.name),
        }
    }
    if up.is_empty() {
        bail!("no build host answered");
    }
    Ok(rank(up))
}

/// One line per host for the record: `builder 3.2 of 4 cores
/// free, 12.5 GiB free memory, 2 of 2 offload slots free`.
pub fn describe(host: &BuildHost, load: &Load) -> String {
    format!(
        "{} {:.1} of {} cores free, {:.1} GiB free memory, {} of {} offload slots free",
        host.name,
        load.free_cores(),
        load.cpus,
        load.mem_available_kib as f64 / (1024.0 * 1024.0),
        load.slots_free,
        load.cap
    )
}

#[cfg(test)]
#[path = "../tests/offload_hosts.rs"]
mod tests;
