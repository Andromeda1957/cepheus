//! Clear events are shared state, not a local view filter. Each service has
//! its own counter so concurrent clears and clocks on other hosts cannot
//! undo one another. Persist them before publishing the cleared state.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use super::Service;
use crate::{remote::auth, util};

const FILE: &str = "error-clears.json";
type Clears = BTreeMap<String, u64>;

fn valid(clears: &Clears) -> bool {
    clears.len() <= 1024 && clears.keys().all(|key| key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(super) fn includes(clears: &Clears, previous: &Clears) -> bool {
    previous.iter().all(|(origin, count)| clears.get(origin).copied().unwrap_or(0) >= *count)
}

pub(super) fn load(dir: &Path) -> Clears {
    let mut bytes = Vec::new();
    let loaded = std::fs::File::open(dir.join(FILE))
        .and_then(|file| file.take(128 * 1024).read_to_end(&mut bytes))
        .ok()
        .and_then(|_| serde_json::from_slice::<Clears>(&bytes).ok());
    loaded.filter(valid).unwrap_or_default()
}

impl Service {
    pub(super) fn clear_errors(&mut self, incoming: Option<Clears>) -> bool {
        let mut clears = self.error_clears.clone();
        if let Some(incoming) = incoming {
            if !valid(&incoming) {
                return false;
            }
            for (origin, count) in incoming {
                if count == 0 {
                    continue;
                }
                let current = clears.entry(origin).or_default();
                *current = (*current).max(count);
            }
            if clears == self.error_clears {
                return true;
            }
        } else {
            let origin = auth::sha256_hex(format!("{}:{}", util::hostname(), self.opts.state_dir.display()).as_bytes());
            let count = clears.entry(origin).or_default();
            *count = count.saturating_add(1);
        }
        if !valid(&clears) {
            return false;
        }
        let saved = serde_json::to_vec(&clears)
            .map_err(std::io::Error::other)
            .and_then(|bytes| util::write_atomic(&self.opts.state_dir.join(FILE), &bytes));
        if let Err(error) = saved {
            self.announce_error(format!("Could not save cleared errors: {error}"));
            return false;
        }
        self.error_clears = clears;
        self.errors.borrow_mut().clear();
        self.clear_cached_host_errors();
        // Publish before any later error alert. Gateways can then retire old
        // notifications without deleting a genuinely new error's notification.
        self.state_changed();
        true
    }

    /// Take back listed errors that no longer hold, unlike a clear, which
    /// dismisses everything listed: every deck's list drops them, and a
    /// phone that connects later is not alerted to them.
    pub(super) fn withdraw_errors(&mut self, texts: &[String]) {
        if texts.is_empty() {
            return;
        }
        let before = self.errors.borrow().len();
        self.errors.borrow_mut().retain(|entry| !texts.contains(&entry.text));
        if self.errors.borrow().len() != before {
            self.state_changed();
        }
    }
}

#[cfg(test)]
#[path = "../tests/clear_errors.rs"]
mod tests;
