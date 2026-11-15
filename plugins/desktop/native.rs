//! Fixed-argument calls to desktop and container tools: podman (read-only
//! listing), hyprctl, notify-send and kitty remote control.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

#[cfg(test)]
use crate::{extensions::conventions::parse_podman_ps, protocol::ContainerInfo};

/// Run a command, returning its stdout if it succeeds within `timeout`.
pub fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Hyprland workspace name for each client window's process id.
pub fn parse_hypr_clients(json: &[u8]) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(json) else { return out };
    for item in items {
        let Some(pid) = item.get("pid").and_then(Value::as_u64) else { continue };
        let ws = item.get("workspace").and_then(|w| {
            w.get("name").and_then(Value::as_str).map(str::to_string).or_else(|| w.get("id").map(|i| i.to_string()))
        });
        if let Some(ws) = ws {
            out.insert(pid as u32, ws);
        }
    }
    out
}

pub fn hypr_workspaces() -> HashMap<u32, String> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return HashMap::new();
    }
    run_with_timeout("hyprctl", &["clients", "-j"], Duration::from_secs(3))
        .map(|out| parse_hypr_clients(&out))
        .unwrap_or_default()
}

/// Bring the window owned by `pid` to the front.
#[cfg(test)]
pub fn focus_window(pid: u32) -> bool {
    let target = format!("pid:{pid}");
    run_with_timeout("hyprctl", &["dispatch", "focuswindow", &target], Duration::from_secs(3)).is_some()
}

/// Read only the screen of the Kitty window whose foreground process is
/// pid. Check ownership before and after reading; never send terminal text
/// or its control socket through the hub.
pub fn kitty_process_screen(pid: u32) -> Option<String> {
    let env = crate::procs::environ(pid);
    kitty_window_screen(env.get("KITTY_LISTEN_ON")?, env.get("KITTY_WINDOW_ID")?, pid)
}

fn kitty_window_screen(address: &str, window: &str, pid: u32) -> Option<String> {
    if window.is_empty() || !window.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let matcher = format!("id:{window}");
    let owns_window = || {
        let data = kitty_read(address, "ls", serde_json::json!({"match": matcher}))?;
        let windows: Value = serde_json::from_str(data.as_str()?).ok()?;
        let windows = windows
            .as_array()?
            .iter()
            .flat_map(|os| os.get("tabs").and_then(Value::as_array).into_iter().flatten())
            .flat_map(|tab| tab.get("windows").and_then(Value::as_array).into_iter().flatten());
        let mut matching =
            windows.filter(|w| w.get("id").and_then(Value::as_u64).map(|id| id.to_string()).as_deref() == Some(window));
        let current = matching.next()?;
        (matching.next().is_none()
            && current
                .get("foreground_processes")?
                .as_array()?
                .iter()
                .any(|p| p.get("pid").and_then(Value::as_u64) == Some(u64::from(pid))))
        .then_some(())
    };
    owns_window()?;
    let text = kitty_read(address, "get-text", serde_json::json!({"match": matcher, "extent": "screen"}))?
        .as_str()?
        .to_string();
    owns_window()?;
    Some(text)
}

