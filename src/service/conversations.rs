use super::*;

/// Resume-list scans: at most one per deck at a time. A request that arrives
/// while one runs waits, replacing any request already waiting. A search reads
/// every saved transcript once and reuses that listing for the next queries
/// until the deck lists its recent conversations again.
#[derive(Default)]
pub(super) struct Scans {
    running: HashMap<u64, Arc<AtomicBool>>,
    waiting: HashMap<u64, (String, Option<u64>)>,
    listing: Arc<Mutex<Option<Arc<Vec<Conversation>>>>>,
}

impl Scans {
    /// A deck left: stop its scan and drop its waiting request.
    pub(super) fn forget(&mut self, cid: u64) {
        if let Some(cancel) = self.running.remove(&cid) {
            cancel.store(true, Ordering::Relaxed);
        }
        self.waiting.remove(&cid);
    }
}

impl Service {
    /// A deck's scan finished: start the request that waited for it.
    pub(super) fn conversations_scanned(&mut self, cid: u64) {
        self.conversation_scans.running.remove(&cid);
        if let Some((query, request)) = self.conversation_scans.waiting.remove(&cid) {
            self.list_conversations(cid, query, request);
        }
    }

    /// The earlier conversations of every program, newest first, for the
    /// `Alt+r` picker and the phone's resume list. A deck older than a
    /// program is not sent its conversations: it cannot read the message.
    pub(super) fn list_conversations(&mut self, cid: u64, query: String, request: Option<u64>) {
        let words = if query.trim().is_empty() {
            Vec::new()
        } else {
            match remote::search::words(&query) {
                Ok(words) => words.into_iter().map(|word| word.to_lowercase()).collect(),
                Err(error) => {
                    self.notice(cid, error.to_string(), true);
                    return;
                }
            }
        };
        // The scan thread holds the other reference until it ends.
        if self.conversation_scans.running.get(&cid).is_some_and(|scan| Arc::strong_count(scan) > 1) {
            self.conversation_scans.waiting.insert(cid, (query, request));
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.conversation_scans.running.insert(cid, cancel.clone());
        self.conversation_scans.waiting.remove(&cid);
        let listing = self.conversation_scans.listing.clone();
        let profiles: HashMap<_, _> = self
            .registry
            .sessions
            .iter()
            .filter_map(|s| Some(((s.tool, s.conversation.clone()?), PathBuf::from(s.account_root.as_ref()?))))
            .collect();
        let claude_dir = self.opts.claude_dir.clone();
        let codex_dir = self.opts.codex_dir.clone();
        let agy_dir = self.opts.antigravity_dir.clone();
        let grok_home = self.opts.grok_home.clone();
        let opencode_db = self.opts.opencode_db.clone();
        let start_dir = self.opts.start_dir.clone();
        let version = self.clients.get(&cid).and_then(|c| c.version).unwrap_or(PROTOCOL_VERSION);
        let open: HashMap<(Tool, String), u64> = self
            .sessions
            .values()
            .filter(|s| s.alive)
            .filter_map(|s| s.conversation.clone().map(|c| ((s.tool, c), s.id)))
            .collect();
        let saved: Vec<Conversation> = self
            .registry
            .sessions
            .iter()
            .filter(|s| {
                matches!(s.tool, Tool::Dynamic(_))
                    || s.account.is_some()
                    || (s.ended_at.is_some()
                        && self.sessions.get(&s.id).is_none_or(|session| has_conversation(session, &self.opts)))
            })
            .filter_map(|s| {
                Some(Conversation {
                    search_words: Vec::new(),
                    tool: s.tool,
                    id: s.conversation.clone()?,
                    title: s
                        .account
                        .as_ref()
                        .map(|account| format!("[{account}] {}", s.name.as_ref().unwrap_or(&s.title)))
                        .unwrap_or_else(|| s.name.clone().unwrap_or_else(|| s.title.clone())),
                    created_at: s.created_at,
                    updated_at: s.ended_at.unwrap_or(s.created_at),
                    open_in: None,
                    host: None,
                })
            })
            .collect();
        let tx = self.tx.clone();
        let spawned = thread::Builder::new().name("conversations".into()).spawn(move || {
            let reuse = match listing.lock() {
                // A plain listing makes the next search read the transcripts afresh.
                Ok(mut listed) if words.is_empty() => {
                    *listed = None;
                    None
                }
                Ok(listed) => listed.clone(),
                Err(_) => None,
            };
            let mut items = saved;
            if let Some(listed) = reuse {
                items.extend(listed.iter().cloned());
            } else {
                let limit = if words.is_empty() { 40 } else { usize::MAX };
                let mut found = Vec::new();
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                found.extend(claude::conversations(&claude_dir, &start_dir, limit));
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                found.extend(codex::conversations(&codex_dir, &start_dir, limit));
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                found.extend(antigravity::conversations(&agy_dir, &start_dir, limit));
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                found.extend(other::grok_conversations(&grok_home, &start_dir, limit));
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                found.extend(other::opencode_conversations(&opencode_db, &start_dir, limit));
                if !words.is_empty() {
                    if let Ok(mut listed) = listing.lock() {
                        *listed = Some(Arc::new(found.clone()));
                    }
                }
                items.extend(found);
            }
            let mut seen = std::collections::HashSet::new();
            items.retain(|c| seen.insert((c.tool, c.id.clone())));
            items.retain(|c| c.tool.since_version() <= version);
            for item in &mut items {
                item.open_in = open.get(&(item.tool, item.id.clone())).copied();
            }
            if !words.is_empty() {
                let mut roots =
                    vec![(Tool::Claude, claude_dir), (Tool::Codex, codex_dir), (Tool::Antigravity, agy_dir)];
                roots.extend(profiles.iter().map(|((tool, _), root)| (*tool, root.clone())));
                roots.extend(
                    fs::read_dir(grok_home.join("sessions"))
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter(|entry| entry.path().is_dir())
                        .map(|entry| (Tool::Grok, entry.path())),
                );
                let mut seen_roots = HashSet::new();
                roots.retain(|root| seen_roots.insert(root.clone()));
                prompt_matches(&mut items, &roots, &profiles, &words, &cancel);
            }
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            items.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
            let _ = tx.send(Ev::Conversations(cid, query, request, items));
        });
        if spawned.is_err() {
            self.conversation_scans.running.remove(&cid);
        }
    }
}

/// Native submitted-prompt histories are small even when tool transcripts span
/// gigabytes. Search those instructions without exporting or retaining their text.
fn prompt_matches(
    items: &mut [Conversation],
    roots: &[(Tool, PathBuf)],
    profiles: &HashMap<(Tool, String), PathBuf>,
    words: &[String],
    cancel: &AtomicBool,
) {
    for (tool, root) in roots {
        let targets: HashMap<_, _> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.tool == *tool && profiles.get(&(*tool, item.id.clone())).is_none_or(|profile| profile == root)
            })
            .map(|(i, item)| (item.id.clone(), i))
            .collect();
        if targets.is_empty() {
            continue;
        }
        let filename = if *tool == Tool::Grok { "prompt_history.jsonl" } else { "history.jsonl" };
        history_records(&root.join(filename), cancel, |record| {
            if record.get("is_bash").and_then(Value::as_bool) == Some(true)
                || record.get("type").and_then(Value::as_str) == Some("slash_command")
            {
                return;
            }
            let (id_key, text_key) = match tool {
                Tool::Claude => ("sessionId", "display"),
                Tool::Antigravity => ("conversationId", "display"),
                Tool::Codex => ("session_id", "text"),
                Tool::Grok => ("session_id", "prompt"),
                _ => return,
            };
            let Some(index) = record.get(id_key).and_then(Value::as_str).and_then(|id| targets.get(id)) else { return };
            let item = &mut items[*index];
            mark_words(item, record.get(text_key).and_then(Value::as_str).unwrap_or(""), words);
            if *tool == Tool::Claude {
                for paste in
                    record.get("pastedContents").and_then(Value::as_object).into_iter().flat_map(|p| p.values())
                {
                    if paste.get("type").and_then(Value::as_str) != Some("text") {
                        continue;
                    }
                    if let Some(text) = paste.get("content").and_then(Value::as_str) {
                        mark_words(item, text, words);
                    } else if let Some(hash) = paste.get("contentHash").and_then(Value::as_str).filter(|h| {
                        h.len() == 16 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    }) {
                        let path = root.join("paste-cache").join(format!("{hash}.txt"));
                        if let Some(bytes) = cached_paste(&path) {
                            mark_words(item, &String::from_utf8_lossy(&bytes), words);
                        } else if valid_conversation_id(&item.id) {
                            // A missing paste cache can still have its original user text
                            // in this chat's transcript. Only that exceptional chat is read.
                            if let Some(project) = record.get("project").and_then(Value::as_str) {
                                let path = claude::transcript_path(root, project, &item.id);
                                history_records(&path, cancel, |record| {
                                    if let Some(text) = claude::typed_text(record) {
                                        mark_words(item, &text, words);
                                    }
                                });
                            }
                        }
                    }
                }
            }
        });
    }
}

