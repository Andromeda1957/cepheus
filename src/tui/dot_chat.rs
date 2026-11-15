//! Dot is a chat, not a terminal program: the deck draws its conversation
//! from the bridge's local message snapshot and keeps the composer itself,
//! submitting through the provider's private, correlated API.
//! The bridge keeps every delivery safeguard (journal, idempotency, uncertain
//! delivery blocking); this view only presents and composes.
use super::*;
use crate::dot::api::{Image, Message};
use crate::kitty;
use crate::pet::{self, Mood, Profile};
use crate::remote::chat::Item;
use std::path::PathBuf;
use std::time::SystemTime;

/// The bridge refuses longer messages without truncating them.
pub const DRAFT_MAX: usize = 32_000;
/// How often the open conversation looks for a new snapshot.
const REFRESH: Duration = Duration::from_millis(400);
/// How often the sidebar looks for a new failure while the chat is closed.
const QUIET_REFRESH: Duration = Duration::from_secs(2);
/// Allow the controller's bounded upload/send exchange to settle first.
const SEND_WAIT: Duration = Duration::from_secs(130);
const REQUEST_BASE: u64 = 1 << 62;

#[derive(Default)]
pub struct View {
    pub items: Vec<Item>,
    /// Wrapped lines scrolled up from the newest message.
    pub scroll: usize,
    pub draft: String,
    pub images: Vec<Image>,
    /// Acknowledged messages stay visible until the same IDs reach the feed.
    pub submitted: Vec<Submitted>,
    ids: Vec<String>,
    /// The message awaiting the service's delivery receipt.
    pub sending: Option<Sending>,
    pub problem: Option<String>,
    path: Option<PathBuf>,
    stamp: Option<(SystemTime, u64)>,
    checked: Option<Instant>,
    /// A snapshot being read and parsed off the interface thread.
    feed_loading: Option<std::sync::mpsc::Receiver<Feed>>,
    /// Bumped whenever what the conversation shows changes.
    pub generation: u64,
    /// The conversation wrapped for the last frame's width and theme.
    pub rendered: Option<Rendered>,
    next_request: u64,
    /// The Dot's name and picture as set in ChatGPT, kept by the bridge.
    pub profile: Option<Profile>,
    /// The art the terminal holds and shows.
    pub art: Option<ArtShape>,
    /// Kitty draws images here; set when the interface starts.
    pub graphics: bool,
    profile_stamp: Option<(SystemTime, u64)>,
    profile_checked: Option<Instant>,
    art_key: Option<String>,
    art_failed: Option<(String, Instant)>,
    art_loading: Option<std::sync::mpsc::Receiver<(String, Result<(Vec<u8>, ArtShape), String>)>>,
    upload: Option<(Vec<u8>, ArtShape)>,
    placed: bool,
}

pub struct Rendered {
    pub generation: u64,
    pub width: u16,
    pub theme: crate::theme::Theme,
    /// The local date the times were written against: past midnight,
    /// yesterday's times gain their date.
    pub day: String,
    pub rows: Vec<ratatui::text::Line<'static>>,
}

