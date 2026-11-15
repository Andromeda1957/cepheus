//! One bounded native PermissionRequest exchange. Failure leaves Claude's UI authoritative.
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const LIMIT: usize = 262_144;
const VERSION: u64 = 16;

struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bounded JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Unique, E> {
                serde_json::Number::from_f64(v).map(|n| Unique(Value::Number(n))).ok_or_else(|| E::custom("number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> Result<Unique, E> {
                self.visit_unit()
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Unique, A::Error> {
                let mut values = Vec::new();
                while let Some(Unique(v)) = a.next_element()? {
                    if values.len() == 1024 {
                        return Err(de::Error::custom("array bound"));
                    }
                    values.push(v);
                }
                Ok(Unique(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Unique, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, Unique(value))) = a.next_entry::<String, Unique>()? {
                    if values.len() == 256 || values.contains_key(&key) {
                        return Err(de::Error::custom("object bound"));
                    }
                    values.insert(key, value);
                }
                Ok(Unique(Value::Object(values)))
            }
        }
        d.deserialize_any(V)
    }
}
fn bounded(v: &Value, depth: usize) -> bool {
    depth <= 16
        && match v {
            Value::String(s) => s.len() <= LIMIT,
            Value::Array(a) => a.iter().all(|v| bounded(v, depth + 1)),
            Value::Object(o) => o.iter().all(|(k, v)| k.len() <= LIMIT && bounded(v, depth + 1)),
            _ => true,
        }
}
fn decode(raw: &[u8]) -> Option<Value> {
    let Unique(v) = serde_json::from_slice(raw).ok()?;
    (v.is_object() && bounded(&v, 0)).then_some(v)
}
fn identity(v: &Value, limit: usize) -> bool {
    v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= limit && !s.chars().any(|c| c < ' ' || c == '\u{7f}'))
}
fn absolute(v: &Value) -> bool {
    identity(v, 4096) && v.as_str().is_some_and(|s| std::path::Path::new(s).is_absolute())
}
fn request(v: &Value) -> bool {
    v["hook_event_name"] == "PermissionRequest"
        && v["tool_name"] == "Read"
        && identity(&v["session_id"], 256)
        && !v["session_id"].as_str().unwrap_or_default().starts_with("served:")
        && identity(&v["prompt_id"], 256)
        && absolute(&v["cwd"])
        && v["tool_input"].as_object().is_some_and(|o| {
            o.keys().all(|k| matches!(k.as_str(), "file_path" | "offset" | "limit"))
                && absolute(&v["tool_input"]["file_path"])
                && [("offset", 10_000_000), ("limit", 10_000)]
                    .iter()
                    .all(|(key, max)| !o.contains_key(*key) || o[*key].as_u64().is_some_and(|n| n > 0 && n <= *max))
        })
}
fn remaining(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| std::io::ErrorKind::TimedOut.into())
}
fn read(stream: &mut UnixStream, buf: &mut [u8], deadline: Instant) -> std::io::Result<()> {
    let mut offset = 0;
    while offset < buf.len() {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let n = stream.read(&mut buf[offset..])?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        offset += n;
    }
    Ok(())
}
fn receive(stream: &mut UnixStream, deadline: Instant) -> Option<Value> {
    let mut size = [0; 4];
    read(stream, &mut size, deadline).ok()?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > LIMIT {
        return None;
    }
    let mut raw = vec![0; size];
    read(stream, &mut raw, deadline).ok()?;
    decode(&raw)
}
fn send(stream: &mut UnixStream, v: Value, deadline: Instant) -> Option<()> {
    let raw = serde_json::to_vec(&v).ok()?;
    if raw.len() > LIMIT {
        return None;
    }
    let mut frame = (raw.len() as u32).to_be_bytes().to_vec();
    frame.extend(raw);
    let mut offset = 0;
    while offset < frame.len() {
        stream.set_write_timeout(Some(remaining(deadline).ok()?)).ok()?;
        let n = stream.write(&frame[offset..]).ok()?;
        if n == 0 {
            return None;
        }
        offset += n;
    }
    Some(())
}
fn stdin(deadline: Instant) -> Option<Vec<u8>> {
    let mut input = std::io::stdin();
    let mut raw = Vec::new();
    while raw.len() <= LIMIT {
        let wait = remaining(deadline).ok()?.as_millis().min(i32::MAX as u128) as i32;
        let mut fd = libc::pollfd { fd: input.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut fd, 1, wait.max(1)) } <= 0 {
            return None;
        }
        let mut buf = [0; 8192];
        let count = (LIMIT + 1 - raw.len()).min(buf.len());
        let n = input.read(&mut buf[..count]).ok()?;
        if n == 0 {
            return Some(raw);
        }
        raw.extend_from_slice(&buf[..n]);
    }
    None
}
fn env(name: &str) -> Option<String> {
    crate::paths::env_var(name)?.into_string().ok()
}
fn positive(s: String) -> Option<u64> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().filter(|n| *n > 0)
}
fn connect(path: &str, deadline: Instant) -> Option<UnixStream> {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path.len() >= address.sun_path.len() {
        return None;
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (out, byte) in address.sun_path.iter_mut().zip(path.bytes()) {
        *out = byte as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
    if fd < 0 {
        return None;
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let rc = unsafe {
        libc::connect(
            fd,
            &address as *const _ as *const libc::sockaddr,
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if rc != 0 {
        if !matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EINPROGRESS | libc::EAGAIN)) {
            return None;
        }
        let mut poll = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
        let wait = remaining(deadline).ok()?.as_millis().min(i32::MAX as u128) as i32;
        if unsafe { libc::poll(&mut poll, 1, wait.max(1)) } <= 0 {
            return None;
        }
        let mut error: libc::c_int = 0;
        let mut len = std::mem::size_of_val(&error) as libc::socklen_t;
        if unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, &mut error as *mut _ as *mut _, &mut len) }
            != 0
            || error != 0
        {
            return None;
        }
    }
    stream.set_nonblocking(false).ok()?;
    Some(stream)
}
fn exchange() -> Option<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let raw = stdin(deadline)?;
    let payload = decode(&raw)?;
    if !request(&payload) {
        return None;
    }
    let socket = env("SOCKET")?;
    if !absolute(&Value::String(socket.clone())) {
        return None;
    }
    let session = positive(env("SESSION")?)?;
    let spawned = positive(std::env::var("CEPHEUS_ATTENTION_SPAWNED_MS").ok()?)?;
    let token = std::env::var("CEPHEUS_PERMISSION_HOOK_TOKEN").ok()?;
    if !identity(&Value::String(token.clone()), 512) {
        return None;
    }
    let mut stream = connect(&socket, deadline)?;
    send(&mut stream, json!({"t":"hello","version":VERSION,"role":"hook"}), deadline)?;
    let welcome = receive(&mut stream, deadline)?;
    if welcome["t"] != "welcome" || welcome["version"].as_u64()? < VERSION {
        return None;
    }
    send(
        &mut stream,
        json!({"t":"permission_request","session":session,"spawned_ms":spawned,"token":token,"payload":payload}),
        deadline,
    )?;
    for skipped in 1..=8 {
        let reply = receive(&mut stream, deadline)?;
        match reply["t"].as_str()? {
            "permission_decision" => {
                if reply["request"] == payload["prompt_id"] && reply["allow"] == true && Instant::now() < deadline {
                    let _ = writeln!(
                        std::io::stdout(),
                        "{}",
                        json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}})
                    );
                }
                return Some(());
            }
            "welcome" if reply["version"].as_u64()? >= VERSION && skipped < 8 => {}
            "state" if skipped < 8 => {}
            _ => return None,
        }
    }
    None
}
pub fn run() -> i32 {
    let _ = exchange();
    0
}

#[cfg(test)]
#[path = "../../src/tests/permission_hook.rs"]
mod tests;
