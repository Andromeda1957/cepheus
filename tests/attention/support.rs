//! Public wire controls plus private fake-process custody. Production input and
//! observation facilities remain with core; immutable P2 cleanup is reused.
use super::*;
use std::os::unix::fs::MetadataExt;

pub fn backend() -> PathBuf {
    std::env::var_os("CEPHEUS_ATTENTION_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_cepheus")))
}
pub fn provenance(binary: &Path) {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    for bytes in [
        include_bytes!("../attention.rs").as_slice(),
        include_bytes!("support.rs").as_slice(),
        include_bytes!("delivery.rs").as_slice(),
        include_bytes!("decisions.rs").as_slice(),
        include_bytes!("compatibility.rs").as_slice(),
        include_bytes!("manual.rs").as_slice(),
        include_bytes!("supervisor.rs").as_slice(),
        include_bytes!("native_permission.rs").as_slice(),
        include_bytes!("target.rs").as_slice(),
        include_bytes!("native_agent.py").as_slice(),
        include_bytes!("fake_agent.py").as_slice(),
        include_bytes!("../job_sessions/support.rs").as_slice(),
    ] {
        digest.update(bytes);
    }
    let fixture = digest.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!(
        "attention-acceptance-provenance: {}",
        json!({"backend_sha256":hash(binary),"fixture_sha256":fixture,"source":std::env::var("CEPHEUS_ATTENTION_TEST_SOURCE").unwrap_or_default(),"host_simulation":"one physical host; fleet and supervisor cases explicitly connect TLS peers","physical_hosts":1})
    );
}
pub fn guard(seconds: u64) -> Duration {
    let configured = std::env::var("CEPHEUS_TEST_TIMEOUT_SCALE").unwrap_or_default();
    let scale = if configured.trim().is_empty() || configured.eq_ignore_ascii_case("auto") {
        let mut load = 0.0;
        let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as f64;
        if unsafe { libc::getloadavg(&mut load, 1) } == 1 {
            (4.0 * load / cpus).clamp(1.0, 10.0)
        } else {
            1.0
        }
    } else {
        let value = configured.parse::<f64>().expect("test timeout scale must be numeric");
        assert!(value.is_finite() && value >= 1.0);
        value
    };
    Duration::from_secs_f64(seconds as f64 * scale)
}
pub fn call_result(service: &Service, operation: &str, args: &Value) -> Result<Value, String> {
    let output = service.run(&["ctl", "fleet", operation, "-"], Some(&serde_json::to_vec(args).unwrap()))?;
    serde_json::from_slice(&output).map_err(|e| e.to_string())
}
pub fn call(service: &Service, operation: &str, args: &Value) -> Value {
    call_result(service, operation, args).unwrap_or_else(|e| panic!("private attention {operation}: {e}"))
}
/// A sender owns its first endpoint/key/target snapshot across lost replies.
/// Retrying never consults a newly reopened row or manufactures a new key.
pub struct FrozenAttentionSend {
    origin: PathBuf,
    pub args: Value,
}
impl FrozenAttentionSend {
    pub fn new(service: &Service, key: &str, id: u64, text: &str) -> Self {
        let row = session(service, id);
        assert!(row["pid"].as_u64().is_some_and(|pid| pid > 1), "captured live hosted process required");
        let spawned =
            row["spawned_ms"].as_u64().filter(|value| *value > 0).expect("protocol17 known target generation required");
        Self {
            origin: service.root.clone(),
            args: json!({"key":key,"session":id,"text":text,"expected_target":{"spawned_ms":spawned,"pid":row["pid"],"conversation":row["conversation"]}}),
        }
    }
    pub fn result(&self, service: &Service) -> Result<Value, String> {
        assert_eq!(service.root, self.origin, "retry must return to the original endpoint");
        call_result(service, "attention-send", &self.args)
    }
    pub fn send(&self, service: &Service) -> Value {
        self.result(service).unwrap_or_else(|error| panic!("frozen attention send: {error}"))
    }
    pub fn id(&self, service: &Service) -> String {
        self.send(service)["message"]["id"].as_str().unwrap().to_owned()
    }
}
/// Preserve the normal load-scaled deadline and original panic, adding only
/// bounded categorical evidence before private scratch custody tears down.
pub fn diagnosed_wait(what: &str, check: impl FnMut() -> bool, evidence: impl FnOnce() -> Value) {
    if let Err(failure) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wait(what, check))) {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(evidence)) {
            Ok(value) => eprintln!("attention-failure-counts: {value}"),
            Err(_) => eprintln!("attention-failure-counts: unavailable"),
        }
        std::panic::resume_unwind(failure);
    }
}
pub fn delivery_counts(root: &Path, service: &Service, targets: &[(&str, u64)]) -> Value {
    let state = service.state();
    let snapshot = attention(service);
    let rows=targets.iter().take(100).map(|(host,id)| {
        let row=state["sessions"].as_array().and_then(|rows|rows.iter().find(|row|row["id"]==*id)).cloned().unwrap_or(Value::Null);
        let mut counts=std::collections::BTreeMap::<String,usize>::new();
        for event in events(root,host,*id) {
            let category=match event["category"].as_str().unwrap_or("other") {
                value@("owner_input"|"owner_input_receipt"|"paste_received"|"draft_visible"|"submitted"|"raw_input"|"status_hook"|"control")=>value,
                _=>"other",
            };
            *counts.entry(category.into()).or_default()+=1;
        }
        json!({"id":id,"host":host,"tool":row["tool"],"status":row["status"],"alive":row["alive"],"inputs":row["inputs"],"conversation_known":row["conversation"].is_string(),"captures":captures(root,host,*id).len(),"events":counts})
    }).collect::<Vec<_>>();
    let messages = snapshot["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .take(200)
        .map(|message| json!({"id":message["id"],"session":message["session"],"state":message["state"]}))
        .collect::<Vec<_>>();
    json!({"sessions":rows,"messages":messages,"queued":snapshot["queued"]})
}
pub fn attention(service: &Service) -> Value {
    call(service, "attention-status", &json!({}))["attention"].clone()
}
pub fn ready(service: &Service) {
    let caps: Value = serde_json::from_slice(&service.run(&["ctl", "fleet", "capabilities"], None).unwrap()).unwrap();
    assert!(caps["protocol"].as_u64().is_some_and(|v| v >= 15), "protocol15 backend required: {caps}");
    assert_eq!(service.state()["attention_capable"], true);
    assert_eq!(attention(service)["supervisor"]["enabled"], false);
}
pub fn install_fake(root: &Path) -> PathBuf {
    for child in ["actors", "controls", "captures", "events"] {
        fs::create_dir_all(root.join(child)).unwrap();
    }
    fs::write(root.join("fixture-policy.json"), json!({"hook_timeout_seconds":guard(30).as_secs_f64()}).to_string())
        .unwrap();
    let path = root.join("fake-agent.py");
    fs::write(&path, include_bytes!("fake_agent.py")).unwrap();
    path
}
pub fn configure(service: &Service, root: &Path, script: &Path, host: &str) {
    service.configure(|config| {
        config["lua"] = json!(false);
        for provider in ["codex", "claude"] {
            config[format!("{provider}_command")] =
                json!(["/usr/bin/python3", script, root, host, provider, service.executable]);
        }
    });
}
pub fn start(service: &Service, tool: &str, host: &str, cwd: &Path, name: &str) -> u64 {
    let bytes = service
        .run(&["ctl", "new", tool, "--host", host, "--cwd", cwd.to_str().unwrap(), "--name", name], None)
        .unwrap();
    String::from_utf8(bytes).unwrap().trim().parse::<u64>().expect("opaque created hosted session id")
}
pub fn key(host: &str, session: u64) -> String {
    format!("{host}-{}", session & ((1u64 << 40) - 1))
}
pub fn control(root: &Path, host: &str, session: u64, mut value: Value) {
    let path = root.join("controls").join(format!("{}.json", key(host, session)));
    let sequence = read_json(&path).and_then(|old| old["sequence"].as_u64()).unwrap_or(0) + 1;
    value["sequence"] = json!(sequence);
    let next = path.with_extension("next");
    fs::write(&next, value.to_string()).unwrap();
    fs::rename(next, path).unwrap();
    wait("fake provider acknowledges its own control generation", || {
        events(root, host, session).iter().any(|event| event["category"] == "control" && event["sequence"] == sequence)
    });
}
pub fn records(path: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}
pub fn events(root: &Path, host: &str, session: u64) -> Vec<Value> {
    records(&root.join("events").join(format!("{}.jsonl", key(host, session))))
}
pub fn captures(root: &Path, host: &str, session: u64) -> Vec<Value> {
    records(&root.join("captures").join(format!("{}.jsonl", key(host, session))))
}
pub fn observe(duration: Duration, mut check: impl FnMut()) {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        check();
        thread::sleep(Duration::from_millis(50));
    }
}
pub fn public(value: &Value) {
    no_private_metadata(value);
    fn attention_metadata(value: &Value) {
        match value {
            Value::Object(map) => {
                for field in
                    ["command", "argv", "env", "stdin", "text", "prompt", "token", "publisher_token", "fingerprint"]
                {
                    assert!(!map.contains_key(field), "private {field} leaked into public attention metadata");
                }
                for value in map.values() {
                    attention_metadata(value)
                }
            }
            Value::Array(values) => values.iter().for_each(attention_metadata),
            _ => {}
        }
    }
    if let Some(snapshot) = value.get("attention") {
        attention_metadata(snapshot)
    } else if value.get("messages").is_some_and(Value::is_array)
        && value.get("supervisor").is_some_and(Value::is_object)
    {
        attention_metadata(value)
    }
    let text = value.to_string();
    for sentinel in ["ATTENTION_PRIVATE_PAYLOAD", "ATTENTION_PRIVATE_COMMAND", "ATTENTION_PRIVATE_FINGERPRINT"] {
        assert!(!text.contains(sentinel), "private attention sentinel leaked into public metadata");
    }
}
fn identity_quiet(value: &Value) -> bool {
    let (Some(pid), Some(start), Some(uid), Some(boot)) =
        (value["pid"].as_u64(), value["start"].as_u64(), value["uid"].as_u64(), value["boot"].as_str())
    else {
        return false;
    };
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let stat = match fs::read_to_string(proc.join("stat")) {
        Ok(v) => v,
        Err(e) => return e.kind() == std::io::ErrorKind::NotFound,
    };
    let Some((_, tail)) = stat.rsplit_once(')') else { return false };
    let fields: Vec<_> = tail.split_whitespace().collect();
    let Some(actual) = fields.get(19).and_then(|v| v.parse::<u64>().ok()) else { return false };
    if actual != start || fields.first() == Some(&"Z") {
        return true;
    }
    let Ok(meta) = fs::metadata(proc) else { return false };
    if meta.uid() as u64 != uid {
        return true;
    }
    fs::read_to_string("/proc/sys/kernel/random/boot_id").is_ok_and(|value| value.trim() != boot)
}
pub struct AttentionFleet {
    inner: Option<Fleet>,
    root: PathBuf,
}
impl AttentionFleet {
    pub fn new(root: &Path, binary: &Path) -> Self {
        Self {
            inner: Some(Fleet {
                hub: Service::new(root.join("hub"), binary, 12288),
                peer: Service::new(root.join("peer"), binary, 14336),
            }),
            root: root.to_path_buf(),
        }
    }
}
impl std::ops::Deref for AttentionFleet {
    type Target = Fleet;
    fn deref(&self) -> &Fleet {
        self.inner.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for AttentionFleet {
    fn deref_mut(&mut self) -> &mut Fleet {
        self.inner.as_mut().unwrap()
    }
}
impl Drop for AttentionFleet {
    fn drop(&mut self) {
        if let Some(fleet) = self.inner.as_ref() {
            for service in [&fleet.hub, &fleet.peer] {
                service.memory(0);
                let _ = call_result(service, "attention-supervisor-off", &json!({"fleet":true}));
                let _ = call_result(service, "queue-enable", &json!({"enabled":false}));
            }
        }
        // Canonical P2 cleanup closes private listeners before stopping daemons
        // and drains independently supervised jobs. Only then inspect fake PTYs.
        drop(self.inner.take());
        let end = Instant::now() + guard(30);
        loop {
            let quiet = fs::read_dir(self.root.join("actors")).is_ok_and(|entries| {
                entries
                    .into_iter()
                    .all(|entry| entry.ok().and_then(|e| read_json(&e.path())).is_some_and(|v| identity_quiet(&v)))
            });
            if quiet {
                return;
            }
            if Instant::now() >= end {
                let _=fs::write(self.root.join("cleanup-incomplete"),"Private hosted fake birth identities remain active; preserve receipts for active-task custody cleanup.\n");
                if !thread::panicking() {
                    panic!("private hosted fake actors must quiesce before scratch deletion");
                }
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Persistent real service clients retain their endpoint identity. Reads are
/// nonblocking and framed incrementally so a partial packet is never discarded.
pub struct Wire {
    socket: std::os::unix::net::UnixStream,
    buffer: Vec<u8>,
    pub seen: Vec<Value>,
    next: u64,
}
impl Wire {
    pub fn new(service: &Service, role: &str, version: u32) -> Self {
        let socket = std::os::unix::net::UnixStream::connect(service.root.join("run/deck.sock")).unwrap();
        socket.set_nonblocking(true).unwrap();
        let mut client = Self { socket, buffer: Vec::new(), seen: Vec::new(), next: 100 };
        client.send(&json!({"t":"hello","version":version,"role":role}));
        client
    }
    pub fn send(&mut self, value: &Value) {
        use std::io::Write;
        let body = serde_json::to_vec(value).unwrap();
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend(body);
        self.socket.set_nonblocking(false).unwrap();
        self.socket.set_write_timeout(Some(guard(10))).unwrap();
        self.socket.write_all(&bytes).unwrap();
        self.socket.set_nonblocking(true).unwrap();
    }
    pub fn poll(&mut self) {
        use std::io::Read;
        let mut chunk = [0; 65536];
        loop {
            match self.socket.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("private socket: {e}"),
            }
        }
        while self.buffer.len() >= 4 {
            let n = u32::from_be_bytes(self.buffer[..4].try_into().unwrap()) as usize;
            assert!(n <= 8 * 1024 * 1024, "private framed state bound");
            if self.buffer.len() < n + 4 {
                break;
            }
            self.seen.push(serde_json::from_slice(&self.buffer[4..n + 4]).unwrap());
            self.buffer.drain(..n + 4);
        }
    }
    pub fn fleet(&mut self, action: &str, args: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"t":"fleet","request":id,"action":action,"args":args}));
        wait("correlated private persistent Fleet response", || {
            self.poll();
            self.seen.iter().any(|v| v["t"] == "fleet_result" && v["request"] == id)
        });
        self.seen.iter().find(|v| v["t"] == "fleet_result" && v["request"] == id).unwrap().clone()
    }
}
pub fn session(service: &Service, id: u64) -> Value {
    service.state()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .expect("hosted session remains listed")
        .clone()
}
pub fn message(service: &Service, id: &str) -> Value {
    call(service, "attention-message", &json!({"id":id}))["message"].clone()
}
pub fn envelope(text: &str) -> String {
    format!(
        "<cross-session-message source=\"cepheus\" encoding=\"html\">\n{}\n</cross-session-message>",
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    )
}
pub fn quiet(root: &Path, host: &str, id: u64, duration: Duration) {
    observe(duration, || {
        assert!(captures(root, host, id).is_empty(), "held automatic message consumed provider input")
    });
}
pub fn control_many(root: &Path, targets: &[(&str, u64)], value: &Value) {
    let mut receipts = Vec::new();
    for (host, id) in targets {
        let path = root.join("controls").join(format!("{}.json", key(host, *id)));
        let sequence = read_json(&path).and_then(|v| v["sequence"].as_u64()).unwrap_or(0) + 1;
        let mut next = value.clone();
        next["sequence"] = json!(sequence);
        let temp = path.with_extension("next");
        fs::write(&temp, next.to_string()).unwrap();
        fs::rename(temp, path).unwrap();
        receipts.push((*host, *id, sequence));
    }
    wait("all private fake control generations", || {
        receipts.iter().all(|(host, id, sequence)| {
            events(root, host, *id).iter().any(|e| e["category"] == "control" && e["sequence"] == *sequence)
        })
    });
}
