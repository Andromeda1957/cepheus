//! Navigation belongs to the open menu, including wheel events over the pane.

use std::mem::{discriminant, Discriminant};
use std::time::Instant;

use super::{choose_matches, choose_snap, new_tools, scroll, App, Dialog, KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone)]
pub struct DialogViewport {
    kind: Option<Discriminant<Dialog>>,
    pub offset: usize,
    pub limit: usize,
    pub page: usize,
    selected: Option<usize>,
    /// Search is absent until `/` is pressed in the current window.
    pub search: Option<String>,
    /// The quick-question answer Ctrl-y copies, marked while scrolled up.
    pub pick: Option<usize>,
}

impl Default for DialogViewport {
    fn default() -> Self {
        Self { kind: None, offset: 0, limit: usize::MAX, page: 10, selected: None, search: None, pick: None }
    }
}

impl DialogViewport {
    pub fn begin(&mut self, dialog: &Dialog) {
        let kind = Some(discriminant(dialog));
        if self.kind != kind {
            *self = Self { kind, ..Self::default() };
        }
    }

    pub fn measure(&mut self, lines: usize, height: usize) {
        if self.page != height.max(1) {
            self.selected = None;
        }
        self.page = height.max(1);
        self.limit = lines.saturating_sub(self.page);
        self.offset = self.offset.min(self.limit);
    }

    /// Follow a changed selection, while allowing the same menu to be read
    /// with page keys without snapping back on every redraw.
    pub fn follow(&mut self, line: usize) {
        if self.selected != Some(line) {
            if line < self.offset {
                self.offset = line;
            } else if line >= self.offset.saturating_add(self.page) {
                self.offset = line.saturating_sub(self.page - 1).min(self.limit);
            }
            self.selected = Some(line);
        }
    }
}

#[derive(Clone, Copy)]
enum Navigation {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
}

impl Navigation {
    fn apply(self, value: usize, last: usize, page: usize) -> usize {
        let value = value.min(last);
        match self {
            Self::Up => value.saturating_sub(1),
            Self::Down => value.saturating_add(1).min(last),
            Self::PageUp => value.saturating_sub(page),
            Self::PageDown => value.saturating_add(page).min(last),
            Self::First => 0,
            Self::Last => last,
        }
    }

    fn reversed(self) -> Self {
        match self {
            Self::Up => Self::Down,
            Self::Down => Self::Up,
            Self::PageUp => Self::PageDown,
            Self::PageDown => Self::PageUp,
            Self::First => Self::Last,
            Self::Last => Self::First,
        }
    }
}

/// A text reader scrolls rows like the session pane; a menu moves its
/// selection one entry per report.
fn wheel_rows(dialog: &Dialog) -> usize {
    match dialog {
        Dialog::Help
        | Dialog::Machines
        | Dialog::Usage
        | Dialog::Confirm { .. }
        | Dialog::Errors { .. }
        | Dialog::Report { .. }
        | Dialog::Ask { .. } => scroll::WHEEL_ROWS,
        Dialog::Choose { pick, .. } if !pick.items.iter().any(|item| item.kind == crate::lua::ItemKind::Choice) => {
            scroll::WHEEL_ROWS
        }
        _ => 1,
    }
}

impl App {
    /// The wheel navigates the open menu as its arrows do, with the session
    /// pane's gain: precise single reports, faster sustained gestures.
    pub(super) fn wheel_dialog(&mut self, up: bool, modifiers: KeyModifiers) {
        let Some(dialog) = &self.dialog else { return };
        let rows = wheel_rows(dialog);
        let gain = if modifiers.is_empty() {
            self.wheel_scroll.multiplier(scroll::MENU, up, Instant::now())
        } else {
            self.wheel_scroll.reset();
            1
        };
        // ↑ and ↓ bring back questions in the quick-question box; the
        // wheel scrolls its answers.
        if let Some(Dialog::Ask { scroll, .. }) = &mut self.dialog {
            let rows = rows * gain;
            *scroll = if up {
                scroll.saturating_add(rows).min(self.dialog_viewport.limit)
            } else {
                scroll.saturating_sub(rows)
            };
            return;
        }
        let key = KeyEvent::new(if up { KeyCode::Up } else { KeyCode::Down }, KeyModifiers::NONE);
        for _ in 0..rows * gain {
            self.dialog_key(key);
        }
    }

