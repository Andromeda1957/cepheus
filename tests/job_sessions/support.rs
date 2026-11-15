//! Private black-box fixtures. Every command uses the explicitly selected build,
//! its own socket/state/config and synthetic credentials, never an owner service.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

// Mirror shared test deadlines: hang guards scale with real host load;
// scheduler probes below remain controlled inputs, not this timing source.
fn hang_guard(seconds: u64) -> Duration {
    static SCALE: OnceLock<f64> = OnceLock::new();
    let scale = *SCALE.get_or_init(|| {
        let configured = std::env::var("CEPHEUS_TEST_TIMEOUT_SCALE").unwrap_or_default();
        if !configured.trim().is_empty() && !configured.eq_ignore_ascii_case("auto") {
            let scale =
                configured.trim().parse::<f64>().expect("CEPHEUS_TEST_TIMEOUT_SCALE must be a number >=1 or auto");
            assert!(scale.is_finite() && scale >= 1.0, "CEPHEUS_TEST_TIMEOUT_SCALE must be finite and >=1");
            return scale;
        }
        let mut load = 0.0;
        if unsafe { libc::getloadavg(&mut load, 1) } != 1 {
            return 1.0;
        }
        let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as f64;
        (4.0 * load / cpus).clamp(1.0, 10.0)
    });
    Duration::try_from_secs_f64(seconds as f64 * scale).expect("configured hang guard exceeds Duration")
}
static NEXT: AtomicU64 = AtomicU64::new(1);

pub fn binary() -> PathBuf {
    let path = std::env::var_os("CEPHEUS_JOB_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_cepheus")));
    assert!(path.is_absolute() && path.is_file(), "job test backend must be an absolute executable path");
    path
}