/// Reject symlinked cache entries and read only a bounded text payload.
fn cached_paste(path: &Path) -> Option<Vec<u8>> {
    if fs::symlink_metadata(path.parent()?).ok()?.file_type().is_symlink() {
        return None;
    }
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(16 << 20).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn mark_words(item: &mut Conversation, text: &str, words: &[String]) {
    let text = text.to_lowercase();
    for word in words {
        if !item.search_words.contains(word) && text.contains(word) {
            item.search_words.push(word.clone());
        }
    }
}

/// Bounded JSONL records; cancelled queries stop between reads and records.
fn history_records(path: &Path, cancel: &AtomicBool, mut visit: impl FnMut(&Value)) {
    let Ok(meta) = fs::metadata(path) else { return };
    let mut offset = 0;
    while offset < meta.len() && !cancel.load(Ordering::Relaxed) {
        let to = meta.len().min(offset + (16 << 20));
        let Ok(bytes) = util::read_between(path, offset, to) else { return };
        let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else {
            offset = to;
            continue;
        };
        for line in bytes[..end].split(|b| *b == b'\n') {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if let Ok(record) = serde_json::from_slice(line) {
                visit(&record);
            }
        }
        offset += end as u64 + 1;
    }
}

#[cfg(test)]
#[path = "../tests/history_prompts.rs"]
mod tests;
