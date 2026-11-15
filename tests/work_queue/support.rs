//! Wire-only helpers; the Phase2 custody implementation remains canonical.
use super::*;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn backend() -> PathBuf {
    std::env::var_os("CEPHEUS_QUEUE_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_cepheus")))
}

pub fn provenance(backend: &Path) {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    for data in [
        include_bytes!("../work_queue.rs").as_slice(),
        include_bytes!("support.rs").as_slice(),
        include_bytes!("source_runtime.rs").as_slice(),
        include_bytes!("pause.rs").as_slice(),
        include_bytes!("../job_sessions/support.rs").as_slice(),
    ] {
        digest.update(data);
    }
    let fixture = digest.finish().as_ref().iter().map(|x| format!("{x:02x}")).collect::<String>();
    eprintln!(
        "queue-acceptance-provenance: {}",
        json!({"backend_sha256":hash(backend),"fixture_sha256":fixture,"backend_source":std::env::var("CEPHEUS_QUEUE_TEST_SOURCE").unwrap_or_else(|_|"cargo-built-or-explicit-override; retain build receipt".into()),"physical_hosts":1,"logical_tls_hosts":2})
    );
}

pub fn result(service: &Service, operation: &str, value: &Value) -> Result<Value, String> {
    let (group, action) = operation.split_once('-').expect("typed control group/action");
    let bytes = service.run(&["ctl", group, action, "-"], Some(&serde_json::to_vec(value).unwrap()))?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

pub fn call(service: &Service, operation: &str, value: &Value) -> Value {
    result(service, operation, value).unwrap_or_else(|error| panic!("private queue {operation}: {error}"))
}

pub fn ready(service: &Service) {
    let caps: Value = serde_json::from_slice(&service.run(&["ctl", "fleet", "capabilities"], None).unwrap()).unwrap();
    assert!(caps["protocol"].as_u64().is_some_and(|n| n >= 14), "protocol14 backend required: {caps}");
    assert_eq!(service.state()["work_queue_capable"], true);
}

pub fn items(service: &Service) -> Vec<Value> {
    call(service, "queue-list", &json!({}))["items"].as_array().unwrap().clone()
}

pub fn item(service: &Service, id: u64) -> Value {
    call(service, "queue-list", &json!({"id":id}))["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == id)
        .expect("durable queue item")
        .clone()
}

pub fn add(service: &Service, spec: &Value) -> u64 {
    call(service, "queue-add", spec)["item"]["id"].as_u64().expect("opaque queue identity")
}

pub fn job_id(item: &Value) -> u64 {
    assert_eq!(item["dispatch"]["kind"], "job");
    item["dispatch"]["id"].as_u64().expect("durable queue dispatch receipt")
}

pub fn spec(script: &Path, root: &Path, key: &str, code: i32, form: &str, service: &Service, placement: &str) -> Value {
    let mut input = request(script, root, key, code, form, &service.root.join("work"), placement);
    input["key"] = json!(format!("queue-{key}"));
    input["kind"] = json!("job");
    input
}

pub fn actor_script(root: &Path) -> std::path::PathBuf {
    let path = actor(root);
    let body = fs::read_to_string(&path)
        .unwrap()
        .replace("data = {'pid':", "data = {'started_ns':time.time_ns(), 'pid':")
        .replace(
            "while not (root / 'release').exists():",
            "while not (root / 'release').exists() and not (root / ('release-' + key)).exists():",
        );
    fs::write(&path, body).unwrap();
    path
}

pub fn started(root: &Path, key: &str) -> Option<Value> {
    read_json(&root.join("started").join(key))
}

pub fn release(root: &Path, key: &str) {
    fs::write(root.join(format!("release-{key}")), "release").unwrap();
}

pub fn settled(service: &Service, id: u64, expected: &str) {
    wait("queue item completion reconciled from exact job receipt", || item(service, id)["state"] == expected);
}

pub fn public(value: &Value) {
    no_private_metadata(value);
    fn visit(value: &Value) {
        match value {
            Value::Object(map) => {
                for field in ["argv", "env", "stdin", "prompt", "publisher_token", "token"] {
                    assert!(!map.contains_key(field), "private launch/source credentials leaked into public state");
                }
                map.values().for_each(visit);
            }
            Value::Array(values) => values.iter().for_each(visit),
            _ => {}
        }
    }
    visit(value);
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

pub fn observe(what: &str, duration: Duration, mut check: impl FnMut()) {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        check();
        thread::sleep(Duration::from_millis(50));
    }
    eprintln!("queue-observation-complete: {what}");
}

// Queue dispatch must quiesce before the Phase2 service/worker custody teardown.
// These controls never spawn a missing service. If our deliberate crash was
// followed by a panic, restore only that private daemon with its probe closed.
pub struct QueueService {
    inner: Service,
    running: bool,
}
impl QueueService {
    pub fn new(service: Service) -> Self {
        Self { inner: service, running: false }
    }
    pub fn start(&mut self) {
        self.running = true;
        self.inner.start();
    }
    pub fn crash(&mut self) {
        self.inner.crash();
        self.running = false;
    }
}
impl std::ops::Deref for QueueService {
    type Target = Service;
    fn deref(&self) -> &Service {
        &self.inner
    }
}
impl std::ops::DerefMut for QueueService {
    fn deref_mut(&mut self) -> &mut Service {
        &mut self.inner
    }
}
impl Drop for QueueService {
    fn drop(&mut self) {
        self.inner.memory(0);
        if !self.running {
            self.start();
        }
        let _ = result(&self.inner, "queue-enable", &json!({"enabled":false}));
        if !super::source_runtime::quiesce(&self.inner) && !std::thread::panicking() {
            panic!("source-runtime custody must quiesce before teardown");
        }
    }
}
pub struct QueueFleet(pub Fleet);
impl Drop for QueueFleet {
    fn drop(&mut self) {
        self.0.hub.memory(0);
        self.0.peer.memory(0);
        let _ = result(&self.0.hub, "queue-enable", &json!({"enabled":false}));
        let _ = result(&self.0.peer, "queue-enable", &json!({"enabled":false}));
        let _ = result(&self.0.hub, "source-reconcile", &json!({"owner":"owner","ids":[]}));
        let _ = result(&self.0.peer, "source-reconcile", &json!({"owner":"owner","ids":[]}));
        let hub_quiet = super::source_runtime::quiesce(&self.0.hub);
        let peer_quiet = super::source_runtime::quiesce(&self.0.peer);
        let quiet = hub_quiet && peer_quiet;
        if !quiet && !std::thread::panicking() {
            panic!("fleet source-runtime custody must quiesce before teardown");
        }
    }
}

/// Preserve the owning daemon's birth identity, not merely its Child handle.
pub fn service_birth(service: &Service) -> Value {
    let pid = service.pid();
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let text = fs::read_to_string(proc.join("stat")).unwrap();
    let start = text.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap().parse::<u64>().unwrap();
    json!({"pid":pid,"start":start,"uid":fs::metadata(proc).unwrap().uid(),"boot":fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim()})
}

/// An absent/replaced PID or empty exe() result is never evidence of exec.
/// Match the actual process executable's inode to the atomically replaced
/// private binary while retaining the original PID/start/UID/boot identity.
pub fn handover_ready(service: &Service, birth: &Value) -> bool {
    let Some(pid) = birth["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) else { return false };
    if !alive(pid) {
        return false;
    }
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let Ok(stat) = fs::read_to_string(proc.join("stat")) else { return false };
    let Some(start) =
        stat.rsplit_once(')').and_then(|(_, tail)| tail.split_whitespace().nth(19)).and_then(|s| s.parse::<u64>().ok())
    else {
        return false;
    };
    if birth["start"].as_u64() != Some(start) {
        return false;
    }
    let Ok(metadata) = fs::metadata(&proc) else { return false };
    if birth["uid"].as_u64() != Some(metadata.uid() as u64) {
        return false;
    }
    let Ok(boot) = fs::read_to_string("/proc/sys/kernel/random/boot_id") else { return false };
    if birth["boot"].as_str() != Some(boot.trim()) {
        return false;
    }
    let Ok(expected) = fs::canonicalize(&service.executable) else { return false };
    let Ok(target) = fs::read_link(proc.join("exe")) else { return false };
    if target.as_os_str().is_empty() || target != expected {
        return false;
    }
    match (fs::metadata(proc.join("exe")), fs::metadata(expected)) {
        (Ok(actual), Ok(expected)) => actual.dev() == expected.dev() && actual.ino() == expected.ino(),
        _ => false,
    }
}
