//! The phone's Clear. Nothing is typed into the terminal: a fresh session starts the way the deck starts one
//! natively (Codex through its own interface, `native_new`), in the cleared one's directory, account and launch
//! model, and the cleared one closes once the fresh one is confirmed. Its conversation stays resumable, as
//! after `/clear`.
use super::*;
use crate::attention::ExpectedTarget;

pub(super) struct Clearing {
    client: u64,
    request: u64,
    id: u64,
    expected: ExpectedTarget,
    inputs: u64,
}

impl Service {
    /// Clear session `id` for the plugin request `request`; the reply, the
    /// fresh session's id or why nothing was cleared, follows once it is
    /// settled.
    pub(super) fn native_clear(&mut self, client: u64, request: u64, id: u64, expected: ExpectedTarget) -> Result<()> {
        let s = self.sessions.get(&id).context("no such local session")?;
        anyhow::ensure!(
            matches!(s.tool, Tool::Claude | Tool::Codex),
            "{} has no native clear; nothing was cleared",
            s.tool.label()
        );
        anyhow::ensure!(
            !matches!(s.tracker.status, Status::Working | Status::NeedsYou),
            "the session is busy; nothing was cleared"
        );
        anyhow::ensure!(
            self.native_clears.len() < 8 && self.native_clears.values().all(|clearing| clearing.id != id),
            "a clear is already pending for this session; nothing was cleared"
        );
        let entry = self.registry.sessions.iter().find(|entry| entry.id == id);
        let start = StartModel {
            model: entry.and_then(|entry| entry.start_model.clone()),
            effort: entry.and_then(|entry| entry.start_effort.clone()),
        };
        let place = Placement {
            cwd: s.cwd.clone(),
            account: s.account.clone(),
            account_root: s.account_root.clone(),
            ..Placement::default()
        };
        let clearing = Clearing { client, request, id, expected, inputs: s.inputs };
        if s.tool == Tool::Codex {
            let bytes = crate::remote::auth::random_bytes(8)?;
            let token = u64::from_le_bytes(bytes.try_into().unwrap());
            anyhow::ensure!(!self.native_clears.contains_key(&token), "clear identity allocation collided");
            self.native_clears.insert(token, clearing);
            self.start_native_new(client, Tool::Codex, start, place, Some(token));
            return Ok(());
        }
        let fresh = self
            .create_with(Tool::Claude, None, &start, &place)
            .context("could not start the fresh session; nothing was cleared")?;
        self.finish_native_clear(clearing, fresh);
        Ok(())
    }

    fn take_native_clear(&mut self, client: u64, request: Option<u64>) -> Option<Clearing> {
        let token = request?;
        self.native_clears.get(&token).filter(|clearing| clearing.client == client)?;
        self.native_clears.remove(&token)
    }

    /// Whether the native New `request` was a clear's; its failure is then
    /// the clear's reply.
    pub(super) fn native_clear_failed(&mut self, client: u64, request: Option<u64>, error: &str) -> bool {
        let Some(clearing) = self.take_native_clear(client, request) else { return false };
        self.plugin_reply(
            clearing.client,
            clearing.request,
            Value::Null,
            Some(format!("{error}; nothing was cleared")),
        );
        true
    }

    /// A failed native New: the clear's reply when it was one, a notice
    /// otherwise.
    pub(super) fn native_new_failed(&mut self, client: u64, request: Option<u64>, error: String) {
        if !self.native_clear_failed(client, request, &error) {
            let error = if error.starts_with("Could not start") {
                error
            } else {
                format!("Could not start native session: {error}")
            };
            self.notice(client, error, true);
        }
    }

    /// Whether the confirmed native New `request` was a clear's, which it
    /// then completes.
    pub(super) fn native_clear_created(&mut self, client: u64, request: Option<u64>, fresh: u64) -> bool {
        let Some(clearing) = self.take_native_clear(client, request) else { return false };
        self.finish_native_clear(clearing, fresh);
        true
    }

    fn finish_native_clear(&mut self, clearing: Clearing, fresh: u64) {
        let unchanged = self.sessions.get(&clearing.id).is_some_and(|s| {
            extensions::native::provider_target::matches(&clearing.expected, s)
                && s.inputs == clearing.inputs
                && !matches!(s.tracker.status, Status::Working | Status::NeedsYou)
        });
        if !unchanged {
            self.close_and_remove(fresh);
            self.plugin_reply(
                clearing.client,
                clearing.request,
                Value::Null,
                Some("the session changed while clearing; nothing was cleared".into()),
            );
            return;
        }
        self.close_and_remove(clearing.id);
        self.plugin_target_reply(clearing.client, clearing.request, serde_json::json!({ "id": fresh }), None);
    }
}

#[cfg(test)]
#[path = "../tests/native_clear.rs"]
mod tests;
