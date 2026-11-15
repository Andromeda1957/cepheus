use super::*;
impl Service {
    /// The dev server's command, its log and the port it listens on.
    pub(crate) fn dev_command(&self) -> (Vec<String>, PathBuf, Option<u16>) {
        let argv = self.opts.config.dev_server_command.clone();
        let port = devserver::port_of(&argv);
        (argv, self.opts.state_dir.join("dev-server.log"), port)
    }

    /// Start or stop the dev server from the deck. A server the deck did not
    /// start is never touched: one already on the port is reported instead.
    pub(crate) fn toggle_dev_server(&mut self, cid: u64, run: bool) {
        if self.shutdown_at.is_some() || !self.opts.config.plugin_enabled("dev-services") {
            self.notice(cid, "Plugin dev-services is disabled", true);
            return;
        }
        let (argv, log, port) = self.dev_command();
        let shown = argv.join(" ");
        if !run {
            match self.native.dev.as_mut() {
                Some(dev) => {
                    dev.stop();
                    self.notice(cid, "Stopping the dev server…", false);
                }
                None if self.native.dev_port_busy => self.notice(
                    cid,
                    format!(
                        "Port {} belongs to a server the deck did not start; stop that one where it runs.",
                        port.unwrap_or_default()
                    ),
                    true,
                ),
                None => self.notice(cid, "The dev server is not running.", false),
            }
            return;
        }
        if self.native.dev.is_some() {
            self.notice(cid, "The dev server is already running.", false);
            return;
        }
        if let Some(port) = port.filter(|p| devserver::port_listening(*p)) {
            self.native.dev_port_busy = true;
            self.announce_error(format!("Dev server not started: something already listens on port {port}."));
            return;
        }
        let env_remove = child_env_remove(std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()));
        match DevProc::start(&argv, &self.opts.start_dir, &log, &env_remove) {
            Ok(dev) => {
                self.notice(cid, format!("Dev server started: {shown}"), false);
                self.native.dev = Some(dev);
            }
            Err(err) => self.announce_error(format!("Dev server failed to start: {err:#}")),
        }
        self.last_status = Instant::now() - STATUS_INTERVAL;
    }

    /// Reap a dev server that ended and see whether another program holds
    /// its port.
    pub(crate) fn watch_dev_server(&mut self, now: Instant) {
        if let Some(ended) = self.native.dev.as_mut().and_then(|d| d.poll(now)) {
            self.native.dev = None;
            match ended {
                Ended::Stopped => {
                    self.broadcast(&ServerMsg::Notice { text: "Dev server stopped.".into(), error: false })
                }
                Ended::Failed { status, ran, tail } => {
                    let how = match status.code() {
                        Some(code) => format!("exit {code}"),
                        None => "killed by a signal".to_string(),
                    };
                    let last = tail.lines().last().unwrap_or("no output");
                    let text =
                        format!("Dev server failed ({how} after {}): {last}", util::short_duration(ran.as_secs()));
                    self.log_error(&format!("{text}\n{tail}"));
                    self.broadcast(&ServerMsg::Notice { text: format!("{text} · F2 shows its output"), error: true });
                }
            }
            self.native.dev_port_checked = None;
        }
        if self.native.dev.is_some() {
            self.native.dev_port_busy = false;
            return;
        }
        if !self.opts.config.plugin_enabled("dev-services") {
            return;
        }
        if self.native.dev_port_checked.is_some_and(|at| now.duration_since(at) < PORT_CHECK_INTERVAL) {
            return;
        }
        self.native.dev_port_checked = Some(now);
        self.native.dev_port_busy = self.dev_command().2.is_some_and(devserver::port_listening);
    }

    pub(crate) fn dev_server_state(&self) -> DevServer {
        if !self.opts.config.plugin_enabled("dev-services") {
            return DevServer::default();
        }
        let (argv, _, port) = self.dev_command();
        DevServer {
            command: argv.join(" "),
            port,
            pid: self.native.dev.as_ref().filter(|d| !d.stopping()).map(|d| d.pid),
            started_at: self.native.dev.as_ref().map(|d| d.started_at),
            port_busy: self.native.dev.is_none() && self.native.dev_port_busy,
        }
    }
}
