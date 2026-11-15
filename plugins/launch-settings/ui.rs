//! The deck's side of the launch settings: `:launch`, and each model and
//! effort as a command of its own (`:opus`, `:gpt-6.1-sol`, `:xhigh`). The
//! service owns the settings, so it reads and saves them.
use super::*;
use crate::providers::launch;

/// Replies to these requests, above the numbers scripts use and below Dot's.
const STATUS: u64 = (1 << 61) | 1;
const CHANGE: u64 = (1 << 61) | 2;

impl App {
    /// The model and effort words this machine's programs offer, each with
    /// what its first argument completes to.
    pub(crate) fn launch_words(&self) -> Vec<(String, Vec<String>)> {
        if !self.cfg.plugin_enabled(launch::PLUGIN) {
            return Vec::new();
        }
        let tools = self.model_dirs.as_ref().map(crate::remote::models::all).unwrap_or_default();
        launch::words(&tools)
    }

    /// `:launch [reset [program] [model|effort]]`.
    pub(crate) fn launch_command(&mut self, rest: &str) -> Result<(), String> {
        let words = actions::words(rest)?;
        match words.split_first() {
            None => self.launch_call(STATUS, "status", Vec::new()),
            Some((first, rest)) if first == "reset" => self.launch_call(CHANGE, "reset", rest.to_vec()),
            Some((other, _)) => return Err(format!("{other:?} is not reset")),
        }
        Ok(())
    }

    /// A model or effort typed as a command, such as `:opus high` or
    /// `:xhigh codex`; whether `word` was one.
    pub(crate) fn launch_word(&mut self, word: &str, rest: &str) -> bool {
        if !self.launch_words().iter().any(|(w, _)| w.eq_ignore_ascii_case(word)) {
            return false;
        }
        match actions::words(rest) {
            Ok(mut words) => {
                words.insert(0, word.to_string());
                self.launch_call(CHANGE, "set", words);
            }
            Err(why) => self.set_notice(format!(":{word} — {why}"), true),
        }
        true
    }

    fn launch_call(&mut self, request: u64, command: &str, words: Vec<String>) {
        if !self.service_has(11, "Launch settings") {
            return;
        }
        self.send(ClientMsg::Plugin {
            request,
            plugin: launch::PLUGIN.into(),
            command: command.into(),
            args: serde_json::json!({"words": words}),
        });
    }

    /// The service's answer to a launch-settings request; whether it was one.
    pub(crate) fn launch_result(&mut self, request: u64, data: &serde_json::Value, error: Option<&str>) -> bool {
        if request != STATUS && request != CHANGE {
            return false;
        }
        if let Some(error) = error {
            self.set_notice(format!("Launch settings — {error}"), true);
            return true;
        }
        if request == STATUS {
            let lines = data["lines"]
                .as_array()
                .map(|lines| lines.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            self.dialog =
                Some(Dialog::Report { title: "Launch settings".into(), lines, scroll: 0, confirm: None, waiting: 0 });
        } else {
            self.set_notice(data["message"].as_str().unwrap_or("Saved.").to_string(), false);
        }
        true
    }
}
