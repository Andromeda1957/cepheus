//! Ephemeral native replies: passive terminal reads while the transcript is
//! still waiting for its completed message. Nothing here writes to a session.
use std::collections::HashMap;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    chat::{self, Item},
    hub::Hub,
};
use crate::protocol::{read_msg, write_msg, ClientMsg, ClientRole, ServerMsg, SessionInfo, Tool, PROTOCOL_VERSION};
use anyhow::{bail, Result};

const CADENCE: Duration = Duration::from_millis(100);

pub(super) fn start(hub: &Arc<Hub>) {
    let weak = Arc::downgrade(hub);
    thread::spawn(move || {
        let mut workers: HashMap<u64, thread::JoinHandle<()>> = HashMap::new();
        while let Some(hub) = weak.upgrade() {
            workers.retain(|_, handle| !handle.is_finished());
            for id in hub.followed() {
                workers.entry(id).or_insert_with(|| {
                    let weak = Arc::downgrade(&hub);
                    thread::spawn(move || follow(weak, id))
                });
            }
            drop(hub);
            thread::sleep(CADENCE);
        }
    });
}

fn follow(weak: Weak<Hub>, id: u64) {
    let mut reader = None;
    let mut replies = ReplyState::default();
    while let Some(hub) = weak.upgrade() {
        if !hub.followed().contains(&id) {
            break;
        }
        let snapshot = hub
            .session(id)
            .filter(|info| info.alive && matches!(info.tool, Tool::Claude | Tool::Codex))
            .and_then(|info| {
                let read = (|| {
                    if reader.is_none() {
                        reader = Some(ScreenReader::open(&hub.opts.socket)?);
                    }
                    reader.as_mut().unwrap().read(id)
                })();
                match read {
                    Ok(lines) => Some((info.conversation, lines)),
                    Err(_) => {
                        reader = None;
                        None
                    }
                }
            });
        hub.update_chat(id, snapshot, &mut replies);
        drop(hub);
        thread::sleep(CADENCE);
    }
}

/// This channel is separate from prompt/command requests; it never attaches a
/// terminal, changes focus, resizes it or moves its scrollback position.
struct ScreenReader {
    read: BufReader<UnixStream>,
    write: UnixStream,
}
impl ScreenReader {
    fn open(path: &Path) -> Result<Self> {
        let mut write = UnixStream::connect(path)?;
        write.set_read_timeout(Some(Duration::from_millis(500)))?;
        write.set_write_timeout(Some(Duration::from_millis(500)))?;
        let read = BufReader::new(write.try_clone()?);
        write_msg(&mut write, &ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control })?;
        Ok(Self { read, write })
    }
    fn read(&mut self, id: u64) -> Result<Vec<String>> {
        write_msg(&mut self.write, &ClientMsg::Screen { id, tail: Some(512) })?;
        let until = Instant::now() + Duration::from_millis(750);
        while Instant::now() < until {
            match read_msg::<_, ServerMsg>(&mut self.read)? {
                Some(ServerMsg::ScreenText { id: got, lines, .. }) if got == id => return Ok(lines),
                None => break,
                _ => {}
            }
        }
        bail!("live output unavailable")
    }
}

fn normalized(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Anchoring survives bounded scrollback. It resets with the canonical turn,
/// and a newly opened, already static block is withheld until it grows.
#[derive(Default)]
pub(super) struct ReplyState {
    context: Option<(Tool, Option<String>, u64, usize, String)>,
    anchored: bool,
    last: Option<String>,
    accepted: Option<String>,
    total: usize,
    completed: Vec<String>,
}

impl ReplyState {
    pub(super) fn reply(
        &mut self,
        info: &SessionInfo,
        lines: &[String],
        generation: u64,
        dropped: usize,
        items: &[Item],
    ) -> Option<String> {
        let user = items.iter().rposition(|item| matches!(item, Item::User { .. }))?;
        let Item::User { text: prompt, .. } = &items[user] else {
            return None;
        };
        let context = (info.tool, info.conversation.clone(), generation, dropped + user, prompt.clone());
        if self.context.as_ref() != Some(&context) {
            *self = Self { context: Some(context), ..Self::default() };
        }
        let total = dropped + items.len();
        let persisted = total != self.total;
        if persisted {
            self.completed = items[user + 1..]
                .iter()
                .rev()
                .filter_map(|item| match item {
                    Item::Assistant { text, .. } => Some(normalized(text)),
                    _ => None,
                })
                .take(8)
                .collect();
        }
        self.total = total;
        let working = info.alive && crate::providers::screen::screen_signals(lines).working;
        if !working {
            self.last = None;
            self.accepted = None;
            return None;
        }
        let previous = self.last.clone();
        let Some((visible_user, text)) = crate::providers::screen::live_reply(info.tool, lines) else {
            self.last = None;
            // Keep the accepted prefix when an enormous block's opening marker
            // leaves our bounded tail. Its canonical record still replaces it.
            let prompt = if info.tool == Tool::Claude { '❯' } else { '›' };
            let no_head = !lines.iter().any(|line| {
                (line.starts_with("● ") || line.starts_with("⏺ ") || line.starts_with("• "))
                    && !line.contains("esc to interrupt")
            });
            return (lines.len() == 512 && no_head && lines.iter().filter(|l| l.starts_with(prompt)).count() == 1)
                .then(|| self.accepted.clone())
                .flatten()
                .filter(|text| !self.completed.iter().any(|saved| saved.starts_with(&normalized(text))));
        };
        let text = chat::clip(&text, chat::TEXT_MAX);
        let key = normalized(&text);
        // A visible current prompt is one anchor. When it has scrolled out,
        // actual growth since this follower's previous snapshot anchors it.
        let visible_matches = visible_user.as_deref().is_some_and(|user| {
            let user = normalized(user);
            let prompt = normalized(prompt);
            !user.is_empty() && !prompt.is_empty() && (prompt.starts_with(&user) || user.starts_with(&prompt))
        });
        // A different visible prompt cannot be overridden by observed growth.
        if visible_user.is_some() && !visible_matches {
            self.anchored = false;
            self.last = None;
            self.accepted = None;
            return None;
        }
        self.anchored |=
            visible_matches || (visible_user.is_none() && previous.as_ref().is_some_and(|old| old != &key));
        self.last = Some(key.clone());
        if !self.anchored
            || key.is_empty()
            || self.completed.iter().any(|saved| !saved.is_empty() && saved.starts_with(&key))
        {
            return None;
        }
        self.accepted = Some(text.clone());
        Some(text)
    }
}
