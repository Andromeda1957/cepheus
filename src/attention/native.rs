//! Local native permission replies. This is separate from guarded PTY input.
//! The hook must consume Welcome before sending its one request, so no ordinary
//! frame can interleave with the dedicated one-shot response on a Hook socket.
use super::*;
use crate::attention::{native_policy, store};
use crate::job_sessions::Identity;
use std::os::unix::fs::MetadataExt;

const NATIVE_WINDOW: Duration = Duration::from_millis(1200);
#[derive(Clone)]
pub(super) struct Endpoint {
    socket: Arc<UnixStream>,
    peer: Identity,
    pub canceled: Arc<AtomicBool>,
    used: Arc<AtomicBool>,
}
impl Endpoint {
    pub(super) fn from_stream(stream: &UnixStream) -> Option<Self> {
        let mut cred = libc::ucred { pid: 0, uid: u32::MAX, gid: u32::MAX };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if rc != 0 || cred.pid <= 1 || cred.uid != unsafe { libc::geteuid() } {
            return None;
        }
        Some(Self {
            socket: Arc::new(stream.try_clone().ok()?),
            peer: Identity::read(cred.pid as u32).ok()?,
            canceled: Arc::new(AtomicBool::new(false)),
            used: Arc::new(AtomicBool::new(false)),
        })
    }
    /// The connected process: its pid and start time when it connected.
    pub(super) fn peer(&self) -> (u32, u64) {
        (self.peer.pid, self.peer.start)
    }
    fn send(&self, request: &str, allow: bool, deadline: Instant) -> bool {
        if self.canceled.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return false;
        }
        let frame = encode(&ServerMsg::PermissionDecision { request: request.into(), allow });
        // Exactly one nonblocking syscall. A partial frame is neutral at the
        // strict helper; neither the remainder nor an approval is ever retried.
        let wrote = unsafe {
            libc::send(
                self.socket.as_raw_fd(),
                frame.as_ptr() as *const libc::c_void,
                frame.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        wrote == frame.len() as isize
    }
}
fn authorized_write(
    endpoint: &Endpoint,
    request: &str,
    deadline: Instant,
    input: &Mutex<u64>,
    epoch: u64,
    authority: &Mutex<u64>,
    revision: u64,
) -> bool {
    let input = input.lock().unwrap_or_else(|e| e.into_inner());
    let authority = authority.lock().unwrap_or_else(|e| e.into_inner());
    *input == epoch && *authority == revision && endpoint.send(request, true, deadline)
}
#[derive(serde::Serialize, serde::Deserialize)]
struct NativeIdentity {
    token: String,
    spawned_ms: u64,
    helper: PathBuf,
    interpreter: PathBuf,
}
#[derive(Clone, serde::Serialize)]
struct Receipt {
    session: u64,
    spawned_ms: u64,
    revision: u64,
    at: u64,
    category: String,
    rule: Option<String>,
}

fn qualified_path(path: &Path) -> bool {
    path.is_absolute() && path.to_str().is_some_and(|s| !s.contains("${") && !s.chars().any(char::is_control))
}
pub(super) fn interpreter() -> Result<PathBuf> {
    let path = std::env::current_exe()?.canonicalize()?;
    let meta = fs::metadata(&path)?;
    anyhow::ensure!(
        qualified_path(&path)
            && meta.is_file()
            && (meta.uid() == 0 || meta.uid() == unsafe { libc::geteuid() })
            && meta.mode() & 0o022 == 0
            && meta.mode() & 0o111 != 0,
        "native hook interpreter is not qualified"
    );
    Ok(path)
}
pub(super) fn argv(helper: &Path) -> Result<Vec<String>> {
    Ok(vec![helper.to_string_lossy().into_owned(), "hook".into(), "permission-request".into()])
}
pub(super) fn materialize(state: &Path) -> Result<PathBuf> {
    // Pin the complete Rust executable for existing hosted processes across
    // service handover. No interpreter or workspace startup code is involved.
    let bytes = fs::read(interpreter()?)?;
    let root = state.join("attention");
    crate::job_sessions::store::check_directory(state)?;
    crate::job_sessions::store::directory(&root)?;
    let root = root.join("helpers");
    crate::job_sessions::store::directory(&root)?;
    let path = root.join(format!("permission-{}", crate::remote::auth::sha256_hex(&bytes)));
    let opened = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path);
    match opened {
        Ok(mut f) => {
            f.write_all(&bytes)?;
            f.sync_all()?;
            fs::File::open(&root)?.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let mut f = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path)?;
    let meta = f.metadata()?;
    anyhow::ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0
            && meta.nlink() == 1
            && meta.mode() & 0o100 != 0
            && meta.len() == bytes.len() as u64,
        "native hook payload unavailable"
    );
    let mut actual = Vec::new();
    f.read_to_end(&mut actual)?;
    anyhow::ensure!(actual == bytes, "native hook payload changed");
    let path = path.canonicalize()?;
    anyhow::ensure!(qualified_path(&path), "native hook payload path is not qualified");
    Ok(path)
}
pub(super) fn spawn_identity(state: &Path, session: u64, spawned_ms: u64, helper: &Path) -> Result<String> {
    let root = state.join("attention/native-identities");
    crate::job_sessions::store::directory(&root)?;
    let token = util::uuid_v4()?;
    store::publish(
        &root.join(format!("{session}.json")),
        &NativeIdentity { token: token.clone(), spawned_ms, helper: helper.into(), interpreter: helper.into() },
    )?;
    Ok(token)
}
fn descendant(peer: &Identity, provider: &Identity) -> bool {
    if !peer.alive() || !provider.alive() {
        return false;
    }
    let mut pid = peer.pid;
    for _ in 0..16 {
        if pid == provider.pid {
            return pid != peer.pid;
        }
        let Some(p) = procs::stat(pid) else { return false };
        if p.ppid <= 1 || p.ppid == pid {
            return false;
        }
        pid = p.ppid;
    }
    false
}
fn source(endpoint: &Endpoint, provider: &Identity, helper: &Path, interpreter: &Path) -> bool {
    let command = procs::cmdline(endpoint.peer.pid);
    descendant(&endpoint.peer, provider)
        && fs::read_link(format!("/proc/{}/exe", endpoint.peer.pid)).is_ok_and(|p| p == interpreter)
        && interpreter == helper
        && argv(helper).is_ok_and(|expected| command == expected)
}

