use super::*;

/// Startup upkeep belongs in the service log. A requested sync gets a reply;
/// only failed operations enter the persistent error history.
fn skills_notice(client: Option<u64>, result: anyhow::Result<crate::skills::SyncReport>) -> Option<Ev> {
    let (text, error) = match result {
        Ok(report) => {
            let summary = report.summary();
            eprintln!("cepheus: {summary}");
            if report.problems.is_empty() {
                client?;
                (summary, false)
            } else {
                (format!("Skills: {}", report.problems.join("; ")), true)
            }
        }
        Err(err) => (format!("Could not link the deck's skills: {err:#}"), true),
    };
    Some(Ev::Notice { client, text, error })
}

impl Service {
    /// Install what a deck asked for (`installer.rs`) on a thread of its
    /// own; the report goes to that deck.
    pub(crate) fn start_install(&mut self, cid: u64, request: crate::protocol::InstallRequest) {
        if self.shutdown_at.is_some() || !self.opts.config.plugin_enabled("agent-extensions") {
            self.notice(cid, "Plugin agent-extensions is disabled", true);
            return;
        }
        if self.native.installs >= 2 {
            self.notice(cid, "Two installs are running here already; try again when they are done.", true);
            return;
        }
        let tx = self.tx.clone();
        let cancel = self.extensions.cancel.clone();
        let home = paths::home();
        let started = thread::Builder::new().name("install".into()).spawn(move || {
            crate::run::with_cancellation(cancel, || {
                let report = crate::installer::install(&request, &home);
                let _ = tx.send(Ev::Installed { client: cid, ok: report.ok, lines: report.lines });
            })
        });
        match started {
            Ok(_) => {
                self.native.installs += 1;
                self.notice(cid, "Installing…", false);
            }
            Err(err) => self.notice(cid, format!("Could not start the install: {err}"), true),
        }
    }

    /// Link the deck's bundled skills into each agent's skills folder again
    /// (`skills.rs`), on a thread of its own.
    pub(crate) fn sync_skills(&mut self, cid: Option<u64>) {
        if self.shutdown_at.is_some() || !self.opts.config.plugin_enabled("agent-extensions") {
            return;
        }
        let home = paths::home();
        let data = paths::data_dir();
        let tx = self.tx.clone();
        let work = crate::extensions::WorkGuard::new(self.extensions.workers.clone());
        let _ = thread::Builder::new().name("skills".into()).spawn(move || {
            let _work = work;
            if let Some(event) = skills_notice(cid, crate::skills::sync(&data, &home)) {
                let _ = tx.send(event);
            }
        });
    }
}

#[cfg(test)]
#[path = "../../src/tests/skill_sync.rs"]
mod tests;