/// A snapshot read off the interface thread: its path, its stamp when read,
/// and its messages with their identities (none when it was unreadable).
type Feed = (PathBuf, Option<(SystemTime, u64)>, Option<(Vec<Item>, Vec<String>)>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtShape {
    /// One looping animation per mood.
    Animated,
    /// One picture for every mood.
    Still,
}

/// Kitty image ids, one per mood; a still picture uses the first.
const ART_ID: u32 = 0x0d_07_00;
/// The pet fills this many sidebar cells: two rows tall, about square.
pub const AVATAR_COLUMNS: u16 = 4;
pub const AVATAR_ROWS: u16 = 2;
const PROFILE_REFRESH: Duration = Duration::from_secs(2);
/// After a failed load (offline, say), try the same art again this late.
const ART_RETRY: Duration = Duration::from_secs(300);

fn mood_id(mood: Mood) -> u32 {
    ART_ID + 1 + Mood::ALL.iter().position(|m| *m == mood).unwrap_or(0) as u32
}

/// The terminal bytes that send a profile's art, built off the interface thread.
fn art_upload(profile: &Profile, dir: &std::path::Path) -> anyhow::Result<(Vec<u8>, ArtShape)> {
    Ok(match pet::load(profile, dir)? {
        pet::Art::Animated(moods) => {
            let bytes = Mood::ALL
                .iter()
                .zip(&moods)
                .flat_map(|(mood, frames)| kitty::animation(mood_id(*mood), frames))
                .collect();
            (bytes, ArtShape::Animated)
        }
        pet::Art::Png(png) => (kitty::png(mood_id(Mood::Still), &png), ArtShape::Still),
        pet::Art::Still(frame) => (kitty::animation(mood_id(Mood::Still), &[frame]), ArtShape::Still),
    })
}

pub struct Sending {
    pub session: u64,
    pub request: u64,
    pub text: String,
    pub images: Vec<Image>,
    pub at: Instant,
}

pub struct Submitted {
    pub id: String,
    pub text: String,
    pub images: Vec<Image>,
}

fn snapshot_stamp(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
}

/// The local Dot provider's snapshot, when this view shares its machine.
fn snapshot_path() -> Option<PathBuf> {
    if crate::client::is_remote_coordinator() {
        return None;
    }
    crate::providers::get(Tool::Dynamic("dot")).and_then(|p| p.chat_snapshot)
}

impl App {
    /// The selected session is the local Dot drawn as a native chat.
    pub fn dot_native(&self) -> bool {
        self.current_session().is_some_and(|s| s.tool == Tool::Dynamic("dot") && s.host.is_none())
            && snapshot_path().is_some()
    }

    /// The Dot's own name from ChatGPT, else "Dot".
    pub fn dot_name(&self) -> String {
        self.dot.profile.as_ref().and_then(|p| p.name.clone()).unwrap_or_else(|| "Dot".into())
    }

    /// This machine's Dot bridge is running.
    fn dot_live(&self) -> bool {
        self.state.sessions.iter().any(|s| s.tool == Tool::Dynamic("dot") && s.host.is_none() && s.alive)
    }

    /// What is failing between the deck and Dot right now (it cannot connect,
    /// or delivery is uncertain), as the running bridge last reported it.
    pub fn dot_failure(&self) -> Option<&str> {
        match self.dot.items.last() {
            Some(Item::Notice { text }) if self.dot_live() => Some(text),
            _ => None,
        }
    }

    /// What the conversation shows changed: wrap it again on the next frame.
    fn dot_changed(&mut self) {
        self.dot.generation += 1;
        self.dirty = true;
    }

    /// The image showing `mood`, once the terminal has the art.
    pub fn dot_avatar(&self, mood: Mood) -> Option<u32> {
        match self.dot.art? {
            ArtShape::Animated => Some(mood_id(mood)),
            ArtShape::Still => Some(mood_id(Mood::Still)),
        }
    }

    /// Bytes to write before the next frame: new art, and the placements a
    /// new art or a resized terminal needs.
    pub(super) fn dot_graphics(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some((bytes, shape)) = self.dot.upload.take() {
            out = bytes;
            self.dot.art = Some(shape);
            self.dot.placed = false;
        }
        if self.dot.art.is_some() && !self.dot.placed {
            for mood in Mood::ALL {
                out.extend(kitty::place(mood_id(mood), AVATAR_COLUMNS, AVATAR_ROWS));
            }
            self.dot.placed = true;
        }
        out
    }

    pub(super) fn dot_resized(&mut self) {
        self.dot.placed = false;
    }

    /// Follow the bridge's profile, and load the art when it changes.
    fn poll_profile(&mut self, now: Instant) {
        if let Some(loading) = &self.dot.art_loading {
            match loading.try_recv() {
                Ok((key, Ok(upload))) => {
                    self.dot.art_key = Some(key);
                    self.dot.upload = Some(upload);
                    self.dot.art_loading = None;
                    self.dot.art_failed = None;
                    self.dirty = true;
                }
                Ok((key, Err(_))) => {
                    self.dot.art_failed = Some((key, now));
                    self.dot.art_loading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.dot.art_loading = None,
            }
        }
        if self.dot.profile_checked.is_some_and(|at| now.duration_since(at) < PROFILE_REFRESH) {
            return;
        }
        self.dot.profile_checked = Some(now);
        let Some(dir) = snapshot_path().and_then(|p| p.parent().map(std::path::Path::to_path_buf)) else { return };
        let stamp = std::fs::metadata(dir.join("profile.json"))
            .ok()
            .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        if stamp != self.dot.profile_stamp {
            self.dot.profile_stamp = stamp;
            let profile = Profile::read(&dir);
            if profile != self.dot.profile {
                self.dot.profile = profile;
                self.dirty = true;
            }
        }
        let Some(profile) = self.dot.profile.clone().filter(|_| self.dot.graphics && self.dot.art_loading.is_none())
        else {
            return;
        };
        let Some(key) = profile.art_key().filter(|k| self.dot.art_key.as_ref() != Some(k)) else { return };
        if self
            .dot
            .art_failed
            .as_ref()
            .is_some_and(|(failed, at)| *failed == key && now.duration_since(*at) < ART_RETRY)
        {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send((key, art_upload(&profile, &dir).map_err(|e| e.to_string())));
        });
        self.dot.art_loading = Some(rx);
    }

    /// Follow the bridge's snapshot: often while the conversation is open,
    /// slowly while only the sidebar shows Dot, and not at all while the
    /// bridge is off and the conversation closed. Only a changed file is
    /// read, off this thread.
    pub(super) fn poll_dot(&mut self, now: Instant) {
        self.poll_profile(now);
        if let Some(sending) = &self.dot.sending {
            if now.duration_since(sending.at) > SEND_WAIT {
                if let Some(sending) = self.dot.sending.take() {
                    self.restore_draft(sending.text, sending.images);
                }
                self.dot.problem = Some("No delivery receipt arrived. Check the chat before sending again.".into());
                self.dot_changed();
            }
        }
        if let Some(loading) = &self.dot.feed_loading {
            match loading.try_recv() {
                Ok((path, stamp, feed)) => {
                    self.dot.feed_loading = None;
                    self.apply_dot_feed(path, stamp, feed);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.dot.feed_loading = None,
            }
        }
        let open = self.dot_native();
        let every = if open { REFRESH } else { QUIET_REFRESH };
        if !(open || self.dot_live()) || self.dot.checked.is_some_and(|at| now.duration_since(at) < every) {
            return;
        }
        self.dot.checked = Some(now);
        let Some(path) = snapshot_path() else { return };
        let stamp = snapshot_stamp(&path);
        if self.dot.path.as_ref() == Some(&path) && self.dot.stamp == stamp {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let feed = crate::providers::snapshot::identified(&path);
            let _ = tx.send((path, stamp, feed));
        });
        self.dot.feed_loading = Some(rx);
    }

    #[cfg(test)]
    fn read_dot_snapshot(&mut self, path: PathBuf) {
        let stamp = snapshot_stamp(&path);
        if self.dot.path.as_ref() == Some(&path) && self.dot.stamp == stamp {
            return;
        }
        let feed = crate::providers::snapshot::identified(&path);
        self.apply_dot_feed(path, stamp, feed);
    }

    /// A failed read keeps the last good conversation on screen and retries
    /// at the next poll. Only successfully read file versions are cached.
    fn apply_dot_feed(
        &mut self,
        path: PathBuf,
        stamp: Option<(SystemTime, u64)>,
        feed: Option<(Vec<Item>, Vec<String>)>,
    ) {
        let Some((items, ids)) = feed else { return };
        self.dot.path = Some(path);
        self.dot.stamp = stamp;
        self.dot.ids = ids;
        let waiting = self.dot.submitted.len();
        self.dot.submitted.retain(|sent| !self.dot.ids.contains(&sent.id));
        if items != self.dot.items || waiting != self.dot.submitted.len() {
            self.dot.items = items;
            self.dot_changed();
        }
    }

    /// Insert-mode keys edit the composer; Enter submits it.
    pub(super) fn dot_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter if alt => self.dot_type("\n"),
            KeyCode::Char('j') if ctrl => self.dot_type("\n"),
            KeyCode::Char('v') if ctrl => match self.clipboard.read() {
                Ok(clip) => self.dot_paste(clip),
                Err(error) => self.dot.problem = Some(format!("Could not read the clipboard: {error:#}")),
            },
            KeyCode::Enter => self.dot_submit(),
            KeyCode::Char('u') if ctrl => {
                self.dot.draft.clear();
                self.dot.images.clear();
                self.dot.problem = None;
            }
            KeyCode::Char('w') if ctrl => {
                let kept = self.dot.draft.trim_end().len();
                let start = self.dot.draft[..kept].rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.dot.draft.truncate(start);
            }
            KeyCode::Backspace if self.dot.draft.is_empty() => {
                self.dot.images.pop();
            }
            KeyCode::Backspace => {
                self.dot.draft.pop();
            }
            KeyCode::PageUp => self.dot_scroll(ScrollOp::PageUp),
            KeyCode::PageDown => self.dot_scroll(ScrollOp::PageDown),
            KeyCode::End => self.dot_scroll(ScrollOp::Bottom),
            KeyCode::Home if ctrl => self.dot_scroll(ScrollOp::Top),
            KeyCode::Up if self.dot.draft.is_empty() => self.dot_scroll(ScrollOp::Lines(1)),
            KeyCode::Down if self.dot.draft.is_empty() => self.dot_scroll(ScrollOp::Lines(-1)),
            KeyCode::Tab => self.dot_type("    "),
            KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => {
                self.dot_type(c.encode_utf8(&mut [0; 4]))
            }
            _ => {}
        }
        self.dirty = true;
    }

    pub(super) fn dot_type(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let clean: String = text.chars().filter(|c| *c == '\n' || !c.is_control()).collect();
        // Keep a little past the limit so the owner sees it was too long
        // instead of having it silently cut.
        let room = (DRAFT_MAX + 1).saturating_sub(self.dot.draft.chars().count());
        self.dot.draft.extend(clean.chars().take(room));
        self.dirty = true;
    }

    fn dot_paste(&mut self, clip: crate::clipboard::Clip) {
        match clip {
            crate::clipboard::Clip::Text(text) => self.dot_type(&text),
            crate::clipboard::Clip::Image { name, bytes } => {
                let result = (|| {
                    let image = Image::new(&name, &bytes)?;
                    let mut images = self.dot.images.clone();
                    images.push(image);
                    Message { text: self.dot.draft.clone(), images: images.clone() }.validate()?;
                    Ok::<_, String>(images)
                })();
                match result {
                    Ok(images) => {
                        self.dot.images = images;
                        self.dot.problem = None;
                    }
                    Err(error) => self.dot.problem = Some(error),
                }
            }
            crate::clipboard::Clip::Empty => self.dot.problem = Some("The clipboard is empty.".into()),
        }
        self.dirty = true;
    }

    fn dot_submit(&mut self) {
        let Some((id, expected)) = self
            .current_session()
            .filter(|s| s.tool == Tool::Dynamic("dot"))
            .and_then(|s| crate::attention::ExpectedTarget::from_session(s).map(|e| (s.id, e)))
        else {
            self.dot.problem = Some("Dot is disconnected. :dot on reconnects; the draft is kept.".into());
            return;
        };
        if self.dot.sending.is_some() {
            self.dot.problem = Some("Still sending the previous message.".into());
            return;
        }
        let text = self.dot.draft.trim_end().to_string();
        if text.trim().is_empty() && self.dot.images.is_empty() {
            return;
        }
        if text.chars().count() > DRAFT_MAX {
            self.dot.problem = Some(format!("Over {DRAFT_MAX} characters; nothing was sent."));
            return;
        }
        if !self.service_has(19, "Direct sending to Dot") {
            return;
        }
        self.dot.next_request += 1;
        let request = REQUEST_BASE | self.dot.next_request;
        let images = std::mem::take(&mut self.dot.images);
        self.send(ClientMsg::Plugin {
            request,
            plugin: "provider-dot".into(),
            command: "send".into(),
            args: serde_json::json!({"id":id,"expected":expected,"text":text,"images":images}),
        });
        self.dot.sending = Some(Sending { session: id, request, text, images, at: Instant::now() });
        self.dot.draft.clear();
        self.dot.problem = None;
        self.dot.scroll = 0;
        self.dot_changed();
    }

    /// The service's receipt for a composer message; a refusal returns the
    /// text to the composer so nothing typed is lost.
    pub(super) fn dot_sent(&mut self, id: u64, request: u64, error: Option<String>) {
        if !self.dot.sending.as_ref().is_some_and(|s| s.session == id && s.request == request) {
            return;
        }
        self.dot_result(
            request,
            serde_json::Value::Null,
            error.or_else(|| Some("Dot delivery acknowledgement is uncertain. Check the conversation.".into())),
        );
    }

    pub(super) fn dot_result(&mut self, request: u64, data: serde_json::Value, error: Option<String>) -> bool {
        if self.dot.sending.as_ref().is_none_or(|s| s.request != request) {
            return false;
        }
        let sending = self.dot.sending.take().unwrap();
        self.dot_changed();
        let control = sending.images.is_empty().then(|| crate::dot::api::Control::parse(&sending.text)).flatten();
        let acknowledged = crate::dot::api::acknowledged(&data, control);
        if error.is_none() && control.is_some() && acknowledged {
            return true;
        }
        let id = data["id"].as_str().filter(|id| !id.is_empty());
        if let Some(error) = error.or_else(|| {
            (!acknowledged).then(|| "Dot delivery acknowledgement is uncertain. Check the conversation.".into())
        }) {
            self.restore_draft(sending.text, sending.images);
            self.dot.problem = Some(util::one_line(&error));
        } else if let Some(id) = id.filter(|id| !self.dot.ids.iter().any(|seen| seen == id)) {
            self.dot.submitted.push(Submitted { id: id.into(), text: sending.text, images: sending.images });
        }
        self.dirty = true;
        true
    }

    fn restore_draft(&mut self, text: String, mut images: Vec<Image>) {
        if self.dot.draft.is_empty() {
            self.dot.draft = text;
        } else if !text.is_empty() {
            self.dot.draft = format!("{text}\n{}", self.dot.draft);
        }
        images.append(&mut self.dot.images);
        self.dot.images = images;
    }

    /// Scrolling moves through the wrapped conversation the view already
    /// holds: no service request per step.
    pub(super) fn dot_scroll(&mut self, op: ScrollOp) {
        let page = crate::view::dot_page(self);
        let limit = crate::view::dot_scroll_limit(self);
        self.dot.scroll = match op {
            ScrollOp::Top => limit,
            ScrollOp::Bottom => 0,
            ScrollOp::Lines(n) if n >= 0 => self.dot.scroll.saturating_add(n as usize).min(limit),
            ScrollOp::Lines(n) => self.dot.scroll.saturating_sub(n.unsigned_abs() as usize),
            ScrollOp::PageUp => self.dot.scroll.saturating_add(page).min(limit),
            ScrollOp::PageDown => self.dot.scroll.saturating_sub(page),
        };
        self.dirty = true;
    }
}

#[cfg(test)]
#[path = "../tests/tui_dot.rs"]
mod tests;