    /// Search owns text input only after `/`; ordinary menu keys keep their actions.
    pub(super) fn search_dialog_key(&mut self, dialog: &mut Dialog, k: KeyEvent) -> bool {
        if !matches!(
            dialog,
            Dialog::Choose { .. }
                | Dialog::Picker { .. }
                | Dialog::Branches { .. }
                | Dialog::Help
                | Dialog::Machines
                | Dialog::Errors { .. }
                | Dialog::Report { confirm: None, .. }
        ) {
            return false;
        }
        let search = &mut self.dialog_viewport.search;
        let text_key = !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        match k.code {
            KeyCode::Char('/') if search.is_none() && text_key => *search = Some(String::new()),
            KeyCode::Esc if search.is_some() => *search = None,
            KeyCode::Backspace if search.is_some() => {
                search.as_mut().unwrap().pop();
            }
            KeyCode::Char('u') if search.is_some() && k.modifiers.contains(KeyModifiers::CONTROL) => {
                search.as_mut().unwrap().clear();
            }
            KeyCode::Char(c) if search.is_some() && text_key => search.as_mut().unwrap().push(c),
            _ => return false,
        }
        self.apply_dialog_search(dialog);
        true
    }

    pub(super) fn apply_dialog_search(&mut self, dialog: &mut Dialog) {
        let query = self.dialog_viewport.search.as_deref().unwrap_or_default();
        match dialog {
            Dialog::Choose { pick, sel, filter, .. } => {
                *filter = query.to_string();
                *sel = choose_snap(&pick.items, &choose_matches(&pick.items, filter), 0, true);
            }
            Dialog::Picker { sel, filter, .. } | Dialog::Branches { sel, filter } => {
                let changed = *filter != query;
                *filter = query.to_string();
                *sel = 0;
                if changed {
                    App::picker_filter_changed(dialog);
                }
            }
            Dialog::Errors { scroll } | Dialog::Report { scroll, .. } => *scroll = 0,
            _ => {}
        }
        self.dialog_viewport.offset = 0;
        self.dialog_viewport.selected = None;
    }

