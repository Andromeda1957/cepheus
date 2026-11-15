//! Read-only job navigation. IDs belong to the service, including remote jobs;
//! no job can become an attached PTY or receive session input.
use super::*;
use crate::job_sessions::Info;
use crate::remote::chat::Item;
use serde_json::{json, Value};

const PAGE_BYTES: u64 = 64 * 1024;

#[derive(Default)]
pub struct View {
    pub id: Option<u64>,
    pub raw: bool,
    pub width: u16,
    pub height: u16,
    pub text: String,
    pub items: Vec<Item>,
    pub page_start: u64,
    pub next_offset: u64,
    pub scroll: usize,
    pub eof: bool,
    pub items_truncated: bool,
    pub problem: Option<String>,
    pub loading: bool,
    back: Vec<u64>,
    pending: HashMap<u64, (Pending, Instant)>,
    polled: Option<Instant>,
}

#[derive(Clone, Copy)]
enum Pending {
    Start,
    Status,
    Control { id: u64, dismiss: bool },
    Log { id: u64, offset: u64 },
}

pub fn help_lines() -> Vec<String> {
    [
        ":jobs / :job status — list retained jobs; arrows select a job",
        ":job start [--title NAME] [--cwd PATH] [--host auto|local|HOST] [--provider TOOL] [--account PROFILE] [--key KEY] -- PROGRAM ARG…",
        "Arguments use quotes/backslashes to keep spaces; no shell, variables, globs or command substitution.",
        ":job start-json {…} — advanced start options, including stdin and environment policy",
        ":job chat|raw [ID] — read structured chat or the raw log page",
        ":job next / :job prev — next / previous raw byte page; PgUp/PgDn scroll the current view",
        ":job status [ID] / :job stop [ID] / :job dismiss [ID] — default to the selected job",
        "IDs are job IDs, including remote jobs, not session row numbers. Jobs have no interactive input.",
        "Finished and failed jobs stay until dismissed or the configured retention period (default 7 days).",
    ].into_iter().map(str::to_string).collect()
}

impl App {
    pub fn ordered_jobs(&self) -> Vec<&Info> {
        let mut jobs: Vec<_> = self.state.jobs.iter().collect();
        jobs.sort_by_key(|j| (std::cmp::Reverse(j.created_at), std::cmp::Reverse(j.id)));
        jobs
    }

    fn jobs_available(&mut self) -> bool {
        if self.service_version >= 14 && self.state.job_capable {
            true
        } else {
            self.set_notice(
                "Jobs: update needed. Update/restart the deck service and host to protocol 14 with job support.",
                true,
            );
            false
        }
    }

    fn request_job(&mut self, action: &str, args: Value, pending: Pending) {
        if self.jobs.pending.len() >= 64 {
            self.set_notice("Job requests are still pending; wait for their outcome before sending more.", true);
            return;
        }
        // Lua allocates small ascending IDs; the native UI owns the upper half.
        let request = self.next_fleet_request();
        self.jobs.pending.insert(request, (pending, Instant::now()));
        self.send(ClientMsg::Fleet { request, action: action.into(), args });
    }

    pub(super) fn select_job_page(&mut self) {
        let id = match self.current {
            Some(Sel::Job(id)) => Some(id),
            _ => None,
        };
        if self.jobs.id == id {
            return;
        }
        self.jobs.id = id;
        self.jobs.raw = false;
        self.jobs.text.clear();
        self.jobs.items.clear();
        self.jobs.page_start = 0;
        self.jobs.next_offset = 0;
        self.jobs.scroll = 0;
        self.jobs.back.clear();
        self.jobs.problem = None;
        self.jobs.items_truncated = false;
        self.jobs.eof = false;
        self.jobs.loading = false;
        self.refresh_job();
    }

    pub(super) fn refresh_job(&mut self) {
        let Some(id) = self.jobs.id else { return };
        if !self.jobs_available() {
            return;
        }
        if self.jobs.pending.values().any(|(p, _)| matches!(p, Pending::Log { id: pending, offset } if *pending == id && *offset == self.jobs.page_start)) { return; }
        let offset = self.jobs.page_start;
        self.jobs.loading = true;
        self.jobs.polled = Some(Instant::now());
        self.request_job("job-log", json!({"job":id,"offset":offset,"limit":PAGE_BYTES}), Pending::Log { id, offset });
    }

    pub(super) fn reconnect_job_reads(&mut self) {
        // Starts and controls may already have taken effect: never replay them.
        self.jobs.pending.retain(|_, (pending, _)| !matches!(pending, Pending::Log { .. }));
        self.jobs.loading = false;
        self.jobs.problem = None;
        self.jobs.polled = None;
        self.refresh_job();
    }