fn private_roots(service: &Service, session: &Sess) -> Vec<PathBuf> {
    let mut roots = vec![
        service.opts.state_dir.clone(),
        paths::config_dir(),
        service.opts.claude_dir.clone(),
        service.opts.codex_dir.clone(),
        service.opts.antigravity_dir.clone(),
        service.opts.grok_home.clone(),
    ];
    if let Some(root) = service.opts.opencode_db.parent() {
        roots.push(root.to_path_buf());
    }
    if let Some(root) = &session.account_root {
        roots.push(root.clone());
    }
    if let Ok(settings) = crate::providers::accounts::Settings::from_config(&service.opts.config) {
        roots.extend(settings.profiles.values().map(|p| p.directory.clone()));
    }
    roots
}
fn approved_worktree(cwd: &Path, root: &Path, private_roots: &[PathBuf]) -> bool {
    let Some((actual, common)) = crate::scheduler::git_location(cwd) else { return false };
    actual.canonicalize().is_ok_and(|p| p == root)
        && common.join("objects").is_dir()
        && fs::symlink_metadata(common.join("HEAD")).is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= 4096)
        && fs::symlink_metadata(common.join("config")).is_ok_and(|m| m.is_file() && m.len() <= 1048576)
        && !private_roots
            .iter()
            .filter_map(|p| p.canonicalize().ok())
            .any(|private| root.starts_with(&private) || private.starts_with(root))
}
fn native_rules(values: &[Value]) -> Vec<Value> {
    values.iter().filter(|v| v["pattern"]["kind"] == "native_exact").cloned().collect()
}

