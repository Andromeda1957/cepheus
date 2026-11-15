//! Explicit repository decisions: immutable recipients and atomic Markdown publication.
use super::*;
use crate::attention as a;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
fn repository(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let path = path.canonicalize()?;
    if let Some((root, common)) = crate::scheduler::git_location(&path) {
        let public =
            if common.file_name().is_some_and(|n| n == ".git") { common.parent().unwrap().to_path_buf() } else { root };
        Ok((common, public))
    } else {
        Ok((path.clone(), path))
    }
}
impl Service {
    pub(super) fn attention_repository_targets(&self, root: &Path) -> Result<Vec<a::store::Target>> {
        let (common, _) = repository(root)?;
        Ok(self
            .sessions
            .values()
            .filter(|s| s.alive && s.tool.is_agent())
            .filter_map(|s| {
                let cwd = s
                    .facts
                    .worktree
                    .as_ref()
                    .map(|w| PathBuf::from(&w.path))
                    .or_else(|| s.cwd.clone())
                    .unwrap_or_else(|| self.opts.start_dir.clone());
                repository(&cwd).is_ok_and(|(id, _)| id == common).then(|| a::store::Target {
                    session: s.id,
                    spawned_ms: s.spawned_ms,
                    conversation: s.conversation.clone(),
                    pid: s.hosted.as_ref().and_then(|h| h.pid),
                })
            })
            .collect())
    }
    pub(super) fn attention_decide(&mut self, args: &Value) -> Result<Value> {
        let key = args["key"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
            .context("decision key required")?;
        let text = args["text"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 15900 && !s.contains('\0'))
            .context("decision text required")?;
        let request = args["repository"].as_str().context("repository path required")?;
        let requested: Option<Vec<u64>> =
            args.get("recipients").map(|v| serde_json::from_value(v.clone())).transpose()?;
        // Retry identity is checked before the present filesystem/live session set.
        let prior = self.attention.as_ref().unwrap().store.decisions.get(key).cloned();
        let decision = if let Some(prior) = prior {
            anyhow::ensure!(
                prior.text == text && prior.request_repository == request && prior.requested == requested,
                "decision key belongs to another payload"
            );
            prior
        } else {
            let store = &self.attention.as_ref().unwrap().store;
            anyhow::ensure!(
                store.decisions.len() < 10000
                    && !store
                        .root
                        .join("retired")
                        .join(format!("decision-{}", crate::remote::auth::sha256_hex(key.as_bytes())))
                        .exists(),
                "decision key expired or receipt capacity reached; inspect repository decision file"
            );
            let (common, root) = repository(Path::new(request))?;
            let peers = self
                .attention_repository_peers(&root)?
                .into_iter()
                .map(|p| {
                    Ok(a::store::DecisionPeer {
                        binding: serde_json::to_value(p)?,
                        targets: None,
                        accepted: Default::default(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let mut targets = Vec::new();
            for s in self.sessions.values().filter(|s| s.alive && s.tool.is_agent()) {
                let cwd = s
                    .facts
                    .worktree
                    .as_ref()
                    .map(|w| PathBuf::from(&w.path))
                    .or_else(|| s.cwd.clone())
                    .unwrap_or_else(|| self.opts.start_dir.clone());
                if repository(&cwd).is_ok_and(|(id, _)| id == common)
                    && requested.as_ref().is_none_or(|ids| ids.contains(&s.id))
                {
                    targets.push(a::store::Target {
                        session: s.id,
                        spawned_ms: s.spawned_ms,
                        conversation: s.conversation.clone(),
                        pid: s.hosted.as_ref().and_then(|h| h.pid),
                    });
                }
            }
            anyhow::ensure!(
                (!targets.is_empty() || !peers.is_empty()) && targets.len() <= 1000,
                "decision has no live repository recipients"
            );
            if let Some(ids) = requested.as_ref().filter(|_| peers.is_empty()) {
                anyhow::ensure!(
                    ids.len() == targets.len() && ids.iter().all(|id| targets.iter().any(|t| t.session == *id)),
                    "decision recipient is outside the repository or unavailable"
                );
            }
            let d = a::store::DecisionRecord {
                key: key.into(),
                repository: root,
                request_repository: request.into(),
                requested: requested.clone(),
                text: text.into(),
                info: a::store::Decision {
                    id: util::uuid_v4()?,
                    created_at: util::now_secs(),
                    recipients: targets.iter().map(|t| t.session).collect(),
                },
                targets,
                completed: false,
                peers,
                durable: true,
            };
            let store = &mut self.attention.as_mut().unwrap().store;
            let path =
                store.root.join("decisions").join(format!("{}.json", crate::remote::auth::sha256_hex(key.as_bytes())));
            // Reconcile publication even if an earlier parent-fsync acknowledgement was lost.
            let accepted: Option<a::store::DecisionRecord> = crate::job_sessions::store::optional(&path)?;
            let mut accepted = accepted.unwrap_or(d);
            anyhow::ensure!(
                accepted.key == key
                    && accepted.text == text
                    && accepted.request_repository == request
                    && accepted.requested == requested,
                "decision identity changed"
            );
            anyhow::ensure!(
                serde_json::to_vec(&accepted)?.len() <= 500000,
                "decision private receipt capacity exceeded"
            );
            let result = crate::attention::store::publish(&path, &accepted);
            accepted.durable = result.is_ok();
            store.decisions.insert(key.into(), accepted.clone());
            result?;
            accepted
        };
        self.attention_complete_decision(&decision)?;
        self.state_changed();
        Ok(serde_json::json!({"decision":self.attention.as_ref().unwrap().store.decisions[key].info}))
    }
    pub(super) fn attention_complete_decision(&mut self, d: &a::store::DecisionRecord) -> Result<()> {
        if d.completed {
            return Ok(());
        }
        let path = self
            .attention
            .as_ref()
            .unwrap()
            .store
            .root
            .join("decisions")
            .join(format!("{}.json", crate::remote::auth::sha256_hex(d.key.as_bytes())));
        let mut current = d.clone();
        if !current.durable {
            crate::attention::store::publish(&path, &current)?;
            current.durable = true;
            self.attention.as_mut().unwrap().store.decisions.insert(d.key.clone(), current.clone());
        }
        if current.peers.iter().any(|p| p.targets.is_none()) {
            self.attention_dispatch_decision_peers(&current)?;
            anyhow::bail!("repository recipient discovery pending; retry the original decision key");
        }
        let mut ids: Vec<_> = current.targets.iter().map(|t| t.session).collect();
        for p in &current.peers {
            let binding: super::federation::AttentionRepositoryPeer = serde_json::from_value(p.binding.clone())?;
            for target in p.targets.as_ref().unwrap() {
                ids.push(binding.global_session(target.session)?);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        anyhow::ensure!(
            !ids.is_empty()
                && ids.len() <= 1000
                && current.requested.as_ref().is_none_or(|wanted| {
                    let mut w = wanted.clone();
                    w.sort_unstable();
                    w == ids
                }),
            "decision recipient unavailable or outside the registered repository"
        );
        if current.info.recipients != ids {
            current.info.recipients = ids;
            crate::attention::store::publish(&path, &current)?;
            self.attention.as_mut().unwrap().store.decisions.insert(current.key.clone(), current.clone());
        }
        self.attention_publish_decision(&current)?;
        self.attention_fanout_decision(&current)?;
        if current
            .peers
            .iter()
            .any(|p| p.targets.as_ref().is_some_and(|ts| ts.iter().any(|t| !p.accepted.contains(&t.session))))
        {
            self.attention_dispatch_decision_peers(&current)?;
            anyhow::bail!("repository decision delivery pending; retry the original decision key");
        }
        current.completed = true;
        crate::attention::store::publish(&path, &current)?;
        self.attention.as_mut().unwrap().store.decisions.insert(d.key.clone(), current);
        Ok(())
    }
    fn attention_dispatch_decision_peers(&mut self, d: &a::store::DecisionRecord) -> Result<()> {
        let now = Instant::now();
        self.attention.as_mut().unwrap().decision_rpc.retain(|_, r| now.duration_since(r.at) < Duration::from_secs(5));
        for (index, p) in d.peers.iter().enumerate() {
            let peer: super::federation::AttentionRepositoryPeer = serde_json::from_value(p.binding.clone())?;
            let tasks: Vec<_> = if let Some(targets) = &p.targets {
                targets.iter().filter(|t| !p.accepted.contains(&t.session)).map(|t| Some(t.clone())).collect()
            } else {
                vec![None]
            };
            for target in tasks.into_iter().take(100) {
                let session = target.as_ref().map(|t| t.session);
                let state = self.attention.as_mut().unwrap();
                if state.decision_rpc.values().any(|r| r.key == d.key && r.peer == index && r.session == session) {
                    continue;
                }
                anyhow::ensure!(state.decision_rpc.len() < 1024, "decision transport capacity reached");
                state.rpc_serial = state
                    .rpc_serial
                    .checked_add(1)
                    .filter(|n| *n < (1u64 << 60))
                    .context("decision request capacity exhausted")?;
                let request = (1u64 << 60) | state.rpc_serial;
                let (action, args) = if let Some(target) = target {
                    (
                        "attention-send",
                        serde_json::json!({"repository":peer.repository,"session":target.session,"target":target,"expected_target":{"spawned_ms":target.spawned_ms,"pid":target.pid,"conversation":target.conversation},"key":format!("decision-{}-{}",d.info.id,peer.global_session(target.session)?),"text":format!("Repository decision:\n{}",d.text)}),
                    )
                } else {
                    ("attention-recipients", serde_json::json!({"repository":peer.repository}))
                };
                self.attention_peer_send(&peer, request, action, args)?;
                self.attention.as_mut().unwrap().decision_rpc.insert(
                    request,
                    super::attention::DecisionRpc { key: d.key.clone(), peer: index, session, at: now },
                );
            }
        }
        Ok(())
    }
    pub(super) fn attention_decision_peer_result(
        &mut self,
        host: &str,
        request: u64,
        data: &Value,
        error: Option<&str>,
    ) -> bool {
        let Some(state) = self.attention.as_mut() else { return false };
        let Some(r) = state.decision_rpc.get(&request).cloned() else { return false };
        let Some(mut d) = state.store.decisions.get(&r.key).cloned() else { return true };
        let Some(p) = d.peers.get_mut(r.peer) else { return true };
        let Ok(peer) = serde_json::from_value::<super::federation::AttentionRepositoryPeer>(p.binding.clone()) else {
            return true;
        };
        if peer.host.name != host || self.hosts.get(&peer.host.tag).is_none_or(|slot| slot.config != peer.host) {
            return false;
        }
        state.decision_rpc.remove(&request);
        if error.is_some() {
            return true;
        }
        if let Some(session) = r.session {
            let Some(target) = p.targets.as_ref().and_then(|ts| ts.iter().find(|t| t.session == session)) else {
                return true;
            };
            if data["message"]["session"].as_u64() != Some(target.session)
                || !data["message"]["id"].as_str().is_some_and(a::store::valid_id)
            {
                return true;
            }
            p.accepted.insert(session);
        } else {
            if data["repository"].as_str() != Some(peer.repository.as_str()) {
                return true;
            }
            let Ok(mut targets) = serde_json::from_value::<Vec<a::store::Target>>(data["targets"].clone()) else {
                return true;
            };
            if targets.len() > 1000 || targets.iter().any(|t| peer.global_session(t.session).is_err()) {
                return true;
            }
            if let Some(wanted) = &d.requested {
                targets.retain(|t| peer.global_session(t.session).is_ok_and(|id| wanted.contains(&id)));
            }
            targets.sort_by_key(|t| t.session);
            if targets.windows(2).any(|t| t[0].session == t[1].session) {
                return true;
            }
            p.targets = Some(targets);
        }
        // A remote discovery response is only a candidate until the complete
        // frozen recipient set and its private receipt remain recoverably bounded.
        if d.targets.len() + d.peers.iter().filter_map(|p| p.targets.as_ref()).map(Vec::len).sum::<usize>() > 1000 {
            return true;
        }
        let path = state
            .store
            .root
            .join("decisions")
            .join(format!("{}.json", crate::remote::auth::sha256_hex(d.key.as_bytes())));
        if crate::attention::store::publish(&path, &d).is_ok() {
            state.store.decisions.insert(d.key.clone(), d);
        }
        self.state_changed();
        true
    }
    pub(super) fn attention_publish_decision(&self, d: &a::store::DecisionRecord) -> Result<()> {
        let dir = d.repository.join(".cepheus");
        crate::job_sessions::store::directory(&dir)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(dir.join(".decisions.lock"))?;
        let meta = lock.metadata()?;
        anyhow::ensure!(
            meta.is_file() && meta.nlink() == 1 && meta.uid() == unsafe { libc::geteuid() },
            "invalid decision lock"
        );
        use std::os::fd::AsRawFd;
        anyhow::ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "repository decision publication busy; retry same key"
        );
        let path = dir.join("decisions.md");
        let mut prior = String::new();
        match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(mut f) => {
                let m = f.metadata()?;
                anyhow::ensure!(
                    m.is_file() && m.nlink() == 1 && m.uid() == unsafe { libc::geteuid() } && m.len() < 4 * 1024 * 1024,
                    "invalid or full decisions file"
                );
                f.read_to_string(&mut prior)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let end = format!("<!-- cepheus-decision:{}:complete -->", d.info.id);
        if prior.lines().any(|line| line == end) {
            fs::File::open(&dir)?.sync_all()?;
            return Ok(());
        }
        let next = format!("{prior}\n## Decision {}\n\n{}\n{end}\n", d.info.created_at, d.text);
        anyhow::ensure!(next.len() <= 4 * 1024 * 1024, "decisions file full");
        let temporary = dir.join(format!(".decision-{}", util::uuid_v4()?));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        let result = (|| -> Result<()> {
            f.write_all(next.as_bytes())?;
            f.sync_all()?;
            fs::rename(&temporary, &path)?;
            fs::File::open(&dir)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
    pub(super) fn attention_fanout_decision(&mut self, d: &a::store::DecisionRecord) -> Result<()> {
        for target in &d.targets {
            let key = format!("decision-{}-{}", d.info.id, target.session);
            self.attention.as_mut().unwrap().store.add(
                &key,
                &format!("Repository decision:\n{}", d.text),
                target.clone(),
                d.info.created_at,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../tests/attention/decisions.rs"]
mod tests;
