//! A view owns one bounded frozen history snapshot; live frames remain separate.
use super::*;
use crate::screen::{HistorySnapshot, HISTORY_PROTOCOL, MAX_HISTORY_BYTES, MAX_HISTORY_LINES};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fence {
    id: u64,
    epoch: u64,
    cols: u16,
    rows: u16,
}
struct Pending {
    fence: Fence,
    request: u64,
    revision: u64,
    offset: usize,
}
struct Frozen {
    fence: Fence,
    snapshot: HistorySnapshot,
    offset: usize,
    frame: ScreenFrame,
}
#[derive(Default)]
pub(super) struct History {
    serial: u64,
    pending: Option<Pending>,
    frozen: Option<Frozen>,
}

fn moved(offset: usize, op: ScrollOp, rows: u16, limit: usize) -> usize {
    let page = usize::from(rows).saturating_sub(1).max(1);
    match op {
        ScrollOp::Lines(n) if n > 0 => offset.saturating_add(n as usize).min(limit),
        ScrollOp::Lines(n) => offset.saturating_sub(n.unsigned_abs() as usize),
        ScrollOp::PageUp => offset.saturating_add(page).min(limit),
        ScrollOp::PageDown => offset.saturating_sub(page),
        ScrollOp::Top => limit,
        ScrollOp::Bottom => 0,
    }
}

impl History {
    fn active(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| p.offset > 0) || self.frozen.as_ref().is_some_and(|f| f.offset > 0)
    }
    pub(super) fn clear(&mut self) {
        self.pending = None;
        self.frozen = None;
    }
    pub(super) fn observe_state(&mut self, state: &DeckState) {
        let fence = self.pending.as_ref().map(|p| p.fence).or_else(|| self.frozen.as_ref().map(|f| f.fence));
        if fence.is_some_and(|fence| {
            !state.sessions.iter().any(|s| {
                s.id == fence.id
                    && s.has_screen
                    && s.metadata
                        .get("_deck_terminal_epoch")
                        .and_then(|v| v.as_u64())
                        .is_some_and(|epoch| epoch == fence.epoch)
            })
        }) {
            self.clear();
        }
    }
    pub(super) fn observe(&mut self, id: u64, frame: &ScreenFrame) {
        let fence = Fence { id, epoch: frame.epoch, cols: frame.cols, rows: frame.rows };
        if self.pending.as_ref().is_some_and(|p| p.fence != fence)
            || self.frozen.as_ref().is_some_and(|f| f.fence != fence)
        {
            self.clear();
        }
        // A frozen snapshot owns its copied rows even when output evicts them
        // from the source. At live bottom, new output retires the old cache.
        if !self.active() && self.frozen.as_ref().is_some_and(|f| f.snapshot.revision != frame.revision) {
            self.frozen = None;
        }
    }
}

impl Frozen {
    fn navigate(&mut self, op: ScrollOp) {
        let rows = usize::from(self.fence.rows);
        let limit = self.snapshot.lines.len().saturating_sub(rows);
        let offset = moved(self.offset, op, self.fence.rows, limit);
        if offset == 0 {
            // Bottom always displays the separate live frame. In particular,
            // ordinary typing must not clone a cached viewport on every key.
            self.offset = 0;
            self.frame.offset = 0;
            self.frame.history = limit as u32;
            return;
        }
        if offset == self.offset {
            return;
        }
        self.offset = offset;
        let end = self.snapshot.lines.len().saturating_sub(self.offset);
        let start = end.saturating_sub(rows);
        self.frame = ScreenFrame {
            epoch: self.fence.epoch,
            revision: self.snapshot.revision,
            cols: self.fence.cols,
            rows: self.fence.rows,
            lines: self.snapshot.lines[start..end].to_vec(),
            cursor: None,
            modes: self.snapshot.modes,
            offset: self.offset as u32,
            history: limit as u32,
        };
    }
}

impl App {
    pub(super) fn history_active(&self) -> bool {
        self.history.active()
    }

    pub fn session_frame(&self, id: u64) -> Option<&ScreenFrame> {
        if let Some(frozen) = self.history.frozen.as_ref().filter(|f| f.fence.id == id && f.offset > 0) {
            return Some(&frozen.frame);
        }
        self.frame.as_ref().filter(|(fid, _)| *fid == id).map(|(_, frame)| frame)
    }

    pub fn history_label(&self) -> Option<String> {
        let frozen = self.history.frozen.as_ref().filter(|f| f.offset > 0)?;
        let extent = if frozen.snapshot.truncated {
            format!("{} retained of {} source lines", frozen.frame.history, frozen.snapshot.history)
        } else {
            format!("{} lines", frozen.frame.history)
        };
        Some(format!(" ↑ {} / {extent} · End live ", frozen.offset))
    }