pub(super) struct Prepared {
    endpoint: Endpoint,
    session: u64,
    spawned_ms: u64,
    conversation: String,
    inputs: u64,
    epoch: u64,
    input: Arc<Mutex<u64>>,
    emu: Arc<Mutex<crate::session::Emu>>,
    output: u64,
    revision: u64,
    authority: Arc<Mutex<u64>>,
    provider: Identity,
    helper: PathBuf,
    interpreter: PathBuf,
    request: String,
    tool_input: Value,
    cwd: PathBuf,
    worktree: PathBuf,
    rules: Vec<Value>,
    private_roots: Vec<PathBuf>,
    deadline: Instant,
    receipt: PathBuf,
    record: Receipt,
    rule: Option<String>,
    claimed: bool,
}
impl Prepared {
    fn prepare(&mut self) -> Result<()> {
        anyhow::ensure!(
            Instant::now() < self.deadline && source(&self.endpoint, &self.provider, &self.helper, &self.interpreter),
            "native source unavailable"
        );
        self.cwd = self.cwd.canonicalize()?;
        self.worktree = self.worktree.canonicalize()?;
        anyhow::ensure!(
            approved_worktree(&self.cwd, &self.worktree, &self.private_roots),
            "native worktree unavailable"
        );
        if let native_policy::Decision::AllowOnce { rule_id } =
            native_policy::evaluate(&self.tool_input, &self.cwd, &self.worktree, &self.rules)
        {
            self.rule = Some(rule_id);
        }
        anyhow::ensure!(self.rule.is_some(), "native request is not eligible");
        anyhow::ensure!(Instant::now() < self.deadline, "native request expired");
        let root = self.receipt.parent().context("native receipt parent missing")?;
        crate::job_sessions::store::directory(root)?;
        anyhow::ensure!(fs::read_dir(root)?.take(10001).count() < 10000, "native receipt capacity exceeded");
        // create_new supplies the cross-connection/process once-only claim.
        // Incomplete or uncertain claims remain a permanent neutral fence.
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.receipt)?;
        file.write_all(&serde_json::to_vec(&self.record)?)?;
        file.sync_all()?;
        fs::File::open(root)?.sync_all()?;
        self.claimed = true;

