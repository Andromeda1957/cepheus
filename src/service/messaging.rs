//! Ordered message submission and acknowledgements. Every message, from the
//! owner or from an automatic queue, reaches the agent through its provider's
//! own message interface. Delivery never reads the terminal or types into it,
//! so a draft in the session's composer stays exactly as the owner left it.
use super::*;
use crate::protocol::Delivered;
use crate::providers::messages::{Delivery, Request};

#[derive(Default)]
pub(super) struct State {
    next: u64,
    batches: HashMap<u64, Batch>,
    queues: HashMap<u64, VecDeque<Job>>,
    peers: HashMap<u64, Vec<PeerSend>>,
    /// Claude's owner input listeners and the messages handed to them.
    pub(super) inputs: super::claude_input::Inputs,
    /// The memory rerun whose message `rerun_message` is starting, and its
    /// outcome if the delivery settled at once.
    rerun_now: Option<(u64, Option<Result<(), String>>)>,
}
struct PeerSend {
    conversation: Option<String>,
    spawned_ms: u64,
    cancel: std::sync::Weak<AtomicBool>,
}
type RecipientResult = (u64, String, Option<Result<Option<Delivered>, String>>);
#[derive(Clone)]
enum OwnerNative {
    Queue(Request),
    /// Codex images the owner uploaded, each with the digest the gateway
    /// took when it was sent, verified again just before submission.
    Images {
        request: Request,
        pid: u32,
        images: Vec<(std::path::PathBuf, String)>,
        state: std::path::PathBuf,
    },
    /// Claude's own prompt interface, through the listener its
    /// `cepheus-owner-input` plugin runs (`claude_input.rs`). Files the owner
    /// uploaded, each with the digest the gateway took, are checked just
    /// before submission and named in the text for Claude to open.
    Claude {
        files: Vec<(std::path::PathBuf, String)>,
        state: std::path::PathBuf,
        client_id: Option<String>,
        /// A broadcast, delivered into the running turn rather than after it.
        /// Other owner messages stay prompts of their own, which the phone's
        /// chat matches to its copy.
        steer: bool,
    },
    Dot(crate::dot::OwnerRequest),
}
struct Batch {
    request: Option<u64>,
    broadcast: Option<(u64, bool)>,
    client: u64,
    results: Vec<RecipientResult>,
    idle: Option<super::work_idle::Receipt>,
    /// The memory rerun this message delivers.
    rerun: Option<u64>,
}
#[derive(Clone)]
struct Job {
    batch: u64,
    id: u64,
    text: String,
    conversation: Option<String>,
    spawned_ms: u64,
    pid: Option<u32>,
    tool: Tool,
    process: Option<Arc<Mutex<crate::session::Emu>>>,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    native: OwnerNative,
    running_only: bool,
    inputs: u64,
}

