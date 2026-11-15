//! Trusted provider capability ingress; no screen-only permission attestation.
use super::*;
use crate::attention::{self as a, policy};
use std::os::unix::ffi::OsStrExt;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct Identity {
    token: String,
    spawned_ms: u64,
    adapter: PathBuf,
}
struct Resolver;
impl policy::PathResolver for Resolver {
    fn resolve(&self, path: &Path) -> Option<PathBuf> {
        if !path.is_absolute() {
            return None;
        }
        if let Ok(p) = path.canonicalize() {
            return Some(p);
        }
        let mut tail = Vec::new();
        let mut parent = path;
        loop {
            match fs::symlink_metadata(parent) {
                Ok(meta) => {
                    if !meta.is_dir() {
                        return None;
                    }
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tail.push(parent.file_name()?.to_os_string());
                    parent = parent.parent()?;
                }
                Err(_) => return None,
            }
        }
        let mut result = parent.canonicalize().ok()?;
        for part in tail.into_iter().rev() {
            if part == "." || part == ".." {
                return None;
            }
            result.push(part);
        }
        Some(result)
    }
}
fn rules(values: &[Value]) -> Result<Vec<policy::Rule>> {
    let parsed: Result<Vec<_>> = values
        .iter()
        .filter(|v| v["pattern"]["kind"] != "native_exact")
        .map(|v| {
            Ok(policy::Rule {
                id: v["id"].as_str().context("rule id required")?.into(),
                tool: v["tool"].as_str().context("rule tool required")?.into(),
                pattern: match v["pattern"]["kind"].as_str() {
                    Some("exact") => policy::Pattern::Exact(
                        v["pattern"]["command"].as_str().context("exact command required")?.into(),
                    ),
                    Some("argv_glob") => {
                        policy::Pattern::ArgvGlob(serde_json::from_value(v["pattern"]["tokens"].clone())?)
                    }
                    _ => anyhow::bail!("invalid rule pattern"),
                },
            })
        })
        .collect();
    let parsed = parsed?;
    anyhow::ensure!(policy::validate_rules(&parsed), "invalid supervisor rules");
    Ok(parsed)
}
impl Service {
    pub(super) fn attention_validate_rules(&self, values: &[Value]) -> Result<()> {
        anyhow::ensure!(values.len() <= 32, "too many supervisor rules");
        let mut ids = HashSet::new();
        anyhow::ensure!(values.iter().all(|v| v["id"].as_str().is_some_and(|id| ids.insert(id))), "duplicate rule ID");
        let native = values.iter().filter(|v| v["pattern"]["kind"] == "native_exact").cloned().collect::<Vec<_>>();
        anyhow::ensure!(crate::attention::native_policy::validate_rules(&native), "invalid native rules");
        rules(values).map(|_| ())
    }
    pub(super) fn attention_revoke_prepared(&mut self) {
        if let Some(a) = &self.attention {
            let mut generation = a.authority.lock().unwrap_or_else(|e| e.into_inner());
            *generation = a.store.saved.supervisor.revision;
        }
    }
    pub(super) fn attention_spawn_identity(
        &self,
        id: u64,
        tool: Tool,
        spawned_ms: u64,
        argv: &[String],
    ) -> Result<Option<String>> {
        let Some(configured) = self.opts.config.attention.permission_adapters.get(tool.key()) else { return Ok(None) };
        let adapter = configured.canonicalize().context("trusted attention adapter is unavailable")?;
        anyhow::ensure!(adapter.is_absolute() && adapter.is_file(), "invalid trusted attention adapter");
        let direct = argv.first().is_some_and(|a| Path::new(a).canonicalize().is_ok_and(|p| p == adapter));
        let script = argv.get(1).is_some_and(|a| Path::new(a).canonicalize().is_ok_and(|p| p == adapter))
            && argv.first().is_some_and(|a| {
                Path::new(a).file_name().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with("python3"))
            });
        anyhow::ensure!(direct || script, "configured trusted adapter is not the hosted executable");
        let root = self.opts.state_dir.join("attention").join("identities");
        crate::job_sessions::store::directory(&root)?;
        let token = util::uuid_v4()?;
        crate::attention::store::publish(
            &root.join(format!("{id}.json")),
            &Identity { token: token.clone(), spawned_ms, adapter },
        )?;
        Ok(Some(token))
    }
    pub(super) fn attention_audit(&mut self, session: u64, category: &str, rule: Option<String>) -> Result<()> {
        let a = self.attention.as_mut().context("attention unavailable")?;
        a.store.saved.supervisor.audit.push(a::Audit {
            session,
            at: util::now_secs(),
            category: category.into(),
            rule,
        });
        if a.store.saved.supervisor.audit.len() > 256 {
            a.store.saved.supervisor.audit.remove(0);
        }
        a.store.save()
    }
    pub(super) fn attention_permission(&mut self, id: u64, tool: Tool, payload: &Value) -> Result<()> {
        let a = self.attention.as_ref().context("attention unavailable")?;
        anyhow::ensure!(
            a.store.supervisor_durable() && a.store.saved.supervisor.enabled,
            "supervisor disabled or durability pending"
        );
        let identity: Identity =
            crate::job_sessions::store::read(&a.store.root.join("identities").join(format!("{id}.json")))?;
        anyhow::ensure!(payload["token"].as_str() == Some(identity.token.as_str()), "permission source unavailable");
        let s = self
            .sessions
            .get(&id)
            .filter(|s| {
                s.alive
                    && s.tool == tool
                    && s.spawned_ms == identity.spawned_ms
                    && payload["spawned_ms"].as_u64() == Some(s.spawned_ms)
                    && payload["conversation"].as_str() == s.conversation.as_deref()
            })
            .context("permission identity changed")?;
        anyhow::ensure!(
            !s.owner_draft && !s.recovery.paused && s.recovery.phase.is_none() && !self.automatic_input_pending(id),
            "permission input custody unavailable"
        );
        let h = s.hosted.as_ref().context("permission target unavailable")?;
        let pid = h.pid.context("permission process unavailable")?;
        let cmdline = fs::read(format!("/proc/{pid}/cmdline"))?;
        anyhow::ensure!(
            cmdline
                .split(|b| *b == 0)
                .take(2)
                .any(|v| Path::new(std::ffi::OsStr::from_bytes(v)).canonicalize().is_ok_and(|p| p == identity.adapter)),
            "permission source changed"
        );
        let request = payload["request"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
            .context("permission request required")?;
        let command = payload["command"]
            .as_str()
            .filter(|s| s.len() <= 4096)
            .context("complete permission command required")?
            .to_string();
        let permission_tool = payload["permission_tool"].as_str().context("permission tool required")?.to_string();
        let cwd = PathBuf::from(payload["cwd"].as_str().context("permission cwd required")?).canonicalize()?;
        let worktree = s
            .facts
            .worktree
            .as_ref()
            .map(|w| PathBuf::from(&w.path))
            .or_else(|| s.cwd.clone())
            .unwrap_or_else(|| self.opts.start_dir.clone())
            .canonicalize()?;
        anyhow::ensure!(cwd == worktree, "permission cwd does not match hosted worktree");
        let executable = PathBuf::from(payload["executable"].as_str().context("resolved executable proof required")?)
            .canonicalize()?;
        anyhow::ensure!(
            executable.is_file() && payload["resolved_simple_shell"] == true,
            "simple executable resolution is unknown"
        );
        let first = command.split_ascii_whitespace().next().context("empty command")?;
        anyhow::ensure!(
            Path::new(first) == executable || Path::new(first).file_name() == executable.file_name(),
            "executable proof mismatch"
        );
        let prompt = {
            let emu = h.lock();
            crate::prompt::parse(&screen::screen_text(&emu.term)).context("permission prompt unavailable")?
        };
        anyhow::ensure!(
            !prompt.multi
                && !prompt.on_submit
                && prompt.free_text.is_none()
                && prompt.question == payload["question"].as_str().unwrap_or(""),
            "permission prompt changed"
        );
        let selected = prompt.options.get(prompt.selected).context("selected permission unavailable")?;
        anyhow::ensure!(
            selected.label == payload["selected"].as_str().unwrap_or("")
                && selected.label.to_ascii_lowercase().contains("allow once"),
            "single allow-once selection is unknown"
        );
        let complete =
            payload["complete"] == true && payload["single_prompt"] == true && payload["allow_once_selected"] == true;
        let rule_values = a.store.saved.rules.clone();
        let parsed = rules(&rule_values)?;
        let fingerprint = crate::remote::auth::sha256_hex(&serde_json::to_vec(&prompt)?);
        let intent = policy::Intent {
            session: id,
            prompt_identity: &fingerprint,
            tool: &permission_tool,
            command: &command,
            cwd: &cwd,
            worktree: &worktree,
            complete,
            single_prompt: true,
            allow_once_selected: true,
            resolved_simple_shell: true,
        };
        let rule = match policy::evaluate(&intent, &parsed, &Resolver) {
            policy::Decision::AllowOnce { rule_id } => rule_id,
            policy::Decision::Decline(_) => {
                self.attention_audit(id, "declined", None)?;
                return Ok(());
            }
        };
        let epoch = h.input_epoch();
        let output = h.lock().output_revision;
        let authority = a.authority.clone();
        let revision = a.store.saved.supervisor.revision;
        let request_key = crate::remote::auth::sha256_hex(format!("{id}:{}:{request}", identity.spawned_ms).as_bytes());
        let requests = a.store.root.join("permission-requests");
        crate::job_sessions::store::directory(&requests)?;
        let receipt = requests.join(request_key);
        anyhow::ensure!(!receipt.exists(), "permission request already considered; never replay");
        // Categorical durable audit and immutable request fence precede action.
        crate::attention::store::publish(
            &receipt,
            &serde_json::json!({"session":id,"generation":revision,"category":"prepared"}),
        )?;
        self.attention_audit(id, "prepared", Some(rule.clone()))?;
        // Re-evaluate paths after audit, before holding any actor-visible lock.
        let parsed = rules(&rule_values)?;
        anyhow::ensure!(
            matches!(policy::evaluate(&intent, &parsed, &Resolver), policy::Decision::AllowOnce { .. }),
            "permission proofs changed"
        );
        let s = self.sessions.get(&id).context("permission target changed")?;
        let h = s.hosted.as_ref().context("permission target changed")?;
        let tx = self.tx.clone();
        h.write_guarded_prepared(
            b"\r".to_vec(),
            Instant::now() + Duration::from_millis(150),
            epoch,
            authority,
            revision,
            move || {
                let Ok(parsed) = rules(&rule_values) else { return false };
                let intent = policy::Intent {
                    session: id,
                    prompt_identity: &fingerprint,
                    tool: &permission_tool,
                    command: &command,
                    cwd: &cwd,
                    worktree: &worktree,
                    complete,
                    single_prompt: true,
                    allow_once_selected: true,
                    resolved_simple_shell: true,
                };
                matches!(policy::evaluate(&intent, &parsed, &Resolver), policy::Decision::AllowOnce { .. })
            },
            move |emu| {
                emu.output_revision == output
                    && emu.parser.sync_timeout().sync_timeout().is_none()
                    && crate::prompt::parse(&screen::screen_text(&emu.term)).as_ref() == Some(&prompt)
            },
            move |written| {
                let _ = tx.send(Ev::SupervisorDone { session: id, rule, written });
            },
        )?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../tests/attention/supervisor.rs"]
mod tests;