    pub(super) fn navigate_dialog(&mut self, dialog: &mut Dialog, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Dialog::Ask { scroll, .. } = dialog {
            // The other keys edit the question; ↑ and ↓ bring back earlier ones.
            let nav = match k.code {
                KeyCode::PageUp => Navigation::PageUp,
                KeyCode::PageDown => Navigation::PageDown,
                KeyCode::Home if ctrl => Navigation::First,
                KeyCode::End if ctrl => Navigation::Last,
                _ => return false,
            };
            let viewport = &self.dialog_viewport;
            *scroll = nav.reversed().apply(*scroll, viewport.limit, viewport.page);
            return true;
        }
        let editing = match dialog {
            Dialog::Broadcast { .. } => true,
            _ => self.dialog_viewport.search.is_some(),
        };
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let vim = !editing || alt || ctrl;
        let nav = match k.code {
            KeyCode::Up => Navigation::Up,
            KeyCode::Down => Navigation::Down,
            KeyCode::PageUp => Navigation::PageUp,
            KeyCode::PageDown => Navigation::PageDown,
            KeyCode::Home => Navigation::First,
            KeyCode::End => Navigation::Last,
            KeyCode::Char('k') if vim => Navigation::Up,
            // Ctrl-j is an existing newline in the broadcast composer.
            KeyCode::Char('j') if vim && !(ctrl && matches!(dialog, Dialog::Broadcast { .. })) => Navigation::Down,
            KeyCode::Char('u' | 'b') if ctrl && !editing => Navigation::PageUp,
            KeyCode::Char('d' | 'f') if ctrl && !editing => Navigation::PageDown,
            _ => return false,
        };
        let task_rows = match dialog {
            Dialog::WorkQueue { tab: super::work_queue::Tab::Tasks, .. } => self.source_rows().len(),
            _ => 0,
        };
        let viewport = &mut self.dialog_viewport;
        let move_to = |value: &mut usize, last| *value = nav.apply(*value, last, viewport.page);
        match dialog {
            Dialog::AttentionSend => return false,
            Dialog::Attention { tab, sel } => {
                if self.attention.reader.is_some() {
                    viewport.offset = nav.apply(viewport.offset, viewport.limit, viewport.page);
                    return true;
                }
                let previous = *sel;
                let count = match tab {
                    super::attention::Tab::Messages => self.state.attention.messages.len(),
                    super::attention::Tab::Supervisor => 0,
                };
                if count == 0 {
                    viewport.offset = nav.apply(viewport.offset, viewport.limit, viewport.page);
                } else if *sel == usize::MAX {
                    *sel = if matches!(nav, Navigation::Last) { count - 1 } else { 0 };
                } else {
                    move_to(sel, count - 1);
                }
                if *sel != previous {
                    self.attention.private = None;
                    self.attention.generation = self.attention.generation.wrapping_add(1);
                }
            }
            Dialog::Help | Dialog::Machines | Dialog::Usage | Dialog::Confirm { .. } => {
                viewport.offset = nav.apply(viewport.offset, viewport.limit, viewport.page);
            }
            Dialog::New { sel, model, effort } => {
                let previous = *sel;
                move_to(sel, new_tools(&self.cfg).len().saturating_sub(1));
                if *sel != previous {
                    *model = 0;
                    *effort = 0;
                }
            }
            Dialog::Pair(pairing) => {
                move_to(&mut pairing.sel, pairing.devices.len().saturating_sub(1));
                viewport.offset = nav.apply(viewport.offset, viewport.limit, viewport.page);
            }
            Dialog::Errors { scroll } | Dialog::Report { scroll, .. } => move_to(scroll, viewport.limit),
            Dialog::Ask { .. } => return false,
            Dialog::Picker { items, sel, filter, .. } => {
                move_to(sel, App::picker_matches(items, filter).len().saturating_sub(1));
            }
            Dialog::Choose { pick, sel, filter, .. } => {
                let matches = choose_matches(&pick.items, filter);
                if !matches.iter().any(|&i| pick.items[i].kind == crate::lua::ItemKind::Choice) {
                    // Nothing to choose: the keys scroll the panel.
                    *sel = nav.apply(*sel, viewport.limit, viewport.page);
                } else {
                    let target = nav.apply(*sel, matches.len().saturating_sub(1), viewport.page);
                    let down = matches!(nav, Navigation::Down | Navigation::PageDown | Navigation::First);
                    *sel = choose_snap(&pick.items, &matches, target, down);
                }
            }
            Dialog::Branches { sel, filter } => {
                let count = self.branch_rows(filter).len();
                *sel = nav.apply(*sel, count.saturating_sub(1), self.dialog_viewport.page);
            }
            Dialog::Broadcast { targets, sel, .. } => move_to(sel, targets.len().saturating_sub(1)),
            Dialog::Scans { sel } => move_to(sel, self.state.scans.len().saturating_sub(1)),
            Dialog::WorkQueue { tab, sel } => {
                let count = match tab {
                    super::work_queue::Tab::Queue => self.state.work_queue.items.len(),
                    super::work_queue::Tab::Tasks => task_rows,
                    super::work_queue::Tab::Chores => self.state.work_queue.chores.len(),
                };
                if *sel == usize::MAX {
                    *sel = if matches!(nav, Navigation::Last) { count.saturating_sub(1) } else { 0 };
                } else {
                    move_to(sel, count.saturating_sub(1));
                }
            }
        }
        true
    }
}
