//! The identity a freshly created native Codex session must keep while its
//! settings are confirmed. Native Codex settings stay on its shared server;
//! never replace that server with CLI overrides.
use super::*;

pub(super) struct PlacedTarget {
    pub nonce: u64,
    pub owner: u64,
    pub expected: crate::attention::ExpectedTarget,
    tool: Tool,
    process: Arc<Mutex<crate::session::Emu>>,
}

impl PlacedTarget {
    pub(super) fn new(session: &Sess, nonce: u64, owner: u64) -> Result<Self> {
        let hosted = session.hosted.as_ref().context("placed target is not hosted")?;
        Ok(Self {
            nonce,
            owner,
            expected: crate::attention::ExpectedTarget {
                spawned_ms: session.spawned_ms,
                pid: hosted.pid,
                conversation: session.conversation.clone(),
            },
            tool: session.tool,
            process: hosted.emu.clone(),
        })
    }

    pub(super) fn matches(&self, session: &Sess, nonce: u64) -> bool {
        self.nonce == nonce
            && session.tool == self.tool
            && extensions::native::provider_target::matches(&self.expected, session)
            && session.hosted.as_ref().is_some_and(|hosted| Arc::ptr_eq(&hosted.emu, &self.process))
    }
}