pub fn hash(path: &Path) -> String {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut file = File::open(path).unwrap();
    let mut buffer = [0; 65536];
    loop {
        let size = file.read(&mut buffer).unwrap();
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    digest.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "deck-jobs-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if self.0.join("cleanup-incomplete").exists() {
            eprintln!("private fixture scratch retained for active-task cleanup: {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn wait(what: &str, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < hang_guard(60), "timed out: {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn read_json(path: &Path) -> Option<Value> {
    let file = File::open(path).ok()?;
    serde_json::from_reader(file.take(65537)).ok()
}

/// Fields drop in declaration order: close the originating hub before its
/// peer. A live upstream can otherwise make the detached listener start a new
/// peer daemon while the fixture is deleting the old daemon's receipts.
pub struct Fleet {
    pub hub: Service,
    pub peer: Service,
}

pub struct Service {
    pub root: PathBuf,
    pub executable: PathBuf,
    child: Option<Child>,
    stopped: bool,
}
impl Service {
    pub fn new(root: PathBuf, source: &Path, memory_mib: u64) -> Self {
        for path in ["bin", "work", "state", "probe/pressure"] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let executable = root.join("bin/cepheus");
        fs::copy(source, &executable).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        for tool in ["claude", "codex", "grok", "opencode", "agy", "antigravity"] {
            let path = root.join("bin").join(tool);
            fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(root.join("probe/pressure/memory"), "full avg10=0\n").unwrap();
        fs::write(root.join("probe/pressure/cpu"), "some avg60=0\n").unwrap();
        fs::write(root.join("probe/loadavg"), "0 0 0 1/1 1\n").unwrap();
        let config = json!({
            "start_dir": root.join("work"), "notifications": false, "claude_hooks": false, "codex_notify": false,
            "sync_logins": false, "auto_continue": [],
            "dev_server_command": ["/bin/sleep", "600"],
            "scheduler": {"probe":{"root":root.join("probe")},"stagger_seconds":0,"repositories":{"job-fixture":root.join("work")}}
        });
        fs::write(root.join("config.json"), config.to_string()).unwrap();
        let result = Self { root, executable, child: None, stopped: false };
        result.memory(memory_mib);
        result
    }
    pub fn memory(&self, available_mib: u64) {
        fs::write(
            self.root.join("probe/meminfo"),
            format!("MemTotal: 16777216 kB\nMemAvailable: {} kB\n", available_mib * 1024),
        )
        .unwrap();
    }
    pub fn configure(&self, change: impl FnOnce(&mut Value)) {
        let mut config = read_json(&self.root.join("config.json")).unwrap();
        change(&mut config);
        fs::write(self.root.join("config.json"), config.to_string()).unwrap();
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.executable);
        cmd.env("CEPHEUS_SOCKET", self.root.join("run/deck.sock"))
            .env("CEPHEUS_STATE_DIR", self.root.join("state"))
            .env("CEPHEUS_CONFIG", self.root.join("config.json"))
            .env("CEPHEUS_START_DIR", self.root.join("work"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("CEPHEUS_ANTIGRAVITY_DIR", self.root.join("agy"))
            .env("CEPHEUS_TEMP_DIR", self.root.join("clients"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("FLEET_TEST_INHERITED", "synthetic-inherited-must-not-reach-job");
        // A test run from an agent's shell still starts its own private
        // service there, not one in the owner's session (`owner_env.rs`).
        for marker in ["CEPHEUS_SESSION", "AGENT_DECK_SESSION", "CLAUDECODE", "AI_AGENT", "CODEX_THREAD_ID", "CODEX_CI"]
        // legacy-name
        {
            cmd.env_remove(marker);
        }
        cmd
    }
    pub fn run(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let mut command = self.command();
        command.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let input_error = input.and_then(|bytes| child.stdin.take().unwrap().write_all(bytes).err());
        drop(child.stdin.take());
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out = thread::spawn(move || {
            let mut data = Vec::new();
            stdout.take(2 * 1024 * 1024 + 1).read_to_end(&mut data).unwrap();
            data
        });
        let err = thread::spawn(move || {
            let mut data = Vec::new();
            stderr.take(65537).read_to_end(&mut data).unwrap();
            data
        });
        let deadline = Instant::now() + hang_guard(30);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(result.err().map_or_else(|| "private CLI timed out".to_string(), |e| e.to_string()));
                }
            }
        };
        let bytes = out.join().unwrap();
        let errors = err.join().unwrap();
        let status = status?;
        if let Some(error) = input_error {
            return Err(error.to_string());
        }
        if bytes.len() > 2 * 1024 * 1024 || errors.len() > 65536 {
            return Err("private CLI output exceeded its bound".into());
        }
        if !status.success() {
            return Err(String::from_utf8_lossy(&errors).into_owned());
        }
        Ok(bytes)
    }
    pub fn job_result(&self, operation: &str, request: &Value) -> Result<Value, String> {
        let bytes = self.run(&["ctl", "job", operation, "-"], Some(&serde_json::to_vec(request).unwrap()))?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }
    pub fn job(&self, operation: &str, request: &Value) -> Value {
        self.job_result(operation, request).unwrap_or_else(|e| panic!("private job {operation}: {e}"))
    }
    pub fn state(&self) -> Value {
        serde_json::from_slice(&self.run(&["ctl", "status"], None).unwrap()).unwrap()
    }
    pub fn jobs(&self) -> Vec<Value> {
        self.job("status", &json!({}))["jobs"].as_array().unwrap().clone()
    }
    pub fn info(&self, id: u64) -> Value {
        self.job("status", &json!({"job":id}))["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"] == id)
            .unwrap()
            .clone()
    }
    pub fn start(&mut self) {
        assert!(self.child.is_none());
        self.stopped = false;
        let log = OpenOptions::new().create(true).append(true).open(self.root.join("service.log")).unwrap();
        let mut cmd = self.command();
        cmd.arg("service").stdin(Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log);
        self.child = Some(cmd.spawn().unwrap());
        wait("private service readiness", || self.run(&["ctl", "fleet", "capabilities"], None).is_ok());
        let caps: Value = serde_json::from_slice(&self.run(&["ctl", "fleet", "capabilities"], None).unwrap()).unwrap();
        assert!(
            caps["protocol"].as_u64().is_some_and(|v| v >= 14)
                && caps["facilities"].as_array().is_some_and(|xs| xs.contains(&json!("jobs"))),
            "the explicit test binary must support protocol14/jobs: {caps}"
        );
    }
    pub fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }
    pub fn control(&self, args: &[&str]) {
        self.run(args, None).unwrap();
    }
    pub fn replace_binary(&self, source: &Path) {
        let temporary = self.root.join("bin/.replacement");
        fs::copy(source, &temporary).unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(temporary, &self.executable).unwrap();
    }
    pub fn stop(&mut self) {
        self.close_sessions().unwrap();
        self.control(&["stop", "--yes"]);
        let child = self.child.as_mut().unwrap();
        wait("owned service shutdown", || child.try_wait().unwrap().is_some());
        self.child = None;
        self.stopped = true;
    }
    fn close_sessions(&self) -> Result<(), String> {
        let state: Value = serde_json::from_slice(&self.run(&["ctl", "status"], None)?).map_err(|e| e.to_string())?;
        for session in state["sessions"].as_array().into_iter().flatten() {
            if let Some(id) = session["id"].as_u64() {
                self.run(&["ctl", "close", &id.to_string()], None)?;
            }
        }
        let deadline = Instant::now() + hang_guard(5);
        loop {
            let state: Value =
                serde_json::from_slice(&self.run(&["ctl", "status"], None)?).map_err(|e| e.to_string())?;
            if state["sessions"].as_array().into_iter().flatten().all(|session| session["alive"] != true) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("fixture sessions did not close".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn close_listener(&self) -> bool {
        let config_path = self.root.join("config.json");
        let Some(mut config) = read_json(&config_path) else {
            return false;
        };
        if config["peer_listen"].as_str().is_none_or(str::is_empty) {
            return true;
        }
        config["peer_listen"] = json!("");
        let temporary = self.root.join("config.shutdown.json");
        if fs::write(&temporary, config.to_string()).and_then(|_| fs::rename(&temporary, &config_path)).is_err() {
            return false;
        }
        let lock_path = self.root.join("state/peer/peer.lock");
        let deadline = Instant::now() + hang_guard(10);
        loop {
            if !lock_path.exists() {
                return true;
            }
            let Ok(file) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
                return false;
            };
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    fn workers_quiet(&self) -> bool {
        let jobs = self.root.join("state/jobs");
        if !jobs.exists() {
            return true;
        }
        let Ok(entries) = fs::read_dir(jobs) else {
            return false;
        };
        let mut leaders = HashMap::new();
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let path = entry.path();
            for name in ["owner.json", "launch.json"] {
                let receipt = path.join(name);
                if !receipt.exists() {
                    continue;
                }
                let Some(value) = read_json(&receipt) else {
                    return false;
                };
                if name == "owner.json" {
                    if identity_alive(&value) {
                        return false;
                    }
                } else {
                    if identity_alive(&value["supervisor"]) || identity_alive(&value["child"]) {
                        return false;
                    }
                    if let Some(pid) = value["child"]["pid"].as_u64() {
                        leaders.insert(pid, value["child"].clone());
                    }
                }
            }
        }
        let started = self.root.parent().unwrap().join("started");
        let Ok(entries) = fs::read_dir(started) else {
            return true;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let Some(record) = read_json(&entry.path()) else {
                return false;
            };
            let Some(identity) = record["pid"].as_u64().and_then(|pid| leaders.get(&pid)) else {
                continue;
            };
            if record["start"].as_str().and_then(|s| s.parse::<u64>().ok()) != identity["start"].as_u64() {
                return false;
            }
            if let Some(child) = record["child"].as_u64() {
                let Some(start) = record["child_start"].as_str().and_then(|s| s.parse::<u64>().ok()) else {
                    return false;
                };
                let child = json!({"pid":child,"start":start,"uid":identity["uid"],"boot":identity["boot"]});
                if identity_alive(&child) {
                    return false;
                }
            }
        }
        true
    }
    pub fn crash(&mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        // Turn off this private detached listener before stopping its daemon.
        // Otherwise peer::connection can connect_or_spawn a replacement.
        let listener_closed = self.close_listener();
        if self.stopped && self.workers_quiet() && listener_closed {
            return;
        }
        // A panic after our deliberate daemon crash still must clean our actors.
        // Restart only this fixture's private service, then use its stop API.
        if self.child.is_none() && !self.stopped {
            if let Ok(log) = OpenOptions::new().create(true).append(true).open(self.root.join("service.log")) {
                if let Ok(error_log) = log.try_clone() {
                    self.child =
                        self.command().arg("service").stdin(Stdio::null()).stdout(log).stderr(error_log).spawn().ok();
                    let deadline = Instant::now() + hang_guard(5);
                    while Instant::now() < deadline && self.run(&["ctl", "fleet", "capabilities"], None).is_err() {
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
        if !self.stopped {
            if let Ok(jobs) = self.job_result("status", &json!({})) {
                if let Some(jobs) = jobs["jobs"].as_array() {
                    for job in jobs {
                        if terminal(job) && !job["reason"].as_str().is_some_and(|r| r.contains("supervisor lost")) {
                            continue;
                        }
                        if let Some(id) = job["id"].as_u64() {
                            let _ = self.job_result("stop", &json!({"job":id}));
                        }
                    }
                }
            }
        }
        if !self.stopped {
            let _ = self.close_sessions();
            let _ = self.run(&["stop", "--yes"], None);
        }
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + hang_guard(5);
            loop {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        // The daemon and listener are now quiescent. Keep private receipts
        // until all independently supervised leaders and recorded descendants
        // are gone; final publication must never race removal of its parent.
        let deadline = Instant::now() + hang_guard(15);
        while !self.workers_quiet() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        let quiet = self.workers_quiet();
        if !quiet || !listener_closed {
            let root = self.root.parent().unwrap();
            let _ = fs::write(
                root.join("cleanup-incomplete"),
                "Owned fixture listener or job identities still active; preserve receipts until active-task cleanup.\n",
            );
            eprintln!(
                "fixture cleanup incomplete: {} listener_closed={listener_closed} workers_quiet={quiet}",
                self.root.display()
            );
            if !std::thread::panicking() {
                panic!("owned fixture processes must quiesce before deleting receipts");
            }
        }
    }
}

/// Conservative identity check used only for preserving this fixture's private
/// receipts during teardown. It never sends a signal or follows a reused PID.
fn identity_alive(identity: &Value) -> bool {
    let (Some(pid), Some(start), Some(uid), Some(boot)) =
        (identity["pid"].as_u64(), identity["start"].as_u64(), identity["uid"].as_u64(), identity["boot"].as_str())
    else {
        return true;
    };
    if pid <= 1 || uid != unsafe { libc::geteuid() } as u64 {
        return true;
    }
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    let Ok(stat) = fs::read_to_string(proc_dir.join("stat")) else {
        return false;
    };
    let Some((_, fields)) = stat.rsplit_once(')') else {
        return true;
    };
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    fields.first().is_none_or(|state| *state != "Z")
        && fields.get(19).and_then(|s| s.parse::<u64>().ok()) == Some(start)
        && fs::metadata(proc_dir).is_ok_and(|m| m.uid() as u64 == uid)
        && fs::read_to_string("/proc/sys/kernel/random/boot_id").is_ok_and(|b| b.trim() == boot)
}

pub fn connect(hub: &Service, peer: &Service) {
    let port = loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        if port != 8000 {
            break port;
        }
    };
    let fingerprint = String::from_utf8(peer.run(&["peer", "fingerprint"], None).unwrap()).unwrap().trim().to_string();
    let key = "af".repeat(32);
    fs::create_dir_all(hub.root.join("state/hub")).unwrap();
    fs::write(hub.root.join("state/hub/key"), &key).unwrap();
    fs::set_permissions(hub.root.join("state/hub/key"), fs::Permissions::from_mode(0o600)).unwrap();
    peer.run(&["peer", "authorize"], Some(key.as_bytes())).unwrap();
    peer.configure(|c| {
        c["peer_listen"] = json!(format!("127.0.0.1:{port}"));
        c["peer_allow"] = json!(["127.0.0.1/32"]);
    });
    hub.configure(|c| {
        c["hosts"] =
            json!([{"name":"worker","tag":1,"addresses":[format!("127.0.0.1:{port}")],"fingerprint":fingerprint}])
    });
}

pub fn actor(root: &Path) -> PathBuf {
    for dir in ["started", "finished", "duplicates"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let path = root.join("actor.py");
    fs::write(&path,r#"import json, os, pathlib, subprocess, sys, time
root, key, code, form = pathlib.Path(sys.argv[1]), sys.argv[2], int(sys.argv[3]), sys.argv[4]
record = root / 'started' / key
try:
    output = record.open('x')
except FileExistsError:
    (root / 'duplicates' / key).write_text('duplicate')
    raise SystemExit(93)
data = {'pid':os.getpid(), 'start':pathlib.Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()[19], 'cwd':os.getcwd(), 'argv':sys.argv[5:], 'env':os.environ.get('FLEET_EXPLICIT'), 'inherited':os.environ.get('FLEET_TEST_INHERITED'), 'stdin':sys.stdin.read()}
if form == 'tree':
    child = subprocess.Popen([sys.executable, '-c', 'import os,time;time.sleep(float(os.environ[\"FLEET_TEST_TIMEOUT_SECONDS\"]))'])
    data['child'] = child.pid
    data['child_start'] = pathlib.Path('/proc/'+str(child.pid)+'/stat').read_text().rsplit(')',1)[1].split()[19]
with output: json.dump(data, output)
if form != 'json': print('begin ' + key, flush=True)
deadline = time.monotonic() + float(os.environ['FLEET_TEST_TIMEOUT_SECONDS'])
while not (root / 'release').exists():
    if time.monotonic() > deadline: raise SystemExit(91)
    time.sleep(0.05)
if form == 'json':
    for index in range(650): print(json.dumps({'type':'item.completed','item':{'id':str(index),'type':'agent_message','text':'bounded message '+str(index)}}), flush=True)
    print(json.dumps({'type':'item.completed','item':{'id':'final','type':'agent_message','text':'fleet result '+key}}), flush=True)
else: print('end ' + key, flush=True)
(root / 'finished' / key).write_text(str(code))
raise SystemExit(code)
"#).unwrap();
    path
}

pub fn request(script: &Path, root: &Path, index: &str, code: i32, form: &str, cwd: &Path, placement: &str) -> Value {
    let mut value = json!({"key":format!("fleet-{index}"),"title":format!("Job {index}"),"argv":["/usr/bin/python3",script,root,index,code.to_string(),form,"literal $(touch must-not-be-executed)","argv-private-marker"],"cwd":cwd,"placement":placement,"stdin":"stdin-private-marker","env_clear":true,"env_allowlist":["PATH"],"env":{"FLEET_EXPLICIT":"env-private-marker","FLEET_TEST_TIMEOUT_SECONDS":hang_guard(180).as_secs_f64().to_string()}});
    if form == "json" {
        value["provider"] = json!("codex");
    }
    value
}

pub fn terminal(job: &Value) -> bool {
    matches!(job["status"].as_str(), Some("finished" | "failed"))
}
pub fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(')').map(|(_, s)| s.trim_start().starts_with('Z')))
        .is_some_and(|zombie| !zombie)
}
pub fn exe(pid: u32) -> String {
    fs::read_link(format!("/proc/{pid}/exe")).map(|x| x.to_string_lossy().into_owned()).unwrap_or_default()
}
pub fn private_modes(root: &Path) {
    assert_eq!(fs::metadata(root).unwrap().permissions().mode() & 0o777, 0o700, "private jobs directory");
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(!metadata.file_type().is_symlink(), "private job files cannot escape through symlinks");
        let mode = metadata.permissions().mode() & 0o777;
        if metadata.is_dir() {
            assert_eq!(mode, 0o700);
            private_modes(&path);
        } else {
            assert_eq!(mode, 0o600, "{}", path.display());
        }
    }
}
pub fn no_private_metadata(value: &Value) {
    let text = serde_json::to_string(value).unwrap();
    for marker in
        ["argv-private-marker", "env-private-marker", "stdin-private-marker", "synthetic-inherited-must-not-reach-job"]
    {
        assert!(!text.contains(marker), "private job input leaked into public metadata");
    }
}

/// Pin the exact supervisor belonging to this actor before the deliberate fault.
/// The pidfd prevents a process-ID reuse race between validation and SIGKILL.
pub fn kill_supervisor(service: &Service, id: u64, actor: &Value) {
    let dir = service.root.join("state/jobs").join(format!("{id:010x}"));
    let launch = read_json(&dir.join("launch.json")).expect("durable launch receipt");
    let intent = read_json(&dir.join("intent.json")).unwrap();
    assert_eq!(launch["nonce"], intent["nonce"]);
    assert_eq!(launch["child"]["pid"], actor["pid"]);
    assert_eq!(launch["child"]["start"].as_u64().unwrap().to_string(), actor["start"].as_str().unwrap());
    assert_eq!(launch["pgid"], actor["pid"]);
    let identity = &launch["supervisor"];
    let pid = identity["pid"].as_u64().unwrap() as u32;
    assert!(pid > 1 && pid != service.pid());
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(raw >= 0, "pin owned supervisor: {}", std::io::Error::last_os_error());
    let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let proc_dir = std::path::PathBuf::from(format!("/proc/{pid}"));
    assert_eq!(identity["uid"].as_u64(), Some(fs::metadata(&proc_dir).unwrap().uid() as u64));
    assert_eq!(identity["uid"].as_u64(), Some(unsafe { libc::geteuid() } as u64));
    assert_eq!(identity["boot"].as_str(), Some(fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim()));
    let stat = fs::read_to_string(proc_dir.join("stat")).unwrap();
    let start = stat.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap().parse::<u64>().unwrap();
    assert_eq!(identity["start"].as_u64(), Some(start));
    let cmdline = fs::read(proc_dir.join("cmdline")).unwrap();
    let args = cmdline.split(|b| *b == 0).collect::<Vec<_>>();
    assert_eq!(args.get(1).copied(), Some(b"__job-supervise".as_slice()));
    assert_eq!(args.get(2).copied(), Some(dir.as_os_str().as_encoded_bytes()));
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        },
        0,
        "kill exact owned supervisor"
    );
}