/// The read-only subset of Kitty's local socket protocol. Bound replies,
/// elapsed time and command names; never connect to a TCP endpoint.
fn kitty_read(address: &str, command: &str, payload: Value) -> Option<Value> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::net::UnixStream;

    if !matches!(command, "ls" | "get-text") {
        return None;
    }
    let address = address.strip_prefix("unix:")?;
    let abstract_name = address.starts_with('@');
    if !abstract_name && !std::path::Path::new(address).is_absolute() {
        return None;
    }
    let name = address.as_bytes();
    // SAFETY: an all-zero sockaddr_un is valid to initialize before filling
    // its address family and bounded pathname below.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if name.is_empty() || name.contains(&0) || name.len() >= address.sun_path.len() {
        return None;
    }
    address.sun_family = libc::AF_UNIX as _;
    for (target, byte) in address.sun_path.iter_mut().zip(name) {
        *target = *byte as _;
    }
    if abstract_name {
        address.sun_path[0] = 0;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + name.len() + usize::from(!abstract_name);
    // SAFETY: socket has no pointer arguments. NONBLOCK also bounds connect
    // when a terminal's Unix socket backlog is full.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is a newly owned socket, transferred exactly once to RAII.
    let mut socket = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: address is initialized and len covers only its bounded path.
    if unsafe { libc::connect(socket.as_raw_fd(), (&address as *const libc::sockaddr_un).cast(), len as _) } != 0 {
        return None;
    }
    socket.set_nonblocking(false).ok()?;
    let timeout = Duration::from_millis(500);
    socket.set_read_timeout(Some(timeout)).ok()?;
    socket.set_write_timeout(Some(timeout)).ok()?;
    const PREFIX: &[u8] = b"\x1bP@kitty-cmd";
    const END: &[u8] = b"\x1b\\";
    let body = serde_json::to_vec(&serde_json::json!({
        "cmd": command, "version": [0, 26, 0], "payload": payload,
        "no_response": false
    }))
    .ok()?;
    socket.write_all(PREFIX).ok()?;
    socket.write_all(&body).ok()?;
    socket.write_all(END).ok()?;
    socket.shutdown(std::net::Shutdown::Write).ok()?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    while bytes.len() < 256 * 1024 && Instant::now() < deadline {
        let count = socket.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.ends_with(END) {
            let response: Value = serde_json::from_slice(bytes.strip_prefix(PREFIX)?.strip_suffix(END)?).ok()?;
            return (response.get("ok")?.as_bool()? && response.get("error").is_none())
                .then(|| response.get("data").cloned())
                .flatten();
        }
    }
    None
}

/// Close one kitty window through its remote-control socket.
pub fn kitty_close_window(listen_on: &str, window_id: &str) -> bool {
    if !window_id.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let matcher = format!("id:{window_id}");
    run_with_timeout("kitten", &["@", "--to", listen_on, "close-window", "--match", &matcher], Duration::from_secs(4))
        .is_some()
}

/// Identical alerts inside this window are one event (status flapping,
/// several hosts relaying the same alert, repeated failure reports).
const NOTIFY_REPEAT: Duration = Duration::from_secs(300);

/// Whether an alert is new: identical summary and body seen within
/// `NOTIFY_REPEAT` are suppressed; anything different still notifies.
fn notify_is_new(summary: &str, body: &str, now: std::time::Instant) -> bool {
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashMap<(String, String), std::time::Instant>>> = OnceLock::new();
    let mut seen = SEEN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    seen.retain(|_, at| now.duration_since(*at) < NOTIFY_REPEAT);
    let key = (summary.to_string(), body.to_string());
    if seen.contains_key(&key) {
        return false;
    }
    seen.insert(key, now);
    true
}

/// `notify-send`'s arguments: the Cepheus name with the installed
/// desktop/icon IDs retained for existing desktop integrations. The icon goes
/// in app_icon (`--app-icon`): notify-send 0.8's `--icon` is a content image
/// that leaves app_icon empty, and Omarchy's shell then shows a letter.
fn notify_args<'a>(summary: &'a str, body: &'a str) -> [&'a str; 7] {
    [
        "--app-name=cepheus",
        "--app-icon=agent-deck",                  // legacy-name: installed icon identity
        "--hint=string:desktop-entry:agent-deck", // legacy-name: installed desktop identity
        "--urgency=normal",
        "--",
        summary,
        body,
    ]
}

/// Desktop notification under the Cepheus name and installed icon. Failures
/// are ignored.
pub fn notify(summary: &str, body: &str) {
    if !notify_is_new(summary, body, std::time::Instant::now()) {
        return;
    }
    let child = Command::new("notify-send")
        .args(notify_args(summary, body))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
#[path = "../../src/tests/external.rs"]
mod tests;

#[derive(Default)]
pub struct TerminalInfo {
    pub pid: Option<u32>,
    pub window: Option<String>,
    pub socket: bool,
    pub workspace: Option<String>,
}
pub fn terminal_info(pid: u32, workspaces: &HashMap<u32, String>) -> TerminalInfo {
    let env = crate::procs::environ(pid);
    let pid = env.get("KITTY_PID").and_then(|p| p.parse::<u32>().ok());
    TerminalInfo {
        pid,
        window: env.get("KITTY_WINDOW_ID").cloned(),
        socket: env.contains_key("KITTY_LISTEN_ON"),
        workspace: pid.and_then(|p| workspaces.get(&p).cloned()),
    }
}
