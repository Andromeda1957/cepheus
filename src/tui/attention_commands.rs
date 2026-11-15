//! Literal desktop commands for core-owned attention actions.
use super::*;
use serde_json::{json, Value};

pub fn help_lines() -> Vec<String> {
    [":send --idle [N] [TEXT] — capture a session and retain a private draft until acknowledged; no immediate-send fallback",
     ":attention recover / :attention retry KEY — privately inspect retained operations, then explicitly reconcile their immutable original requests",
     ":decide TEXT — record a decision for the selected session's repository and queue delivery when safe and idle",
     ":supervisor [status|on|off|off --fleet|rules [JSON]] — explicit opt-in, private exact allowlist; off --fleet shows pending hosts",
     "Messages: Enter privately fetches body, c cancels only queued/held. No attention action answers a question or acknowledges Finished."].into_iter().map(str::to_owned).collect()
}

impl App {
    pub(super) fn attention_command(&mut self, word: &str, rest: &str) -> Result<(), String> {
        if !self.attention_gate() {
            return Ok(());
        }
        match word {
            "attention" if rest == "recover" => {
                self.restore_attention_store().map_err(|_| {
                    "Private attention recovery could not be read; existing state preserved".to_string()
                })?;
                let mut lines=vec!["Private recovered operations; no automatic replay. :attention retry KEY reconciles the original immutable request.".into()];
                for (key, draft) in
                    self.attention.drafts.iter().filter(|(_, d)| d.namespace == self.attention.namespace && !d.accepted)
                {
                    lines.push(format!(
                        "Idle {key} · captured session {} · attempted {}",
                        draft.session, draft.attempted
                    ));
                    lines.push(util::one_line(&draft.text));
                }
                for (key, record) in
                    self.attention.decisions.iter().filter(|(_, d)| d.namespace == self.attention.namespace && !d.known)
                {
                    lines.push(format!(
                        "Decision {key} · {}",
                        util::one_line(record.args["repository"].as_str().unwrap_or("unknown repository"))
                    ));
                    lines.push(util::one_line(record.args["text"].as_str().unwrap_or("")));
                }
                self.dialog = Some(Dialog::Report {
                    title: "Private attention recovery".into(),
                    lines,
                    scroll: 0,
                    confirm: None,
                    waiting: 0,
                });
            }
            "attention" if rest.starts_with("retry ") => {
                let key = rest[6..].trim();
                if self
                    .attention
                    .drafts
                    .get(key)
                    .is_some_and(|d| d.namespace == self.attention.namespace && !d.accepted)
                {
                    self.attention.active_draft = Some(key.into());
                    self.dialog = Some(Dialog::AttentionSend);
                    self.submit_idle_draft();
                } else if let Some(record) =
                    self.attention.decisions.get(key).filter(|d| d.namespace == self.attention.namespace && !d.known)
                {
                    let args = record.args.clone();
                    if record.local_only {
                        // This candidate never reached transport. Its first retry must acquire durable custody.
                        self.attention.decisions.get_mut(key).unwrap().local_only = false;
                        if !self.attention_request("attention-decide", args) {
                            self.attention.decisions.get_mut(key).unwrap().local_only = true;
                        }
                    } else {
                        self.attention_reconcile("attention-decide", args);
                    }
                } else {
                    return Err("No unresolved operation with this key at the current coordinator".into());
                }
            }
            "attention" if rest == "help" => {
                self.dialog = Some(Dialog::Report {
                    title: "Attention commands".into(),
                    lines: help_lines(),
                    scroll: 0,
                    confirm: None,
                    waiting: 0,
                })
            }
            "decide" => {
                if rest.is_empty() || rest.len() > 15900 {
                    return Err("give decision text, at most 15900 UTF-8 bytes".into());
                }
                let session = self.current_session().ok_or("select an agent in the repository first")?;
                let repository = session
                    .worktree
                    .as_ref()
                    .map(|w| w.path.as_str())
                    .or(session.cwd.as_deref())
                    .ok_or("selected session has no repository path")?;
                // Core resolves this path to the repository identity on its owning host.
                let host = session.host.clone().unwrap_or_else(|| "@here".into());
                let repository = repository.to_owned();
                let matches_repo = |d: &&attention::Decision| {
                    d.namespace == self.attention.namespace
                        && !d.known
                        && !d.local_only
                        && d.args["repository"] == repository
                        && d.args.get("host").and_then(Value::as_str).unwrap_or("@here") == host
                };
                let unresolved = self
                    .attention
                    .decisions
                    .values()
                    .filter(matches_repo)
                    .find(|d| d.args["text"] == rest)
                    .or_else(|| self.attention.decisions.values().find(matches_repo));
                let fresh = unresolved.is_none();
                let args = if let Some(record) = unresolved {
                    if record.args["text"] != rest {
                        return Err("An unresolved decision is retained for this repository; retry its exact original text before creating another".into());
                    }
                    record.args.clone()
                } else {
                    if self.attention.decisions.len() >= 64 {
                        self.attention.decisions.retain(|_, d| !d.known);
                    }
                    if self.attention.decisions.len() >= 64 {
                        return Err("Unresolved-decision capacity reached".into());
                    }
                    let key = attention::operation_key();
                    let args = json!({"key":key,"repository":repository,"text":rest,"host":host});
                    self.attention.decisions.insert(
                        key,
                        attention::Decision {
                            args: args.clone(),
                            namespace: self.attention.namespace,
                            known: false,
                            local_only: false,
                        },
                    );
                    args
                };
                let key = args["key"].as_str().unwrap().to_owned();
                let sent = if fresh {
                    self.attention_request("attention-decide", args)
                } else {
                    self.attention_reconcile("attention-decide", args)
                };
                if !sent && fresh {
                    if let Some(record) = self.attention.decisions.get_mut(&key) {
                        record.local_only = true;
                    }
                }
            }
            "supervisor" => match rest {
                "" | "status" => {
                    self.open_attention(attention::Tab::Supervisor);
                    self.attention_request("attention-supervisor-status", json!({}));
                }
                "on" | "off" => {
                    self.attention_request("attention-supervisor-set", json!({"enabled":rest=="on"}));
                }
                "off --fleet" => {
                    self.attention_request("attention-supervisor-off", json!({"fleet":true}));
                }
                "rules" => {
                    self.open_attention(attention::Tab::Supervisor);
                    self.attention_request("attention-supervisor-rules", json!({}));
                }
                _ if rest.starts_with("rules ") => {
                    let body = rest[6..].trim();
                    if body.len() > 65536 {
                        return Err("rules exceed 64 KiB".into());
                    }
                    let rules: Value =
                        serde_json::from_str(body).map_err(|_| "rules require a JSON array".to_string())?;
                    if !rules.is_array() || rules.as_array().is_some_and(|r| r.len() > 32) {
                        return Err("rules require at most 32 exact allowlist records".into());
                    }
                    self.attention_request("attention-supervisor-rules", json!({"rules":rules}));
                }
                _ => return Err("use status, on, off, off --fleet, or rules [JSON]".into()),
            },
            _ => return Err("use :attention recover, :attention retry KEY, :decide TEXT or :supervisor status".into()),
        }
        Ok(())
    }
    pub(super) fn idle_send_command(&mut self, rest: &str) -> Result<(), String> {
        if !self.attention_gate() {
            return Ok(());
        }
        let (target, text) = match rest.split_once(char::is_whitespace) {
            Some((n, t)) => (Some(n), t.trim()),
            None if rest.is_empty() => (None, ""),
            None => (Some(rest), ""),
        };
        let session = match target {
            None => self.current_session().map(|s| s.id).ok_or("select a session first")?,
            Some(n) => n
                .trim_start_matches('#')
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .and_then(|n| self.numbered_sessions().get(n - 1).map(|s| s.id))
                .ok_or("give a valid session number")?,
        };
        if !self.session(session).is_some_and(|s| s.alive && s.tool.is_agent()) {
            return Err("idle messages require a live hosted agent".into());
        }
        if text.len() > 16000 {
            return Err("message exceeds 16000 UTF-8 bytes".into());
        }
        if let Some(key) = self
            .attention
            .draft_for
            .get(&session)
            .filter(|key| {
                self.attention.drafts.get(*key).is_some_and(|d| !d.accepted && d.namespace == self.attention.namespace)
            })
            .cloned()
        {
            let draft = &self.attention.drafts[&key];
            if draft.attempted && !text.is_empty() && draft.text != text {
                return Err("An unresolved idle send is retained for this session; retry the unchanged draft before creating another".into());
            }
            if !text.is_empty() && !draft.attempted {
                self.attention.drafts.get_mut(&key).unwrap().text = text.into();
            }
            self.attention.active_draft = Some(key);
            self.dialog = Some(Dialog::AttentionSend);
            if !text.is_empty() {
                self.submit_idle_draft();
            }
            return Ok(());
        }
        if self.attention.drafts.len() >= 64 {
            self.attention.drafts.retain(|_, d| !d.accepted);
        }
        if self.attention.drafts.len() >= 64 {
            return Err("64 retained drafts; resolve existing drafts before adding another".into());
        }
        let key = attention::operation_key();
        self.attention.drafts.insert(
            key.clone(),
            attention::Draft {
                session,
                namespace: self.attention.namespace,
                conversation: self.session(session).and_then(|s| s.conversation.clone()),
                pid: self.session(session).and_then(|s| s.pid),
                spawned_ms: self.session(session).map_or(0, |s| s.spawned_ms),
                host: self.session(session).and_then(|s| s.host.clone()),
                text: text.into(),
                request: None,
                error: None,
                accepted: false,
                attempted: false,
                local_only: false,
            },
        );
        self.attention.draft_for.insert(session, key.clone());
        self.attention.active_draft = Some(key);
        self.dialog = Some(Dialog::AttentionSend);
        if !text.is_empty() {
            self.submit_idle_draft();
        }
        Ok(())
    }
    pub(super) fn submit_idle_draft(&mut self) {
        let Some(key) = self.attention.active_draft.clone() else { return };
        let Some(draft) = self.attention.drafts.get(&key) else { return };
        if draft.namespace != self.attention.namespace {
            self.set_notice("This draft belongs to a previous coordinator; it cannot be submitted here.", true);
            return;
        }
        if draft.request.is_some() || draft.accepted || draft.text.trim().is_empty() {
            return;
        }
        if self.service_version < 17 {
            self.set_notice("Identity-safe idle messages need protocol 17; update needed on the selected service/host. Draft retained.", true);
            return;
        }
        if draft.spawned_ms == 0 || draft.pid.is_none() {
            self.set_notice("Captured idle-message identity is unknown (legacy draft or missing generation/PID); draft retained without submission.", true);
            return;
        }
        let was_attempted = draft.attempted;
        let (session, text, conversation, pid, host, spawned_ms) = (
            draft.session,
            draft.text.clone(),
            draft.conversation.clone(),
            draft.pid,
            draft.host.clone(),
            draft.spawned_ms,
        );
        if crate::protocol::split_id(session).0 != 0 && host.is_none() {
            self.set_notice("Remote draft has no retained origin; submission held without guessing a host.", true);
            return;
        }
        if !was_attempted
            && !self.session(session).is_some_and(|s| {
                s.alive
                    && s.tool.is_agent()
                    && s.conversation == conversation
                    && s.host == host
                    && s.spawned_ms == spawned_ms
                    && s.pid == pid
            })
        {
            self.set_notice(
                "Captured idle-message target changed or is unavailable; unsubmitted draft retained.",
                true,
            );
            return;
        }
        if let Some(draft) = self.attention.drafts.get_mut(&key) {
            draft.attempted = true;
            draft.local_only = false;
        }
        let args = json!({"key":key,"session":session,"text":text,"host":host.as_deref().unwrap_or("@here"),
            "expected_target":{"spawned_ms":spawned_ms,"pid":pid,"conversation":conversation}});
        let sent = if was_attempted {
            self.attention_reconcile("attention-send", args)
        } else {
            self.attention_request("attention-send", args)
        };
        if sent {
            let request = self.attention.last_request;
            if let Some(draft) = self.attention.drafts.get_mut(&key) {
                draft.request = request;
                draft.attempted = true;
                draft.error = None;
            }
        } else if !was_attempted {
            if let Some(draft) = self.attention.drafts.get_mut(&key) {
                draft.attempted = false;
                draft.local_only = true;
                draft.error=Some("Not submitted: local recovery admission held this candidate. :attention recover loads the owning operation.".into());
            }
        }
    }
    fn editable_idle_key(&mut self) -> Option<String> {
        let key = self.attention.active_draft.clone()?;
        let draft = self.attention.drafts.get(&key)?;
        if draft.request.is_some() || draft.accepted {
            return None;
        }
        if draft.attempted {
            self.set_notice("This send has an unresolved receipt; its text is immutable until reconciliation.", true);
            return None;
        }
        Some(key)
    }
    pub(super) fn paste_idle_draft(&mut self, text: &str) {
        if text.is_empty() || self.editable_idle_key().is_none() {
            return;
        }
        if let Some(draft) = self
            .attention
            .active_draft
            .as_ref()
            .and_then(|k| self.attention.drafts.get_mut(k))
            .filter(|d| d.request.is_none() && !d.accepted)
        {
            let text = util::one_line(text);
            for ch in text.chars() {
                if draft.text.len() + ch.len_utf8() > 16000 {
                    break;
                }
                draft.text.push(ch);
            }
        }
    }
    pub(super) fn idle_draft_key(&mut self, k: KeyEvent) -> Option<Dialog> {
        if k.code == KeyCode::Esc {
            self.persist_attention();
            return None;
        }
        if k.code == KeyCode::Enter {
            self.submit_idle_draft();
            return Some(Dialog::AttentionSend);
        }
        if !matches!(k.code, KeyCode::Backspace | KeyCode::Char(_))
            || k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return Some(Dialog::AttentionSend);
        }
        let Some(key) = self.editable_idle_key() else { return Some(Dialog::AttentionSend) };
        let Some(draft) = self.attention.drafts.get_mut(&key) else { return None };
        if draft.request.is_none() && !draft.accepted {
            match k.code {
                KeyCode::Backspace => {
                    draft.text.pop();
                }
                KeyCode::Char(c)
                    if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                        && draft.text.len() + c.len_utf8() <= 16000 =>
                {
                    draft.text.push(c)
                }
                _ => {}
            }
        }
        Some(Dialog::AttentionSend)
    }
}