impl Job {
    fn targets(&self, session: &Sess) -> bool {
        session.spawned_ms == self.spawned_ms
            && session.conversation == self.conversation
            && session.tool == self.tool
            && session.hosted.as_ref().and_then(|hosted| hosted.pid) == self.pid
            && match (&self.process, session.hosted.as_ref()) {
                (Some(process), Some(hosted)) => Arc::ptr_eq(process, &hosted.emu),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Service {
    pub(super) fn broadcast_message(&mut self, cid: u64, ids: &[u64], text: &str) {
        self.begin_messages(cid, ids, text, false, None, None);
    }
    pub(super) fn send_message(&mut self, cid: u64, id: u64, request: u64, text: &str, attachments: bool) {
        if attachments {
            let error = Some("native attachment delivery is not available; nothing was sent".into());
            self.send(cid, &ServerMsg::MessageSent { id, request, error, delivered: None });
            return;
        }
        self.begin_messages(cid, &[id], text, false, Some(request), None);
    }
    pub(super) fn begin_messages(
        &mut self,
        cid: u64,
        ids: &[u64],
        text: &str,
        running_only: bool,
        request: Option<u64>,
        idle: Option<super::work_idle::Receipt>,
    ) {
        self.begin_messages_inner(cid, ids, text, running_only, request, idle, None, &[], None, None);
    }

    /// Deliver memory rerun `rerun`'s message to `session`: the outcome when
    /// the delivery settled at once, and none while the provider has not
    /// answered yet; that answer settles it through `rerun_settled`.
    pub(super) fn rerun_message(&mut self, rerun: u64, session: u64, text: &str) -> Option<Result<(), String>> {
        self.messages.rerun_now = Some((rerun, None));
        self.begin_messages_inner(0, &[session], text, false, None, None, None, &[], Some(rerun), None);
        let settled = self.messages.rerun_now.take().and_then(|(_, outcome)| outcome);
        let in_flight = self.messages.batches.values().any(|b| b.rerun == Some(rerun));
        settled.or_else(|| (!in_flight).then(|| Err("nothing was sent".into())))
    }

    /// An owner message carrying uploaded files. Codex takes images through
    /// its own interface and Claude takes any file by its checked path; any
    /// other provider refuses them.
    pub(super) fn send_owner_images(
        &mut self,
        cid: u64,
        id: u64,
        request: u64,
        text: &str,
        images: &[(std::path::PathBuf, String)],
        client_id: Option<&str>,
    ) {
        self.begin_messages_inner(cid, &[id], text, false, Some(request), None, None, images, None, client_id);
    }

    pub(super) fn begin_broadcast_part(
        &mut self,
        cid: u64,
        request: u64,
        selected: Option<&[u64]>,
        text: &str,
        local: bool,
    ) {
        let current;
        let ids = match selected {
            Some(ids) => ids,
            None => {
                current = self
                    .build_state()
                    .sessions
                    .iter()
                    .filter(|s| crate::protocol::split_id(s.id).0 == 0 && s.takes_broadcast())
                    .map(|s| s.id)
                    .collect::<Vec<_>>();
                &current
            }
        };
        let wrapped = Self::broadcast_text(text);
        let invalid = text.trim().is_empty()
            || wrapped.len() > crate::remote::wire::SEND_MAX
            || text.contains('\0')
            || ids.len() > 1024
            || ids.iter().any(|id| crate::protocol::split_id(*id).0 != 0);
        if invalid || ids.is_empty() {
            let failed = if invalid {
                ids.iter().take(1024).map(|&id| (id, "invalid broadcast request; nothing sent".into())).collect()
            } else {
                vec![]
            };
            if local {
                self.broadcast_part_done(cid, request, 0, vec![], failed);
            } else {
                self.send(cid, &ServerMsg::BroadcastDone { request, sent: vec![], failed });
            }
            return;
        }
        self.begin_messages_inner(cid, ids, &wrapped, true, None, None, Some((request, local)), &[], None, None);
    }

    pub(super) fn broadcast_text(text: &str) -> String {
        format!(
            "[Owner broadcast to all running agents]\n\
This is a shared instruction, not a new task. Apply it only to your own current work. \
If it concerns behavior you are doing, stop or correct that behavior yourself. \
Otherwise continue your assigned task without replying to it. Do not acknowledge it, \
investigate which other agent it concerns, inspect other sessions, or coordinate a response \
unless this broadcast explicitly asks you to.\n\n\
Owner message:\n{text}"
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_messages_inner(
        &mut self,
        cid: u64,
        ids: &[u64],
        text: &str,
        running_only: bool,
        request: Option<u64>,
        idle: Option<super::work_idle::Receipt>,
        broadcast: Option<(u64, bool)>,
        images: &[(std::path::PathBuf, String)],
        rerun: Option<u64>,
        client_id: Option<&str>,
    ) {
        if text.trim().is_empty() && images.is_empty() {
            if let Some(request) = request {
                self.send(
                    cid,
                    &ServerMsg::MessageSent {
                        id: ids[0],
                        request,
                        error: Some("Type a message first.".into()),
                        delivered: None,
                    },
                );
            } else {
                self.notice(cid, "Type a message first.", true);
            }
            return;
        }
        let steer = idle.is_none() && rerun.is_none();
        let mut seen = std::collections::HashSet::new();
        let ids: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|id| {
                seen.insert(*id) && (request.is_some() || broadcast.is_some() || self.sessions.contains_key(id))
            })
            .collect();
        self.messages.next += 1;
        let batch = self.messages.next;
        let results = ids.iter().map(|&id| (id, self.label_of(id), None)).collect();
        self.messages.batches.insert(batch, Batch { client: cid, request, broadcast, results, idle, rerun });
        if ids.is_empty() {
            self.messages.batches.remove(&batch);
            self.notice(cid, "Sent to no session", true);
            return;
        }
        for id in ids {
            let error = if self.pending_directories.contains_key(&id) {
                Some("changing directory; wait for it to restart before sending".into())
            } else if text.len() > crate::remote::wire::SEND_MAX || text.contains('\0') {
                Some("message is too long or contains a NUL".into())
            } else if self.messages.queues.values().map(VecDeque::len).sum::<usize>() >= 128 {
                Some("message queue is full; try again after pending sends finish".into())
            } else if self.messages.queues.get(&id).is_some_and(|q| q.len() >= 8) {
                Some("too many pending messages for this session".into())
            } else {
                None
            };
            if let Some(error) = error {
                self.message_finished(batch, id, Err(error));
                continue;
            }
            let Some(s) = self.sessions.get_mut(&id) else {
                self.message_finished(batch, id, Err("not running".into()));
                continue;
            };
            let cwd = s.cwd.as_deref().unwrap_or(&self.opts.start_dir);
            let prepared = (|| -> Result<OwnerNative> {
                let pid = s
                    .hosted
                    .as_ref()
                    .and_then(|h| h.pid)
                    .context("native process identity is not ready; nothing was sent")?;
                anyhow::ensure!(
                    images.is_empty() || matches!(s.tool, Tool::Codex | Tool::Claude),
                    "native image delivery is unavailable for this provider; nothing was sent"
                );
                if s.tool == Tool::Dynamic("dot") {
                    let provider = self
                        .opts
                        .config
                        .providers
                        .iter()
                        .find(|p| p.id == "dot")
                        .context("the Dot native provider is unavailable; nothing was sent")?;
                    return crate::dot::OwnerRequest::prepare(provider, pid, text)
                        .map(OwnerNative::Dot)
                        .map_err(anyhow::Error::msg);
                }
                let conversation =
                    s.conversation.as_deref().context("native conversation identity is not ready; nothing was sent")?;
                let first = text.split_whitespace().next().unwrap_or("");
                anyhow::ensure!(
                    first.strip_prefix('/').is_none_or(|tail| tail.contains('/')),
                    "this command requires a native provider control; nothing was sent"
                );
                if s.tool == Tool::Claude {
                    return Ok(OwnerNative::Claude {
                        files: images.to_vec(),
                        state: self.opts.state_dir.clone(),
                        client_id: client_id.map(str::to_owned),
                        steer: steer && broadcast.is_some(),
                    });
                }
                let root = s.account_root.as_deref().unwrap_or(&self.opts.codex_dir);
                let request =
                    crate::providers::messages::request(s.tool, &self.opts.config, conversation, text, cwd, root)
                        .with_context(|| {
                            format!("{} has no supported direct message interface; nothing was sent", s.tool.label())
                        })?
                        .for_owner(pid)
                        .with_steering(steer)
                        .with_client_id(client_id);
                anyhow::ensure!(
                    request.matches_runtime(pid),
                    "native runtime identity could not be verified; nothing was sent"
                );
                if !images.is_empty() {
                    let (images, state) = (images.to_vec(), self.opts.state_dir.clone());
                    return Ok(OwnerNative::Images { request, pid, images, state });
                }
                Ok(OwnerNative::Queue(request))
            })();
            let native = match prepared {
                Ok(native) => native,
                Err(error) => {
                    self.message_finished(batch, id, Err(error.to_string()));
                    continue;
                }
            };
            let job = Job {
                batch,
                id,
                text: text.into(),
                conversation: s.conversation.clone(),
                spawned_ms: s.spawned_ms,
                pid: s.hosted.as_ref().and_then(|hosted| hosted.pid),
                tool: s.tool,
                process: s.hosted.as_ref().map(|hosted| hosted.emu.clone()),
                deadline: Instant::now() + Duration::from_secs(35),
                cancel: s.chat.control_cancel(),
                native,
                running_only,
                inputs: s.inputs,
            };
            self.messages.peers.entry(id).or_default().push(PeerSend {
                conversation: job.conversation.clone(),
                spawned_ms: job.spawned_ms,
                cancel: Arc::downgrade(&job.cancel),
            });
            let queue = self.messages.queues.entry(id).or_default();
            queue.push_back(job);
            if queue.len() == 1 {
                self.start_message(id);
            }
        }
    }

    fn start_message(&mut self, id: u64) {
        let Some(job) = self.messages.queues.get(&id).and_then(|q| q.front()).cloned() else {
            return;
        };
        let idle = self.messages.batches.get(&job.batch).is_some_and(|b| b.idle.is_some());
        let Some(s) = self.sessions.get_mut(&id) else {
            self.message_finished(job.batch, id, Err("not running".into()));
            return;
        };
        let error = if job.cancel.load(Ordering::Relaxed) {
            Some("delivery canceled; nothing further was sent")
        } else if Instant::now() >= job.deadline {
            Some("delivery expired before submission; nothing was sent")
        } else if self.shutdown_at.is_some() || s.closing.is_some() {
            Some("session is closing; queued message was not sent")
        } else if !s.alive || s.hosted.is_none() {
            Some("not running")
        } else if !job.targets(s) {
            Some("the session changed before delivery")
        } else if idle && (s.inputs != job.inputs || !s.native_idle(util::now_ms())) {
            Some("the session is no longer idle; nothing was sent")
        } else if s.chat.asked.is_some() {
            Some("the chat is being cleared; wait for the new conversation before sending")
        } else if self.pending_directories.contains_key(&id) {
            Some("changing directory; queued message was not sent")
        } else if job.running_only
            && !(s.tracker.status == Status::Working
                || s.tracker.status == Status::NeedsYou && s.detail() == crate::protocol::QUEUED_QUESTION)
        {
            Some("no longer working; queued broadcast was not sent")
        } else if !crate::providers::takes_broadcast(s.tool) {
            Some("broadcast disabled")
        } else {
            None
        };
        if let Some(error) = error {
            self.message_finished(job.batch, id, Err(error.into()));
            return;
        }
        // This is a backend operation. The terminal's mode, visible question,
        // draft and recovery composer neither authorize nor receive it.
        s.input_changed();
        self.messages.queues.get_mut(&id).unwrap().front_mut().unwrap().inputs = s.inputs;
        let label = s.tool.label();
        if let OwnerNative::Claude { files, state, client_id, steer } = &job.native {
            if files.is_empty() {
                let conversation = job.conversation.as_deref().unwrap_or_default();
                let sent = self.owner_input_send(
                    id,
                    job.batch,
                    conversation,
                    &job.text,
                    &[],
                    *steer,
                    client_id.as_deref(),
                    job.deadline,
                );
                if let Err(error) = sent {
                    self.message_finished(job.batch, id, Err(error));
                }
                return;
            }
            // Reading every file can take a while: check them off the loop.
            let (files, state, tx, batch) = (files.clone(), state.clone(), self.tx.clone(), job.batch);
            let (deadline, cancel) = (job.deadline, job.cancel.clone());
            let checking = thread::Builder::new().name(format!("message-files-{id}")).spawn(move || {
                let result = super::extensions::native::provider_target::owner_files(&state, &files, deadline, &cancel)
                    .map_err(|error| match format!("{error:#}") {
                        error if error.ends_with("nothing was sent") => error,
                        error => format!("the uploaded file was refused ({error}); nothing was sent"),
                    });
                let _ = tx.send(Ev::ClaudeFilesChecked { id, batch, result });
            });
            if checking.is_err() {
                self.message_finished(batch, id, Err("could not start native delivery".into()));
            }
            return;
        }
        let tx = self.tx.clone();
        let batch = job.batch;
        if thread::Builder::new()
            .name(format!("message-{id}"))
            .spawn(move || {
                let result = match job.native {
                    OwnerNative::Queue(request) => match request.deliver(job.deadline, &job.cancel) {
                        Delivery::Unsupported => Delivery::Failed(format!(
                            "{label} native message interface is unavailable; nothing was sent"
                        )),
                        result => result,
                    },
                    OwnerNative::Images { request, pid, images, state } => {
                        let control = |paths| {
                            let images = crate::providers::messages::controls::Control::OwnerInput {
                                text: job.text.clone(),
                                paths,
                            };
                            request.control_until(pid, images, job.deadline, &job.cancel)
                        };
                        let receipt = super::extensions::native::provider_target::owner_images(
                            &state,
                            &images,
                            job.deadline,
                            &job.cancel,
                        )
                        .context("the uploaded image was refused; nothing was sent")
                        .and_then(control);
                        match receipt {
                            Ok(value) if value["accepted"] == true => {
                                match serde_json::from_value::<Delivered>(value["delivered"].clone()) {
                                    Ok(delivered) => Delivery::Accepted(delivered),
                                    Err(_) => Delivery::Failed(
                                        "image submission has no delivery receipt; check before retrying".into(),
                                    ),
                                }
                            }
                            Ok(_) => Delivery::Failed(format!(
                                "{label} native image interface is unavailable; nothing was sent"
                            )),
                            Err(error) => Delivery::Failed(format!("{error:#}")),
                        }
                    }
                    OwnerNative::Claude { .. } => unreachable!("Claude's listener takes it above"),
                    // A local controller receipt does not establish that an
                    // agent turn started.
                    OwnerNative::Dot(request) => match request.deliver(job.deadline, &job.cancel) {
                        Ok(_) => Delivery::Accepted(Delivered::Submitted),
                        Err(error) => Delivery::Failed(error),
                    },
                };
                let _ = tx.send(Ev::MessageDelivered { id, batch, result });
            })
            .is_err()
        {
            self.message_finished(batch, id, Err("could not start native delivery".into()));
        }
    }

    /// The files of the Claude message at the front of session `id`'s queue
    /// were checked: hand Claude the text and their paths, unless the session
    /// changed or the delivery was canceled meanwhile.
    pub(super) fn claude_files_checked(&mut self, id: u64, batch: u64, result: Result<Vec<PathBuf>, String>) {
        let Some(job) = self.messages.queues.get(&id).and_then(|q| q.front()).filter(|j| j.batch == batch).cloned()
        else {
            return;
        };
        let OwnerNative::Claude { client_id, steer, .. } = &job.native else { return };
        let current = self.sessions.get(&id).is_some_and(|s| s.alive && s.closing.is_none() && job.targets(s));
        let sent = if job.cancel.load(Ordering::Relaxed) {
            Err("delivery canceled; nothing further was sent".to_string())
        } else if !current {
            Err("the session changed before delivery".into())
        } else {
            result.and_then(|paths| {
                let text = crate::remote::uploads::with_files(&job.text, &paths);
                if text.len() > crate::remote::wire::SEND_MAX {
                    return Err("the message and its file paths are too long; nothing was sent".into());
                }
                let files: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                let conversation = job.conversation.as_deref().unwrap_or_default();
                self.owner_input_send(
                    id,
                    batch,
                    conversation,
                    &text,
                    &files,
                    *steer,
                    client_id.as_deref(),
                    job.deadline,
                )
            })
        };
        if let Err(error) = sent {
            self.message_finished(batch, id, Err(error));
        }
    }

    pub(super) fn native_message_delivered(&mut self, id: u64, batch: u64, result: Delivery) {
        let admitted = self.messages.queues.get(&id).and_then(|q| q.front()).filter(|job| {
            job.batch == batch
                && !job.cancel.load(Ordering::Relaxed)
                && matches!(
                    &result,
                    Delivery::Accepted(
                        Delivered::Queued | Delivered::Started | Delivered::Steered | Delivered::Submitted
                    )
                )
        });
        if let Some(s) = self
            .sessions
            .get_mut(&id)
            .filter(|s| s.alive && s.closing.is_none() && admitted.is_some_and(|job| job.targets(s)))
        {
            s.tracker.user_input();
            s.tracker.clear_finished();
        }
        match result {
            Delivery::Accepted(delivered) => self.message_settled(batch, id, Ok(Some(delivered))),
            Delivery::Failed(error) => self.message_finished(batch, id, Err(error)),
            Delivery::Unsupported => self.message_finished(
                batch,
                id,
                Err("native message interface is unavailable; nothing was sent".into()),
            ),
        }
    }

    /// Settle a recipient without a provider receipt: a refusal, or a
    /// placement that carried the message elsewhere.
    pub(super) fn message_finished(&mut self, batch: u64, id: u64, result: Result<(), String>) {
        self.message_settled(batch, id, result.map(|()| None));
    }

    fn message_settled(&mut self, batch: u64, id: u64, result: Result<Option<Delivered>, String>) {
        // A successful result alone is not a custody receipt: a late native
        // reply can arrive after Stop, a new chat, or a replacement process.
        let admitted_inputs = self
            .messages
            .queues
            .get(&id)
            .and_then(|q| q.front())
            .filter(|job| job.batch == batch && !job.cancel.load(Ordering::Relaxed))
            .and_then(|job| {
                self.sessions
                    .get(&id)
                    .filter(|s| {
                        s.alive
                            && s.closing.is_none()
                            && self.shutdown_at.is_none()
                            && !self.pending_directories.contains_key(&id)
                            && job.targets(s)
                            && s.inputs == job.inputs
                    })
                    .map(|s| s.inputs)
            });
        if let Some(b) = self.messages.batches.get_mut(&batch) {
            if let Some((_, _, seen)) = b.results.iter_mut().find(|(who, _, _)| *who == id) {
                *seen = Some(result);
            }
            if b.results.iter().all(|(_, _, result)| result.is_some()) {
                let b = self.messages.batches.remove(&batch).unwrap();
                if let Some(receipt) = b.idle.as_ref() {
                    if let Some(inputs) =
                        admitted_inputs.filter(|_| b.results.iter().all(|(_, _, r)| matches!(r, Some(Ok(_)))))
                    {
                        if self.guarded_idle_admitted(receipt, inputs).is_err() {
                            self.log_error("Idle message admission could not be saved; outcome unknown");
                        }
                    } else {
                        self.guarded_idle_failed(receipt);
                    }
                }
                // Name what each provider confirmed: queued behind a turn, a
                // started turn, or submitted without a turn-start receipt.
                let sent: Vec<_> = b
                    .results
                    .iter()
                    .filter_map(|(_, name, r)| match r {
                        Some(Ok(Some(delivered))) => Some(format!("{name} ({})", delivered.label())),
                        Some(Ok(None)) => Some(name.clone()),
                        _ => None,
                    })
                    .collect();
                let skipped: Vec<_> = b
                    .results
                    .iter()
                    .filter_map(|(_, name, r)| r.as_ref()?.as_ref().err().map(|error| format!("{name} ({error})")))
                    .collect();
                let mut text = match sent.len() {
                    0 => "Sent to no session".into(),
                    1 => format!("Sent to {}", sent[0]),
                    n => format!("Sent to {n} sessions ({})", sent.join(", ")),
                };
                if !skipped.is_empty() {
                    text.push_str(&format!("; skipped {}", skipped.join(", ")));
                }
                // A rerun is done only once the agent took its message.
                if let Some(rerun) = b.rerun {
                    let outcome = if skipped.is_empty() && !sent.is_empty() {
                        Ok(())
                    } else {
                        Err(if skipped.is_empty() { "nothing was sent".into() } else { skipped.join(", ") })
                    };
                    if self.messages.rerun_now.as_ref().is_some_and(|(now, _)| *now == rerun) {
                        self.messages.rerun_now = Some((rerun, Some(outcome)));
                    } else {
                        self.rerun_settled(rerun, outcome);
                    }
                }
                if let Some((request, local)) = b.broadcast {
                    let sent =
                        b.results.iter().filter(|(_, _, r)| matches!(r, Some(Ok(_)))).map(|(id, _, _)| *id).collect();
                    let failed = b
                        .results
                        .iter()
                        .filter_map(|(id, _, r)| r.as_ref()?.as_ref().err().map(|error| (*id, error.clone())))
                        .collect();
                    if local {
                        self.broadcast_part_done(b.client, request, 0, sent, failed);
                    } else {
                        self.send(b.client, &ServerMsg::BroadcastDone { request, sent, failed });
                    }
                } else if let Some(request) = b.request {
                    let delivered = b.results.iter().find_map(|(_, _, r)| r.as_ref()?.as_ref().ok().copied().flatten());
                    let error = if sent.is_empty() { Some(text) } else { None };
                    self.send(b.client, &ServerMsg::MessageSent { id, request, error, delivered });
                } else {
                    self.notice(b.client, text, sent.is_empty());
                }
            }
        }
        let current = self.messages.queues.get(&id).and_then(|q| q.front()).is_some_and(|j| j.batch == batch);
        if current {
            let queue = self.messages.queues.get_mut(&id).unwrap();
            queue.pop_front();
            if queue.is_empty() {
                self.messages.queues.remove(&id);
            } else {
                self.start_message(id);
            }
        }
    }

    pub(super) fn messages_due(&mut self) {
        // Native sends share Close/Stop and conversation-generation fences.
        self.messages.peers.retain(|id, jobs| {
            jobs.retain(|job| {
                let Some(cancel) = job.cancel.upgrade() else { return false };
                if !self.sessions.get(id).is_some_and(|session| {
                    session.alive
                        && session.closing.is_none()
                        && session.spawned_ms == job.spawned_ms
                        && session.conversation == job.conversation
                }) || self.extensions.cancel.load(Ordering::Relaxed)
                {
                    cancel.store(true, Ordering::Relaxed);
                }
                !cancel.load(Ordering::Relaxed)
            });
            !jobs.is_empty()
        });
        self.owner_input_due();
    }
}
impl State {
    pub(super) fn peer_stop(&mut self, id: u64, conversation: &str, spawned_ms: u64) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.peers.entry(id).or_default().push(PeerSend {
            conversation: Some(conversation.into()),
            spawned_ms,
            cancel: Arc::downgrade(&cancel),
        });
        cancel
    }
    pub(super) fn cancel(&self, id: u64) {
        if let Some(jobs) = self.peers.get(&id) {
            for job in jobs {
                if let Some(cancel) = job.cancel.upgrade() {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        }
        if let Some(queue) = self.queues.get(&id) {
            for job in queue {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
    }
    pub(super) fn active(&self, id: u64) -> bool {
        self.queues.contains_key(&id)
    }
    /// Whether session `id`'s message in flight is still batch `batch`'s,
    /// and not canceled.
    pub(super) fn current(&self, id: u64, batch: u64) -> bool {
        self.queues
            .get(&id)
            .and_then(|q| q.front())
            .is_some_and(|job| job.batch == batch && !job.cancel.load(Ordering::Relaxed))
    }
    pub(super) fn busy(&self) -> bool {
        !self.queues.is_empty()
    }
}

#[cfg(test)]
#[path = "../tests/native_owner_dot.rs"]
mod dot_owner_tests;

#[cfg(test)]
#[path = "../tests/peer_lifecycle.rs"]
mod peer_tests;
