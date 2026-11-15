use super::*;
impl App {
    /// `:scan [host] [--scope TEXT]`.
    pub(crate) fn scan_command(&mut self, mut words: Vec<String>) -> Result<(), String> {
        let scope = actions::take_value(&mut words, "--scope")?;
        let host = match words.first() {
            Some(word) => self.resolve_host(word)?,
            None => None,
        };
        if !self.service_has(9, "The security scan") {
            return Ok(());
        }
        let on = host.as_deref().map(|h| format!(" on {}", self.host_label(Some(h)))).unwrap_or_default();
        self.send(ClientMsg::SecurityScan { host, scope });
        self.set_notice(format!("Preparing a scratch worktree for the security scan{on}…"), false);
        Ok(())
    }

    /// Enter on a scan: its session while it runs, else its report in a
    /// terminal on its host.
    pub(crate) fn open_scan(&mut self, sel: usize) {
        let Some(scan) = self.state.scans.get(sel).cloned() else { return };
        if let Some(id) = scan.session.filter(|id| self.session(*id).is_some_and(|s| s.alive)) {
            self.select(Sel::Session(id));
            return;
        }
        let Some(report) = scan.report.clone() else {
            self.set_notice(format!("Scan {} has no report: {}.", scan.id, scan.state), true);
            return;
        };
        let pager = vec!["less".to_string(), "-R".to_string(), "--".to_string(), report];
        let name = Some(format!("scan {}", scan.id));
        self.start_session(Tool::Shell, scan.host.clone(), Some(scan.dir.clone()), name, pager, None);
    }
}