    pub(super) fn poll_job(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .jobs
            .pending
            .iter()
            .filter(|(_, (_, at))| now.saturating_duration_since(*at) >= Duration::from_secs(15))
            .map(|(r, _)| *r)
            .collect();
        for request in expired {
            if let Some((pending, _)) = self.jobs.pending.remove(&request) {
                match pending {
                    Pending::Log { id, offset } if Some(id) == self.jobs.id && offset == self.jobs.page_start => {
                        self.jobs.loading = false;
                        self.jobs.problem =
                            Some("Job request timed out. Enter retries; :job status refreshes metadata.".into());
                    }
                    Pending::Log { .. } => {} // A departed page cannot stop the current page.
                    _ => self.set_notice(
                        "Job request timed out. :job status checks its outcome before retrying a start or control.",
                        true,
                    ),
                }
            }
        }
        let selected = self.jobs.id.is_some() && matches!(self.current, Some(Sel::Job(_)));
        if selected
            && self.jobs.problem.is_none()
            && self.jobs.polled.is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(1))
        {
            self.refresh_job();
        }
    }

    fn job_id_arg(&self, word: Option<&String>) -> Result<u64, String> {
        match word {
            Some(word) => {
                word.trim_start_matches('#').parse().map_err(|_| "give a job ID, not a session number".into())
            }
            None => match self.current {
                Some(Sel::Job(id)) => Ok(id),
                _ => Err("select a job or give its ID".into()),
            },
        }
    }

    pub(super) fn job_command(&mut self, rest: &str) -> Result<(), String> {
        if !self.jobs_available() {
            return Ok(());
        }
        if rest == "help" {
            self.dialog = Some(Dialog::Report {
                title: "Jobs".into(),
                lines: help_lines(),
                scroll: 0,
                confirm: None,
                waiting: 0,
            });
            return Ok(());
        }
        if let Some(encoded) = rest.strip_prefix("start-json ") {
            let args: Value = serde_json::from_str(encoded).map_err(|e| format!("invalid job start JSON: {e}"))?;
            if !args.is_object() {
                return Err("start-json needs an object".into());
            }
            self.request_job("job-start", args, Pending::Start);
            return Ok(());
        }
        let words = crate::actions::words(rest)?;
        let action = words.first().map(String::as_str).unwrap_or("status");
        if action == "start" {
            if words.len() == 1 {
                self.command
                    .set(format!("job start --cwd {} --host auto -- ", util::shell_quote(&self.state.start_dir)));
                self.mode = Mode::Command;
                self.set_notice("Add the executable and literal arguments. Quotes keep spaces; no shell expansion. Enter starts the job.", false);
                return Ok(());
            }
            let mut args = json!({"key":util::uuid_v4().map_err(|e| e.to_string())?,"cwd":self.state.start_dir,"placement":"auto"});
            let mut at = 1;
            while at < words.len() && words[at] != "--" && words[at].starts_with("--") {
                let field = match words[at].as_str() {
                    "--key" => "key",
                    "--title" => "title",
                    "--cwd" => "cwd",
                    "--host" | "--placement" => "placement",
                    "--provider" => "provider",
                    "--account" => "account",
                    unknown => return Err(format!("unknown start option {unknown}; :job help lists options")),
                };
                let value = words.get(at + 1).ok_or_else(|| format!("{} needs a value", words[at]))?;
                args[field] = Value::String(value.clone());
                at += 2;
            }
            if words.get(at).is_some_and(|w| w == "--") {
                at += 1;
            }
            let argv = &words[at..];
            if argv.is_empty() || argv[0].is_empty() {
                return Err("give an executable and literal arguments after --".into());
            }
            args["argv"] = json!(argv);
            if args["title"].is_null() {
                args["title"] = json!(argv[0]);
            }
            let placement = args["placement"].as_str().unwrap_or("auto");
            if placement != "auto" {
                args["placement"] = json!(self.resolve_host(placement)?.unwrap_or_else(|| "local".into()));
            }
            self.request_job("job-start", args, Pending::Start);
            return Ok(());
        }
        if !matches!(action, "status" | "log" | "raw" | "chat" | "next" | "prev" | "stop" | "dismiss") {
            return Err("use :job start / status / chat / raw / next / prev / stop / dismiss / help".into());
        }
        if words.len() > 2 {
            return Err("give one opaque job ID; the service routes its host".into());
        }
        if action == "status" {
            let id = if words.len() > 1 { Some(self.job_id_arg(words.get(1))?) } else { self.jobs.id };
            self.request_job("job-status", id.map_or_else(|| json!({}), |id| json!({"job":id})), Pending::Status);
            if rest.is_empty() {
                if let Some(id) = self.ordered_jobs().first().map(|j| j.id) {
                    self.select(Sel::Job(id));
                } else {
                    self.dialog = Some(Dialog::Report {
                        title: "Jobs".into(),
                        lines: help_lines(),
                        scroll: 0,
                        confirm: None,
                        waiting: 0,
                    });
                }
            }
            return Ok(());
        }
        let id = self.job_id_arg(words.get(1))?;
        if matches!(action, "stop" | "dismiss") {
            self.request_job(
                &format!("job-{action}"),
                json!({"job":id}),
                Pending::Control { id, dismiss: action == "dismiss" },
            );
        } else {
            if !self.state.jobs.iter().any(|j| j.id == id) {
                return Err("that job is not listed; :jobs refreshes the list".into());
            }
            self.select(Sel::Job(id));
            match action {
                "next" if self.jobs.next_offset > self.jobs.page_start => {
                    if self.jobs.back.len() == 256 {
                        self.jobs.back.remove(0);
                    }
                    self.jobs.back.push(self.jobs.page_start);
                    self.jobs.page_start = self.jobs.next_offset;
                    self.jobs.raw = true;
                }
                "prev" => {
                    self.jobs.page_start = self.jobs.back.pop().unwrap_or(0);
                    self.jobs.raw = true;
                }
                "next" => return Err("no further bytes yet; Enter refreshes this page".into()),
                _ => {
                    self.jobs.raw = action != "chat";
                    self.jobs.page_start = 0;
                    self.jobs.back.clear();
                }
            }
            // The next byte cursor belongs only to the response for this page.
            self.jobs.next_offset = self.jobs.page_start;
            self.jobs.text.clear();
            self.jobs.eof = false;
            self.jobs.problem = None;
            self.jobs.scroll = 0;
            self.refresh_job();
        }
        Ok(())
    }

    pub(super) fn job_result(&mut self, request: u64, data: &Value, error: Option<&str>) -> bool {
        let Some((pending, _)) = self.jobs.pending.remove(&request) else { return request & (1 << 63) != 0 };
        if let Pending::Log { id, offset } = pending {
            if Some(id) != self.jobs.id || offset != self.jobs.page_start {
                return true;
            }
            self.jobs.loading = false;
        }
        if let Some(error) = error {
            let text = util::one_line(error);
            if matches!(pending, Pending::Log { .. }) {
                self.jobs.problem = Some(text);
            } else {
                self.set_notice(format!("Job: {text}"), true);
            }
            return true;
        }
        match pending {
            Pending::Log { id, offset } => {
                if data["job"].as_u64() != Some(id) || self.jobs.page_start != offset {
                    return true;
                }
                self.jobs.text = data["text"].as_str().unwrap_or("").chars().take(PAGE_BYTES as usize).collect();
                self.jobs.next_offset = data["offset"].as_u64().unwrap_or(offset);
                self.jobs.eof = data["eof"].as_bool().unwrap_or(false);
                // This is a cumulative complete-record snapshot, not a delta.
                self.jobs.items = serde_json::from_value(data["items"].clone()).unwrap_or_default();
                self.jobs.items_truncated =
                    data["items_truncated"].as_bool().unwrap_or(false) || self.jobs.items.len() > 600;
                self.jobs.items.truncate(600);
                self.jobs.problem = None;
            }
            Pending::Start => {
                if let Ok(info) = serde_json::from_value::<Info>(data["job"].clone()) {
                    let id = info.id;
                    self.update_job(info);
                    self.select(Sel::Job(id));
                    self.mode = Mode::Normal;
                } else {
                    self.set_notice("Job start returned no metadata; :job status checks its outcome.", true);
                }
            }
            Pending::Status => {
                if let Ok(jobs) = serde_json::from_value::<Vec<Info>>(data["jobs"].clone()) {
                    for job in jobs {
                        self.update_job(job);
                    }
                }
            }
            Pending::Control { id, dismiss } => {
                if dismiss {
                    self.state.jobs.retain(|j| j.id != id);
                    self.fix_selection(false);
                } else if let Ok(info) = serde_json::from_value::<Info>(data["job"].clone()) {
                    self.update_job(info);
                }
                self.set_notice(
                    if dismiss { "Job dismissed." } else { "Job stop requested; its exit status remains visible." },
                    false,
                );
            }
        }
        self.dirty = true;
        true
    }

    fn update_job(&mut self, info: Info) {
        self.state.jobs.retain(|j| j.id != info.id);
        self.state.jobs.push(info);
    }

    pub(super) fn scroll_job(&mut self, op: ScrollOp) {
        let page = self.pane.height.saturating_sub(7).max(1) as usize;
        let limit = crate::view::job_sessions_scroll_limit(self);
        self.jobs.scroll = match op {
            ScrollOp::Top => 0,
            ScrollOp::Bottom => limit,
            ScrollOp::Lines(n) if n >= 0 => self.jobs.scroll.saturating_sub(n as usize),
            ScrollOp::Lines(n) => self.jobs.scroll.saturating_add(n.unsigned_abs() as usize).min(limit),
            ScrollOp::PageUp => self.jobs.scroll.saturating_sub(page),
            ScrollOp::PageDown => self.jobs.scroll.saturating_add(page).min(limit),
        };
        self.dirty = true;
    }
}

#[cfg(test)]
#[path = "../tests/tui_jobs.rs"]
mod tests;
