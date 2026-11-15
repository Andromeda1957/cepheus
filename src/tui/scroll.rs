//! Vertical wheel gain shared by the session pane and the menus.

use std::time::{Duration, Instant};

/// Rows one wheel report moves a view the deck scrolls itself (session
/// history, Help and other text readers), as terminal multiplexers scroll.
/// kitty sends one report per wheel notch, or per row of touchpad travel,
/// while the deck has the mouse.
pub(super) const WHEEL_ROWS: usize = 3;

/// The gesture target of whichever menu is open; sessions use their ids.
pub(super) const MENU: u64 = u64::MAX;

/// Keep isolated steps precise; a sustained swipe ramps up without an
/// unbounded flood of reports to the hosted program.
#[derive(Default)]
pub(super) struct ScrollGesture {
    last: Option<(u64, bool, Instant)>,
    multiplier: usize,
}

impl ScrollGesture {
    pub(super) fn multiplier(&mut self, id: u64, up: bool, now: Instant) -> usize {
        let continuing = self.last.is_some_and(|(previous_id, previous_up, at)| {
            previous_id == id && previous_up == up && now.saturating_duration_since(at) <= Duration::from_millis(80)
        });
        self.multiplier = if continuing { (self.multiplier + 1).min(4) } else { 1 };
        self.last = Some((id, up, now));
        self.multiplier
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
}
