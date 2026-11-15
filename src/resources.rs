//! This machine's CPU, memory and disk use, for the line at the top of the sidebar. Read from `/proc` and
//! `statvfs`, which cost a few microseconds.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Total and idle CPU time from the `cpu` line of `/proc/stat`, in ticks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

pub fn parse_cpu(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line.split_whitespace().skip(1).map_while(|f| f.parse().ok()).collect();
    // user nice system idle iowait irq softirq steal; guest time is already
    // counted in user and nice.
    if fields.len() < 4 {
        return None;
    }
    let total = fields.iter().take(8).sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Some(CpuTimes { total, idle })
}

/// Percent of CPU time busy between two samples.
pub fn cpu_percent(before: CpuTimes, after: CpuTimes) -> Option<f64> {
    let total = after.total.checked_sub(before.total)?;
    let idle = after.idle.checked_sub(before.idle)?;
    (total > 0).then(|| (total.saturating_sub(idle)) as f64 * 100.0 / total as f64)
}

/// Percent of memory in use: everything but `MemAvailable`, which counts the
/// cache the kernel gives back on demand.
pub fn memory_percent(meminfo: &str) -> Option<f64> {
    let field = |name: &str| {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':')?.split_whitespace().next()?.parse::<u64>().ok())
    };
    let total = field("MemTotal").filter(|t| *t > 0)?;
    let available = field("MemAvailable")?;
    Some(total.saturating_sub(available) as f64 * 100.0 / total as f64)
}

/// Space on a disk as `df` counts it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DiskSpace {
    /// Bytes in use, including any held by root's reserved blocks.
    pub used: u64,
    /// Bytes this user can still write.
    pub available: u64,
}

impl DiskSpace {
    /// How full the disk is, as `df`'s Use% column: used over used plus
    /// available, so blocks reserved for root do not count as room.
    pub fn percent_used(&self) -> Option<f64> {
        let room = self.used + self.available;
        (room > 0).then(|| self.used as f64 * 100.0 / room as f64)
    }
}

/// `df`'s arithmetic over a `statvfs` reading: block counts and block size.
pub fn disk_usage(blocks: u64, free: u64, available: u64, block_size: u64) -> DiskSpace {
    DiskSpace { used: blocks.saturating_sub(free) * block_size, available: available * block_size }
}

/// Used and available space on the filesystem holding `path`.
pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes into the zeroed struct we pass.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated string and st is writable.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(disk_usage(st.f_blocks as u64, st.f_bfree as u64, st.f_bavail as u64, st.f_frsize as u64))
}

/// A size as the line shows it: `312G`, `1.4T`, `850M`.
pub fn short_size(bytes: u64) -> String {
    const G: f64 = 1024.0 * 1024.0 * 1024.0;
    let gib = bytes as f64 / G;
    if gib >= 1000.0 {
        format!("{:.1}T", gib / 1024.0)
    } else if gib >= 10.0 {
        format!("{gib:.0}G")
    } else if gib >= 1.0 {
        format!("{gib:.1}G")
    } else {
        format!("{:.0}M", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resources {
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    /// The disk the sessions start on.
    pub disk: Option<DiskSpace>,
}

/// Samples kept between reads, since CPU use is a difference over time.
#[derive(Default)]
pub struct Sampler {
    last_cpu: Option<CpuTimes>,
    pub now: Resources,
}

impl Sampler {
    pub fn sample(&mut self, disk_path: &Path) {
        let cpu = fs::read_to_string("/proc/stat").ok().as_deref().and_then(parse_cpu);
        if let (Some(before), Some(after)) = (self.last_cpu, cpu) {
            if let Some(percent) = cpu_percent(before, after) {
                self.now.cpu = Some(percent);
            }
        }
        if cpu.is_some() {
            self.last_cpu = cpu;
        }
        self.now.memory = fs::read_to_string("/proc/meminfo").ok().as_deref().and_then(memory_percent);
        self.now.disk = disk_space(disk_path);
    }
}

#[cfg(test)]
#[path = "tests/resources.rs"]
mod tests;
