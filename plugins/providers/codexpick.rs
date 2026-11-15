//! Choosing a model and reasoning effort in Codex's `/model` picker. Codex
//! takes neither as a start flag (`service::model_flags`), so a new session
//! gets them by typing `/model` and pressing through its two lists: for a
//! phone's new session (`remote`) and the deck's New dialog alike.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::prompt::{self, Prompt};
use crate::remote::models;

/// The pause between keys pressed in a list.
pub const KEY_GAP: Duration = Duration::from_millis(150);

/// What the picker needs of a connection to the service.
pub trait SessionIo {
    /// The plain text of a session's screen, and the inputs it has had.
    fn screen(&mut self, id: u64) -> Result<(Vec<String>, u64)>;
    /// Send keys to a session as they are.
    fn input(&mut self, id: u64, data: &str) -> Result<()>;
    /// Type a message into a session and submit it, as the deck's broadcast does.
    fn type_message(&mut self, id: u64, text: &str) -> Result<()>;
}

/// Choose `model` and `effort` (each the one Codex marks when absent) in a
/// session's `/model` picker: its list of models, then of efforts for the
/// chosen one.
pub fn pick(io: &mut impl SessionIo, id: u64, model: Option<&str>, effort: Option<&str>) -> Result<()> {
    io.type_message(id, "/model")?;
    let models = wait_list(io, id, None)?.ok_or_else(|| anyhow::anyhow!("no model list appeared"))?;
    let target = match model {
        Some(m) => models.options.iter().position(|o| models::option_matches(&o.label, m)),
        None => Some(models.selected),
    };
    let Some(target) = target else {
        let _ = io.input(id, "\x1b");
        bail!("Codex's /model list has no {}", model.unwrap_or(""));
    };
    press(io, id, &models, target)?;
    // No second list is a model without effort levels, which completes a
    // choice of the model alone. A screen that could not be read is an
    // error, and an effort asked for is chosen only from its list (audit
    // OPS-P14501: both used to be reported as chosen).
    let Some(efforts) = wait_list(io, id, Some(&models.fingerprint()))? else {
        if let Some(effort) = effort {
            bail!("Codex showed no effort list for that model, so {effort} effort was not chosen");
        }
        return Ok(());
    };
    let target = match effort {
        Some(e) => efforts.options.iter().position(|o| models::option_matches(&o.label, e)),
        None => Some(efforts.selected),
    };
    let Some(target) = target else {
        let _ = io.input(id, "\x1b");
        bail!("Codex offers no {} effort for that model", effort.unwrap_or(""));
    };
    press(io, id, &efforts, target)
}

/// The numbered list a session shows, other than the one `not`; `None`
/// when none appears in time. A screen that cannot be read is an error.
fn wait_list(io: &mut impl SessionIo, id: u64, not: Option<&str>) -> Result<Option<Prompt>> {
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(300));
        let (lines, _) = io.screen(id)?;
        if let Some(list) = prompt::options(&lines) {
            if not != Some(list.fingerprint().as_str()) {
                return Ok(Some(list));
            }
        }
    }
    Ok(None)
}

fn press(io: &mut impl SessionIo, id: u64, list: &Prompt, choice: usize) -> Result<()> {
    let modes = 0;
    for key in list.keys(choice, None, modes)? {
        io.input(id, &key)?;
        thread::sleep(KEY_GAP);
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../src/tests/codexpick.rs"]
mod tests;
