//! Read-only process inspection through /proc.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    pub state: char,
    pub start_ticks: u64,
}

/// Parse the contents of /proc/<pid>/stat.
pub fn parse_stat(pid: u32, text: &str) -> Option<ProcInfo> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // rest[0] is field 3 (state); ppid is field 4, starttime field 22.
    let state = rest.first()?.chars().next()?;
    let ppid = rest.get(1)?.parse().ok()?;
    let start_ticks = rest.get(19)?.parse().ok()?;
    Some(ProcInfo { pid, ppid, comm, state, start_ticks })
}

pub fn stat(pid: u32) -> Option<ProcInfo> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(pid, &text)
}

pub fn scan() -> HashMap<u32, ProcInfo> {
    let mut out = HashMap::new();
    let Ok(dir) = fs::read_dir("/proc") else { return out };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if let Some(info) = stat(pid) {
            out.insert(pid, info);
        }
    }
    out
}

pub fn cmdline(pid: u32) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|&c| c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
        })
        .unwrap_or_default()
}

pub fn environ(pid: u32) -> HashMap<String, String> {
    let Ok(bytes) = fs::read(format!("/proc/{pid}/environ")) else { return HashMap::new() };
    bytes
        .split(|&c| c == 0)
        .filter_map(|entry| {
            let s = String::from_utf8_lossy(entry);
            let (k, v) = s.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

pub fn cwd(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

pub fn stdin_is_tty(pid: u32) -> bool {
    fs::read_link(format!("/proc/{pid}/fd/0")).map(|p| p.to_string_lossy().starts_with("/dev/pts/")).unwrap_or(false)
}

pub fn alive_with_start(pid: u32, start_ticks: Option<u64>) -> bool {
    match stat(pid) {
        Some(info) => info.state != 'Z' && info.state != 'X' && start_ticks.is_none_or(|t| t == info.start_ticks),
        None => false,
    }
}

pub fn is_descendant(procs: &HashMap<u32, ProcInfo>, pid: u32, ancestor: u32) -> bool {
    let mut cur = pid;
    for _ in 0..64 {
        let Some(info) = procs.get(&cur) else { return false };
        if info.ppid == ancestor {
            return true;
        }
        if info.ppid <= 1 {
            return false;
        }
        cur = info.ppid;
    }
    false
}

pub fn descendants(procs: &HashMap<u32, ProcInfo>, root: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for info in procs.values() {
        children.entry(info.ppid).or_default().push(info.pid);
    }
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if let Some(kids) = children.get(&pid) {
            for &kid in kids {
                if out.len() > 4096 {
                    return out;
                }
                out.push(kid);
                stack.push(kid);
            }
        }
    }
    out
}

/// A program's processes: `root` and its descendants, without the subtree of
/// another process named `comm` below the first one at or under `root`. A
/// launcher may run the program as its child, and another copy the program
/// runs as a tool (a hosted Codex running `codex exec`) is not the program.
pub fn program_tree(procs: &HashMap<u32, ProcInfo>, root: u32, comm: &str) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for info in procs.values() {
        children.entry(info.ppid).or_default().push(info.pid);
    }
    let named = |pid: u32| procs.get(&pid).is_some_and(|p| p.comm == comm);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Each entry carries whether the program itself is at or above it.
    let mut stack = vec![(root, false)];
    while let Some((pid, inside)) = stack.pop() {
        if !seen.insert(pid) || (inside && named(pid)) || out.len() > 4096 {
            continue;
        }
        out.push(pid);
        let inside = inside || named(pid);
        for &kid in children.get(&pid).into_iter().flatten() {
            stack.push((kid, inside));
        }
    }
    out
}

/// Whether `/proc/<pid>/fdinfo/<fd>` text describes a descriptor opened for
/// writing: its octal `flags` access mode is write-only or read-write.
pub fn fdinfo_writable(text: &str) -> bool {
    text.lines()
        .find_map(|l| l.strip_prefix("flags:"))
        .and_then(|f| u32::from_str_radix(f.trim(), 8).ok())
        .is_some_and(|flags| flags & libc::O_ACCMODE as u32 != libc::O_RDONLY as u32)
}

/// Files `pid` holds open for writing that `wanted` accepts (empty when it is
/// gone or not ours). A program that only reads a file, as a log reader
/// does, holds it read-only and is left out.
pub fn files_open_for_writing(pid: u32, wanted: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let Ok(dir) = fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
    let mut out = Vec::new();
    for fd in dir.flatten() {
        let Ok(path) = fs::read_link(fd.path()) else { continue };
        if !path.is_absolute() || !wanted(&path) || out.contains(&path) {
            continue;
        }
        let info = fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", fd.file_name().to_string_lossy()));
        if info.is_ok_and(|text| fdinfo_writable(&text)) {
            out.push(path);
        }
    }
    out
}

fn boot_time() -> u64 {
    fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok())))
        .unwrap_or(0)
}

/// Unix seconds at which a process with the given start ticks began.
pub fn start_secs(start_ticks: u64) -> u64 {
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = if hz > 0 { hz as u64 } else { 100 };
    boot_time() + start_ticks / hz
}

#[cfg(test)]
#[path = "tests/procs.rs"]
mod tests;