    pub(super) fn history_key(&mut self, k: KeyEvent) -> bool {
        // Dot's conversation scrolls the same way, through its own rows.
        if self.mode == Mode::Normal && self.dot_native() {
            let op = match k.code {
                KeyCode::Home => ScrollOp::Top,
                KeyCode::End => ScrollOp::Bottom,
                _ => return false,
            };
            self.dot_scroll(op);
            return true;
        }
        if self.mode == Mode::Command || self.dot_native() || !self.current_session().is_some_and(|s| s.has_screen) {
            return false;
        }
        // A full-screen program (Claude's fullscreen transcript, less) keeps
        // its own scrollback, so its page keys reach it, from the list too, as
        // the wheel does. While typing, Ctrl+Home and Ctrl+End are its own.
        if !self.history_active() && self.current_modes() & M_ALT_SCREEN != 0 {
            if self.mode != Mode::Normal || !matches!(k.code, KeyCode::PageUp | KeyCode::PageDown) {
                return false;
            }
            let id = self.current_session().filter(|s| s.alive).map(|s| s.id);
            if let (Some(id), Some(data)) = (id, keys::encode_key(&k, self.current_modes())) {
                self.send(ClientMsg::Input { id, data });
            }
            return true;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let fast = k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL);
        let rows = self.frame.as_ref().map_or(24, |(_, f)| f.rows);
        let op = match k.code {
            KeyCode::PageUp if fast => Some(ScrollOp::Lines(i32::from(rows) * 10)),
            KeyCode::PageDown if fast => Some(ScrollOp::Lines(-i32::from(rows) * 10)),
            KeyCode::PageUp => Some(ScrollOp::PageUp),
            KeyCode::PageDown => Some(ScrollOp::PageDown),
            KeyCode::Home if ctrl || self.mode == Mode::Normal => Some(ScrollOp::Top),
            KeyCode::End if ctrl || self.mode == Mode::Normal || self.history_active() => Some(ScrollOp::Bottom),
            _ => None,
        };
        if let Some(op) = op {
            self.scroll_session(op);
            return true;
        }
        // Normal-mode commands keep their roles. Insert handles typing and
        // paste by returning live before forwarding the unchanged input.
        false
    }

    pub(super) fn scroll_session(&mut self, op: ScrollOp) {
        self.selection = None;
        if op == ScrollOp::Bottom {
            self.history.pending = None;
            if let Some(frozen) = self.history.frozen.as_mut() {
                frozen.navigate(op);
            }
            self.dirty = true;
            return;
        }
        let Some((id, frame)) = self.frame.as_ref().filter(|(id, _)| self.current == Some(Sel::Session(*id))) else {
            return;
        };
        let fence = Fence { id: *id, epoch: frame.epoch, cols: frame.cols, rows: frame.rows };
        let revision = frame.revision;
        let limit = frame.history as usize;
        self.history.observe(fence.id, frame);
        if let Some(frozen) = self.history.frozen.as_mut() {
            frozen.navigate(op);
            self.dirty = true;
            return;
        }
        if let Some(pending) = self.history.pending.as_mut() {
            pending.offset = moved(pending.offset, op, fence.rows, limit);
            return;
        }
        // A step toward live output, or a screen without history, has nothing
        // to fetch. The first step back fetches one snapshot; every later
        // step, wheel or key, moves within it here.
        let offset = moved(0, op, fence.rows, limit);
        if offset == 0 {
            return;
        }
        if self.service_version < HISTORY_PROTOCOL || fence.epoch == 0 {
            self.set_notice("Local history needs an updated coordinator and source host (protocol 18).", true);
            return;
        }
        self.history.serial = self.history.serial.wrapping_add(1);
        let request = self.history.serial;
        self.history.pending = Some(Pending { fence, request, revision, offset });
        self.send(ClientMsg::History { id: fence.id, request, epoch: fence.epoch, cols: fence.cols, rows: fence.rows });
    }

    pub(super) fn history_reply(&mut self, msg: ServerMsg) {
        let ServerMsg::History { id, request, epoch, cols, rows, snapshot, error } = msg else { return };
        let fence = Fence { id, epoch, cols, rows };
        let Some(pending) = self.history.pending.as_ref().filter(|p| p.fence == fence && p.request == request) else {
            return;
        };
        let offset = pending.offset;
        let revision = pending.revision;
        self.history.pending = None;
        if self.current != Some(Sel::Session(id))
            || !self
                .frame
                .as_ref()
                .is_some_and(|(fid, f)| *fid == id && (f.epoch, f.cols, f.rows) == (epoch, cols, rows))
        {
            return;
        }
        if let Some(error) = error {
            self.set_notice(error, true);
            return;
        }
        let Some(snapshot) = snapshot else { return };
        if snapshot.revision < revision
            || snapshot.lines.len() > MAX_HISTORY_LINES
            || !(1..=500).contains(&cols)
            || !(1..=300).contains(&rows)
            || serde_json::to_vec(&snapshot).map_or(true, |b| b.len() > MAX_HISTORY_BYTES)
        {
            return;
        }
        let mut frozen = Frozen { fence, snapshot, offset: 0, frame: ScreenFrame::default() };
        frozen.navigate(ScrollOp::Lines(offset.min(i32::MAX as usize) as i32));
        let too_short = frozen.frame.history == 0 && frozen.snapshot.truncated;
        let retained = frozen.snapshot.lines.len();
        let total = frozen.snapshot.history as usize + usize::from(rows);
        self.history.frozen = Some(frozen);
        if too_short {
            self.set_notice(
                format!("History retained {retained} of {total} rows; no older page fits the cache budget."),
                false,
            );
        }
        self.dirty = true;
    }
}
