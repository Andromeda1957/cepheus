use std::collections::VecDeque;

use super::*;

/// A session whose screen reads follow a script, the last read repeating;
/// `None` is a read that fails.
struct Scripted {
    screens: VecDeque<Option<Vec<String>>>,
}

impl SessionIo for Scripted {
    fn screen(&mut self, _: u64) -> Result<(Vec<String>, u64)> {
        let shot = if self.screens.len() > 1 { self.screens.pop_front().flatten() } else { self.screens[0].clone() };
        shot.map(|lines| (lines, 0)).ok_or_else(|| anyhow::anyhow!("the screen could not be read"))
    }
    fn input(&mut self, _: u64, _: &str) -> Result<()> {
        Ok(())
    }
    fn type_message(&mut self, _: u64, _: &str) -> Result<()> {
        Ok(())
    }
}

fn session(after_model: Option<Vec<String>>) -> Scripted {
    let models = ["Select Model", "", "› 1. gpt-6.1-sol (current)", "  2. gpt-6.1-mini", ""];
    Scripted { screens: VecDeque::from([Some(models.map(String::from).to_vec()), after_model]) }
}

#[test]
fn an_effort_asked_for_is_chosen_only_when_its_list_was_seen() {
    // Audit OPS-P14501: once the model was pressed, a failed read or an
    // effort list that never appeared was taken as success.
    let unread = pick(&mut session(None), 7, Some("gpt-6.1-mini"), Some("high"));
    assert!(unread.is_err(), "a screen that could not be read chose high effort");

    let closed = Some(vec!["› ".to_string()]);
    let unseen = pick(&mut session(closed.clone()), 7, Some("gpt-6.1-mini"), Some("high"));
    assert!(unseen.is_err(), "no effort list was seen, yet high effort was chosen");

    // A model without effort levels completes a choice of the model alone.
    assert!(pick(&mut session(closed), 7, Some("gpt-6.1-mini"), None).is_ok());
}
