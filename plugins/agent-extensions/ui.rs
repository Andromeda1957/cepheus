use super::*;
impl App {
    /// `:install` and `:uninstall`: show what it would do on each host,
    /// then do it on `y` (or at once with `--yes`).
    pub(crate) fn install_command(&mut self, mut words: Vec<String>, remove: bool) -> Result<(), String> {
        let agents = actions::take_value(&mut words, "--agents")?;
        let host = actions::take_value(&mut words, "--host")?;
        let dry_run = actions::take_switch(&mut words, "--dry-run");
        let yes = actions::take_switch(&mut words, "--yes");
        let source = match words.as_slice() {
            [source] => source.clone(),
            [] => {
                return Err("give what to install: owner/repo, a URL, a folder, npm:package or mcp:name=command".into())
            }
            [_, extra, ..] => return Err(format!("{extra:?} is one word too many")),
        };
        let agents: Vec<String> = agents
            .map(|a| a.split(',').map(|w| w.trim().to_string()).filter(|w| !w.is_empty()).collect())
            .unwrap_or_default();
        if let Some(bad) = agents.iter().find(|a| !crate::installer::AGENTS.contains(&a.as_str())) {
            return Err(format!("{bad:?} is not claude, codex or agy"));
        }
        let hosts: Vec<Option<String>> = match host.as_deref() {
            None => vec![None],
            Some("all") => self.start_hosts(),
            Some(word) => vec![self.resolve_host(word)?],
        };
        if !self.service_has(9, "Installing for the agents") {
            return Ok(());
        }
        let request =
            |dry_run: bool| InstallRequest { source: source.clone(), agents: agents.clone(), dry_run, remove };
        let msgs = |dry_run: bool| -> Vec<ClientMsg> {
            hosts.iter().map(|host| ClientMsg::Install { request: request(dry_run), host: host.clone() }).collect()
        };
        let verb = if remove { "Remove" } else { "Install" };
        let title = format!("{verb} {source}");
        let (send, confirm, first) = if dry_run || yes {
            (msgs(dry_run), None, if dry_run { "What it would do:" } else { "Working…" })
        } else {
            (msgs(true), Some(msgs(false)), "Reading what it would do…")
        };
        let waiting = send.len();
        for msg in send {
            self.send(msg);
        }
        self.dialog = Some(Dialog::Report { title, lines: vec![first.to_string()], scroll: 0, confirm, waiting });
        Ok(())
    }

    /// A host's install report arrived.
    pub fn installed(&mut self, host: String, ok: bool, lines: Vec<String>) {
        let mark = if ok { "✓" } else { "✗" };
        match &mut self.dialog {
            Some(Dialog::Report { lines: shown, waiting, .. }) => {
                *waiting = waiting.saturating_sub(1);
                shown.push(String::new());
                shown.push(format!("{mark} {host}"));
                shown.extend(lines.into_iter().map(|l| format!("  {l}")));
            }
            _ => {
                let last = lines.last().cloned().unwrap_or_default();
                self.set_notice(format!("{mark} {host}: {last}"), !ok);
            }
        }
    }

    /// `:skills` lists the deck's skills here; `:skills sync` links them on
    /// every host.
    pub(crate) fn skills_command(&mut self, rest: &str) -> Result<(), String> {
        match rest {
            "sync" => {
                if self.service_has(9, "Syncing the skills") {
                    self.send(ClientMsg::SyncSkills);
                    self.set_notice("Linking the deck's skills on every host…", false);
                }
                Ok(())
            }
            "" => {
                let mut lines = Vec::new();
                for s in crate::skills::list(&paths::data_dir(), &paths::home()) {
                    let linked = if s.linked.is_empty() { "not linked".to_string() } else { s.linked.join(", ") };
                    lines.push(format!("{}  ({linked})", s.name));
                    lines.push(format!("  {}", util::truncate_width(&s.description, 100)));
                    lines.push(format!("  from {}", s.source));
                }
                if lines.is_empty() {
                    lines.push("No skills yet: :skills sync writes the deck's own.".into());
                }
                self.dialog =
                    Some(Dialog::Report { title: "Skills".into(), lines, scroll: 0, confirm: None, waiting: 0 });
                Ok(())
            }
            other => Err(format!("{other:?} is not sync")),
        }
    }
}