        Ok(())
    }
    fn respond(mut self, allow: bool) -> bool {
        let proof = allow
            && self.claimed
            && self.rule.is_some()
            && Instant::now() < self.deadline
            && source(&self.endpoint, &self.provider, &self.helper, &self.interpreter)
            && approved_worktree(&self.cwd, &self.worktree, &self.private_roots)
            && matches!(native_policy::evaluate(&self.tool_input, &self.cwd, &self.worktree, &self.rules),
                native_policy::Decision::AllowOnce { rule_id } if Some(&rule_id) == self.rule.as_ref());
        let written = if proof {
            let emu = self.emu.lock().unwrap_or_else(|e| e.into_inner());
            emu.output_revision == self.output
                && authorized_write(
                    &self.endpoint,
                    &self.request,
                    self.deadline,
                    &self.input,
                    self.epoch,
                    &self.authority,
                    self.revision,
                )
        } else {
            self.endpoint.send(&self.request, false, self.deadline);
            false
        };
        if self.claimed {
            self.record.category = if written { "allowed" } else { "neutral_or_unknown" }.into();
            self.record.rule = self.rule.clone();
            let _ = store::publish(&self.receipt, &self.record);
        }
        written
    }
}
impl Service {
    pub(super) fn attention_native_request(&mut self, cid: u64, id: u64, spawned_ms: u64, token: &str, payload: Value) {
        let result = (|| -> Result<Prepared> {
            let client = self.clients.get(&cid).context("native client unavailable")?;
            anyhow::ensure!(
                client.role == ClientRole::Hook && client.version.is_some_and(|v| v >= 16),
                "native protocol unavailable"
            );
            let endpoint = client.native_endpoint.clone().context("native peer unavailable")?;
            anyhow::ensure!(!endpoint.used.swap(true, Ordering::SeqCst), "native invocation already consumed");
            let a = self.attention.as_ref().context("attention unavailable")?;
            anyhow::ensure!(
                a.native_pending.len() < 64 && a.store.supervisor_durable() && a.store.saved.supervisor.enabled,
                "native authority unavailable"
            );
            let identity: NativeIdentity =
                crate::job_sessions::store::read(&a.store.root.join("native-identities").join(format!("{id}.json")))?;
            anyhow::ensure!(token == identity.token && spawned_ms == identity.spawned_ms, "native identity changed");
            let s = self
                .sessions
                .get(&id)
                .filter(|s| s.alive && s.tool == Tool::Claude && s.spawned_ms == spawned_ms)
                .context("native session unavailable")?;
            let conversation = s.conversation.as_ref().context("native conversation unavailable")?;
            anyhow::ensure!(
                payload["hook_event_name"] == "PermissionRequest"
                    && payload["tool_name"] == "Read"
                    && payload["session_id"].as_str() == Some(conversation)
                    && serde_json::to_vec(&payload)?.len() <= 262144,
                "native intent unavailable"
            );
            let request = payload["prompt_id"]
                .as_str()
                .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
                .context("native request unavailable")?
                .to_string();
            anyhow::ensure!(
                !s.owner_draft && !s.recovery.paused && s.recovery.phase.is_none() && !self.automatic_input_pending(id),
                "native input custody unavailable"
            );
            let h = s.hosted.as_ref().context("native process unavailable")?;
            let provider = Identity::read(h.pid.context("native process unavailable")?)?;
            let cwd = s.cwd.clone().unwrap_or_else(|| self.opts.start_dir.clone());
            anyhow::ensure!(payload["cwd"].as_str().is_some_and(|v| Path::new(v) == cwd), "native cwd changed");
            let worktree = s.facts.worktree.as_ref().map(|w| PathBuf::from(&w.path)).unwrap_or_else(|| cwd.clone());
            let key = crate::remote::auth::sha256_hex(&serde_json::to_vec(&(id, spawned_ms, conversation, &request))?);
            let revision = a.store.saved.supervisor.revision;
            Ok(Prepared {
                endpoint,
                session: id,
                spawned_ms,
                conversation: conversation.clone(),
                inputs: s.inputs,
                epoch: h.input_epoch(),
                input: h.input_fence(),
                emu: h.emu.clone(),
                output: h.lock().output_revision,
                revision,
                authority: a.authority.clone(),
                provider,
                helper: identity.helper,
                interpreter: identity.interpreter,
                request,
                tool_input: payload["tool_input"].clone(),
                cwd,
                worktree,
                rules: native_rules(&a.store.saved.rules),
                private_roots: private_roots(self, s),
                deadline: Instant::now() + NATIVE_WINDOW,
                receipt: a.store.root.join("permission-requests").join(format!("{key}.json")),
                record: Receipt {
                    session: id,
                    spawned_ms,
                    revision,
                    at: util::now_secs(),
                    category: "prepared".into(),
                    rule: None,
                },
                rule: None,
                claimed: false,
            })
        })();
        match result {
            Ok(mut prepared) => {
                self.attention.as_mut().unwrap().native_pending.insert(cid);
                let tx = self.tx.clone();
                if thread::Builder::new()
                    .name("native-permission-prepare".into())
                    .spawn(move || {
                        if prepared.prepare().is_err() {
                            prepared.rule = None;
                        }
                        let _ = tx.send(Ev::NativePrepared { client: cid, prepared: Box::new(prepared) });
                    })
                    .is_err()
                {
                    self.attention.as_mut().unwrap().native_pending.remove(&cid);
                }
            }
            Err(_) => {
                // A categorical neutral response only; never echo private input.
                if let (Some(endpoint), Some(request)) = (
                    self.clients
                        .get(&cid)
                        .filter(|c| c.role == ClientRole::Hook && c.version.is_some_and(|v| v >= 16))
                        .and_then(|c| c.native_endpoint.as_ref()),
                    payload["prompt_id"].as_str().filter(|r| r.len() <= 256),
                ) {
                    endpoint.send(request, false, Instant::now() + Duration::from_millis(50));
                }
            }
        }
    }
    pub(super) fn attention_native_prepared(&mut self, cid: u64, prepared: Prepared) {
        let id = prepared.session;
        let current = self.clients.get(&cid).is_some_and(|c| c.role == ClientRole::Hook && c.native_endpoint.is_some())
            && self.attention.as_ref().is_some_and(|a| {
                a.store.supervisor_durable()
                    && a.store.saved.supervisor.enabled
                    && a.store.saved.supervisor.revision == prepared.revision
            })
            && self.sessions.get(&id).is_some_and(|s| {
                s.alive
                    && s.spawned_ms == prepared.spawned_ms
                    && s.conversation.as_ref() == Some(&prepared.conversation)
                    && s.inputs == prepared.inputs
                    && !s.owner_draft
                    && !s.recovery.paused
                    && s.recovery.phase.is_none()
                    && s.hosted
                        .as_ref()
                        .is_some_and(|h| h.pid == Some(prepared.provider.pid) && h.input_epoch() == prepared.epoch)
            })
            && !self.automatic_input_pending(id);
        let rule = prepared.rule.clone();
        let allow = current && rule.is_some() && self.attention_audit(id, "native_prepared", rule.clone()).is_ok();
        let tx = self.tx.clone();
        if thread::Builder::new()
            .name("native-permission-reply".into())
            .spawn(move || {
                let written = prepared.respond(allow);
                let _ = tx.send(Ev::NativeDone { client: cid, session: id, rule, written });
            })
            .is_err()
        {
            if let Some(a) = &mut self.attention {
                a.native_pending.remove(&cid);
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/attention/native.rs"]
mod tests;
