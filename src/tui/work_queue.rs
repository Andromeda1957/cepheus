//! Explicit queue controls. Rendering never submits work, refreshes a source or takes a claim.
use super::*;
use crate::actions;
use crate::work_queue::{Dependency, Spec};
use serde_json::{json, Value};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

const MAX_BATCH_BYTES: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Queue,
    Tasks,
    Chores,
}

impl Tab {
    fn next(self) -> Self {
        match self {
            Self::Queue => Self::Tasks,
            Self::Tasks => Self::Chores,
            Self::Chores => Self::Queue,
        }
    }
}

pub(super) enum RowKey {
    Queue(u64),
    Task(String, String),
    Chore(String),
    Missing,
}

#[derive(Default)]
pub struct View {
    pending: HashMap<u64, (String, Instant, bool)>,
    /// Every source row from the last `source-list`. The state leaves most
    /// merged rows out; the Tasks tab lists them from here.
    listed: Vec<crate::work_queue::Task>,
    /// The state's omitted-row count when `listed` was asked for.
    listed_omitted: Option<usize>,
}

pub fn help_lines() -> Vec<String> {
    [
        ":queue [list] — work queue; Tab switches queue / task sources / chores",
        ":queue enable|disable — explicitly allow or hold dispatch; default is disabled",
        ":queue add [--key KEY] [--title TITLE] [--cwd DIR] [--host auto|local|HOST] [--priority N] [--budget USD] -- PROGRAM ARG…",
        ":queue add --kind session --provider TOOL [--model MODEL] --prompt 'text' [other options]",
        "Arguments are literal, with quotes/backslashes for spaces; no shell expansion. Budget is positive USD; enforcement is not available yet.",
        ":queue add-json {full Spec} — advanced stdin/environment options",
        ":queue cancel ID; :queue reorder ID… — provide the COMPLETE cancelable set; reordering resets previous priority differences",
        ":after queue ID finished <add options>; :after task SOURCE TASK ready|merged <add options> — creates a NEW dependent item",
        "Dependencies are AND. --after ID adds a finished dependency; --after-task SOURCE TASK ready|merged adds another exact, fresh source dependency.",
        ":batch FILE — up to 100 full JSON specs (array or one per line), or literal add-option lines; parsed completely before atomic submission; maximum 1 MiB",
        ":queue sources; :queue refresh SOURCE; :queue source SOURCE TASK <add options> — explicit executable spec required; task metadata is never executed",
        ":chores [list]; :chores enable|disable ID; :chores run ID [KEY]",
        ":chores register ID --owner OWNER --repo REPO --cron 'm h * * dow' [--timezone local|UTC] [--target session|job|idle] <add options> — registers disabled",
        ":chores register-json {…} — advanced registration; :chores schedule ID 'm h * * dow' [local|UTC]; :chores remove ID",
        "Chore claims and outcomes are read-only here. Schedule changes preserve the current claim; run requires queue and chore enabled.",
        "Enter opens a dispatched session/job. Esc closes; arrows/j/k and page keys navigate. Queue/chores require protocol 14 support.",
    ].into_iter().map(str::to_string).collect()
}

fn source_form(source: &str, item: &str) -> Result<String, String> {
    let literal = |id: &str| {
        if id.is_empty() || id.chars().any(char::is_control) {
            Err("Source/task IDs must be nonempty and contain no control characters".to_string())
        } else {
            Ok(util::shell_quote(id))
        }
    };
    Ok(format!("queue source {} {} --title 'New task job' -- ", literal(source)?, literal(item)?))
}

fn generated_key() -> String {
    format!(
        "desktop-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()
    )
}

fn number(word: Option<&String>, what: &str) -> Result<u64, String> {
    word.and_then(|s| s.parse().ok()).filter(|id| *id > 0).ok_or_else(|| format!("{what} requires a positive ID"))
}

/// Shared literal option syntax for new queue entries, dependencies and batch lines.
fn spec(words: &[String], cwd: &str) -> Result<Spec, String> {
    let mut value = json!({"key":generated_key(),"title":"Queued job","kind":"job","cwd":cwd});
    let mut deps = Vec::<Dependency>::new();
    let mut i = 0;
    while i < words.len() {
        let option = words[i].as_str();
        if option == "--" {
            value["argv"] = json!(&words[i + 1..]);
            break;
        }
        i += 1;
        let arg = words.get(i).ok_or_else(|| format!("{option} needs a value"))?;
        let field = match option {
            "--key" => "key",
            "--title" => "title",
            "--kind" => "kind",
            "--cwd" => "cwd",
            "--host" => "placement",
            "--provider" => "provider",
            "--model" => "model",
            "--account" => "account",
            "--prompt" => "prompt",
            "--priority" => {
                value["priority"] = json!(arg.parse::<i32>().map_err(|_| "priority must be an integer")?);
                i += 1;
                continue;
            }
            "--budget" => {
                let amount: f64 = arg.parse().map_err(|_| "budget must be positive finite USD")?;
                if !amount.is_finite() || amount <= 0.0 {
                    return Err("budget must be positive finite USD".into());
                }
                value["budget"] = json!(amount);
                i += 1;
                continue;
            }
            "--after" => {
                deps.push(Dependency {
                    queue: Some(number(Some(arg), "--after")?),
                    source: None,
                    task: None,
                    condition: "finished".into(),
                });
                i += 1;
                continue;
            }
            "--after-task" => {
                let task = words.get(i + 1).ok_or("--after-task needs SOURCE TASK ready|merged")?;
                let condition = words
                    .get(i + 2)
                    .filter(|c| matches!(c.as_str(), "ready" | "merged"))
                    .ok_or("--after-task needs SOURCE TASK ready|merged")?;
                deps.push(Dependency {
                    queue: None,
                    source: Some(arg.clone()),
                    task: Some(task.clone()),
                    condition: condition.clone(),
                });
                i += 3;
                continue;
            }
            _ => return Err(format!("Unknown add option {option}; put literal argv after --")),
        };
        if field == "provider" {
            value[field] =
                serde_json::to_value(Tool::parse(arg).ok_or("Unknown provider")?).map_err(|e| e.to_string())?;
        } else {
            value[field] = json!(arg);
        }
        i += 1;
    }
    value["dependencies"] = json!(deps);
    if value["kind"] == "session" && value["title"] == "Queued job" {
        value["title"] = json!("Queued session");
    }
    let spec: Spec = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if !spec.cwd.is_absolute() {
        return Err("cwd must be an absolute directory".into());
    }
    match spec.kind.as_str() {
        "job" if !spec.argv.is_empty() && spec.prompt.is_none() => {}
        "session"
            if spec.provider.is_some()
                && spec.argv.is_empty()
                && spec.prompt.as_ref().is_some_and(|p| !p.trim().is_empty()) => {}
        _ => {
            return Err("job needs -- PROGRAM ARG…; session needs --kind session --provider TOOL --prompt 'text'".into())
        }
    }
    Ok(spec)
}

fn batch(path: &str, cwd: &str) -> Result<Vec<Spec>, String> {
    // Nonblocking open prevents FIFOs from freezing the UI before descriptor inspection.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("Cannot read batch: {e}"))?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err("Batch must be a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BATCH_BYTES + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BATCH_BYTES {
        return Err("Batch exceeds 1 MiB".into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "Batch must be UTF-8")?;
    let items: Vec<Spec> = if text.trim_start().starts_with('[') {
        serde_json::from_str(text).map_err(|e| format!("Invalid batch JSON: {e}"))?
    } else {
        text.lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .map(|(line, l)| {
                let l = l.trim();
                if l.starts_with('{') {
                    serde_json::from_str(l).map_err(|e| format!("Line {}: {e}", line + 1))
                } else {
                    actions::words(l.strip_prefix("queue add ").unwrap_or(l))
                        .and_then(|w| spec(&w, cwd))
                        .map_err(|e| format!("Line {}: {e}", line + 1))
                }
            })
            .collect::<Result<_, _>>()?
    };
    if items.is_empty() || items.len() > 100 {
        return Err("Batch requires 1–100 items".into());
    }
    Ok(items)
}

impl App {
    /// Native controllers share one ID stream; Lua uses the lower half.
    pub(super) fn next_fleet_request(&mut self) -> u64 {
        self.fleet_serial = self.fleet_serial.wrapping_add(1) & (u64::MAX >> 1);
        (1 << 63) | self.fleet_serial
    }

    fn queue_available(&mut self) -> bool {
        if self.service_version >= 14 && self.state.work_queue_capable {
            true
        } else {
            self.set_notice("Work queue: update needed. Update/restart the deck service and host to protocol 14 with queue support.",true);
            false
        }
    }

    /// The Tasks tab's rows: the state's current rows, then the merged rows
    /// only the last full listing has.
    pub(crate) fn source_rows(&self) -> Vec<&crate::work_queue::Task> {
        let rows = &self.state.work_queue.tasks;
        let known: std::collections::HashSet<(&str, &str)> =
            rows.iter().map(|t| (t.source_id.as_str(), t.id.as_str())).collect();
        let omitted = self
            .work_queue
            .listed
            .iter()
            .filter(|t| t.state == "merged" && !known.contains(&(t.source_id.as_str(), t.id.as_str())));
        rows.iter().chain(omitted).collect()
    }

    /// List every source row again when the rows the state leaves out changed.
    fn list_source_rows(&mut self, force: bool) {
        let omitted: usize = self.state.work_queue.sources.iter().map(|s| s.omitted).sum();
        if force || self.work_queue.listed_omitted != Some(omitted) {
            self.work_queue.listed_omitted = Some(omitted);
            self.queue_request("source-list", json!({}));
        }
    }

    /// A new state: an open Tasks tab follows the rows it leaves out.
    pub(super) fn queue_state_changed(&mut self) {
        if matches!(self.dialog, Some(Dialog::WorkQueue { tab: Tab::Tasks, .. })) {
            self.list_source_rows(false);
        }
    }

    fn queue_request(&mut self, action: &str, args: Value) {
        if !self.queue_available() {
            return;
        }
        if self.work_queue.pending.len() >= 64 {
            self.set_notice("Queue requests are still pending; check status before retrying.", true);
            return;
        }
        let request = self.next_fleet_request();
        self.work_queue
            .pending
            .insert(request, (action.into(), Instant::now(), args.get("id").is_none() && args.get("key").is_none()));
        self.send(ClientMsg::Fleet { request, action: action.into(), args });
    }

    pub(super) fn poll_work_queue(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .work_queue
            .pending
            .iter()
            .filter(|(_, (_, at, _))| now.saturating_duration_since(*at) >= Duration::from_secs(15))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.work_queue.pending.remove(&id);
            self.set_notice(
                "Queue request timed out. Check status before retrying a mutation; it may already have succeeded.",
                true,
            );
        }
    }

    pub(super) fn work_queue_result(&mut self, request: u64, data: &Value, error: Option<&str>) -> bool {
        let Some((action, _, full_list)) = self.work_queue.pending.remove(&request) else { return false };
        if !self.queue_available() {
            return true;
        }
        if let Some(error) = error {
            self.set_notice(format!("{action}: {}", util::one_line(error)), true);
            return true;
        }
        let selected = self.queue_selection();
        // Replies carry metadata only; never echo private specs or registration payloads.
        let snapshot = &mut self.state.work_queue;
        if let Some(enabled) = data.get("enabled").and_then(Value::as_bool) {
            snapshot.enabled = enabled;
            // A missing field is unconfirmed, never an effective distributed pause.
            snapshot.pause_effective = data.get("pause_effective").and_then(Value::as_bool).unwrap_or(false);
            snapshot.pause_pending =
                data.get("pause_pending").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
        }
        if let Some(items) =
            data.get("items").and_then(|v| serde_json::from_value::<Vec<crate::work_queue::Info>>(v.clone()).ok())
        {
            if matches!(action.as_str(), "queue-list" | "queue-reorder") && full_list {
                snapshot.items = items;
            } else {
                for item in items {
                    snapshot.items.retain(|i| i.id != item.id);
                    snapshot.items.push(item);
                }
            }
        }
        if let Some(item) =
            data.get("item").and_then(|v| serde_json::from_value::<crate::work_queue::Info>(v.clone()).ok())
        {
            snapshot.items.retain(|i| i.id != item.id);
            snapshot.items.push(item);
        }
        let mut listed: Vec<String> = Vec::new();
        if let Some(mut sources) = data
            .get("sources")
            .and_then(|v| serde_json::from_value::<Vec<crate::work_queue::SourceInfo>>(v.clone()).ok())
        {
            for source in &mut sources {
                listed.push(source.id.clone());
                // A listing carries every row; the state's count of what it leaves out stands.
                source.omitted = snapshot.sources.iter().find(|s| s.id == source.id).map_or(0, |s| s.omitted);
            }
            snapshot.sources = sources;
        }
        if let Some(tasks) =
            data.get("tasks").and_then(|v| serde_json::from_value::<Vec<crate::work_queue::Task>>(v.clone()).ok())
        {
            self.work_queue.listed.retain(|t| !listed.contains(&t.source_id));
            self.work_queue.listed.extend(tasks);
        }
        if let Some(chores) = data.get("chores").and_then(|v| serde_json::from_value(v.clone()).ok()) {
            snapshot.chores = chores;
        }
        if let Some(chore) =
            data.get("chore").and_then(|v| serde_json::from_value::<crate::work_queue::ChoreInfo>(v.clone()).ok())
        {
            snapshot.chores.retain(|i| i.id != chore.id);
            snapshot.chores.push(chore);
        }
        self.restore_queue_selection(selected);
        self.set_notice(
            if action == "queue-reorder" {
                "Queue reordered; previous priority differences reset.".into()
            } else {
                format!("{action} accepted.")
            },
            false,
        );
        true
    }

    pub(super) fn queue_selection(&self) -> Option<(Tab, RowKey)> {
        let Some(Dialog::WorkQueue { tab, sel }) = self.dialog.as_ref() else { return None };
        let key = if *sel == usize::MAX {
            RowKey::Missing
        } else {
            match tab {
                Tab::Queue => RowKey::Queue(self.state.work_queue.items.get(*sel)?.id),
                Tab::Tasks => {
                    let task = *self.source_rows().get(*sel)?;
                    RowKey::Task(task.source_id.clone(), task.id.clone())
                }
                Tab::Chores => RowKey::Chore(self.state.work_queue.chores.get(*sel)?.id.clone()),
            }
        };
        Some((*tab, key))
    }

    pub(super) fn restore_queue_selection(&mut self, selected: Option<(Tab, RowKey)>) {
        let Some((old_tab, key)) = selected else { return };
        if !matches!(self.dialog, Some(Dialog::WorkQueue { tab, .. }) if tab == old_tab) {
            return;
        }
        let position = match key {
            RowKey::Queue(id) => self.state.work_queue.items.iter().position(|i| i.id == id),
            RowKey::Task(source, id) => self.source_rows().iter().position(|t| t.source_id == source && t.id == id),
            RowKey::Chore(id) => self.state.work_queue.chores.iter().position(|c| c.id == id),
            RowKey::Missing => None,
        };
        if let Some(Dialog::WorkQueue { sel, .. }) = self.dialog.as_mut() {
            *sel = position.unwrap_or(usize::MAX);
        }
    }

    pub(crate) fn queue_rows(&self, tab: Tab) -> usize {
        match tab {
            Tab::Queue => self.state.work_queue.items.len(),
            Tab::Tasks => self.source_rows().len(),
            Tab::Chores => self.state.work_queue.chores.len(),
        }
    }

    fn open_work_queue(&mut self, tab: Tab) {
        self.dialog = Some(Dialog::WorkQueue { tab, sel: 0 });
    }
    fn queue_entry(&mut self, line: String) {
        self.dialog = None;
        self.command.set(line);
        self.completion = None;
        self.mode = Mode::Command;
    }

    pub(super) fn work_queue_command(&mut self, name: &str, rest: &str) -> Result<(), String> {
        if !self.queue_available() {
            return Ok(());
        }
        let cwd = self.state.start_dir.clone();
        let words = actions::words(rest)?;
        if name == "batch" {
            if words.len() != 1 {
                return Err("batch requires one file path".into());
            }
            let items = batch(&words[0], &cwd)?;
            self.queue_request("queue-batch", json!({"items":items}));
            return Ok(());
        }
        if name == "after" {
            let (dependency,skip) = match words.first().map(String::as_str) {
                Some("queue") if words.get(2).is_some_and(|c| c=="finished") => (Dependency {queue:Some(number(words.get(1),"after queue")?),source:None,task:None,condition:"finished".into()},3),
                Some("task") if words.get(3).is_some_and(|c| matches!(c.as_str(),"ready"|"merged")) => (Dependency {queue:None,source:words.get(1).cloned(),task:words.get(2).cloned(),condition:words[3].clone()},4),
                _ => return Err("after queue ID finished <add options> / after task SOURCE TASK ready|merged <add options>; creates a NEW item".into()),
            };
            let mut item = spec(&words[skip..], &cwd)?;
            item.dependencies.insert(0, dependency);
            self.queue_request("queue-add", json!(item));
            return Ok(());
        }
        if matches!(name, "chore" | "chores") {
            return self.chore_command(rest, &words, &cwd);
        }
        let operation = words.first().map(String::as_str).unwrap_or("list");
        match operation {
            "help" => self.dialog = Some(Dialog::Report {title:"Queue and chores commands".into(),lines:help_lines(),scroll:0,confirm:None,waiting:0}),
            "list" => { if words.len()>2 { return Err("queue list [ID]".into()); } self.open_work_queue(Tab::Queue); self.queue_request("queue-list",if words.len()==2 {json!({"id":number(words.get(1),"queue list")?})} else {json!({})}); }
            "enable"|"disable" if words.len()==1 => self.queue_request("queue-enable",json!({"enabled":operation=="enable"})),
            "add" if words.len()==1 => self.queue_entry("queue add --title 'New job' -- ".into()),
            "add" => { let item = spec(&words[1..],&cwd)?; self.queue_request("queue-add",json!(item)); }
            "add-json" => { let item: Spec = serde_json::from_str(rest.strip_prefix("add-json").unwrap_or("").trim()).map_err(|e|e.to_string())?; self.queue_request("queue-add",json!(item)); }
            "cancel" if words.len()==2 => { let id=number(words.get(1),"queue cancel")?; self.queue_request("queue-cancel",json!({"id":id})); }
            "reorder" if words.len()>1 => { let ids=words[1..].iter().map(|w|number(Some(w),"queue reorder")).collect::<Result<Vec<_>,_>>()?; self.queue_request("queue-reorder",json!({"ids":ids})); }
            "sources" if words.len()==1 => { self.open_work_queue(Tab::Tasks); self.list_source_rows(true); }
            "refresh" if words.len()==2 => self.queue_request("source-refresh",json!({"source":words[1]})),
            "source" if words.len()>=3 => {
                if words.len()==3 { self.queue_entry(source_form(&words[1], &words[2])?); }
                else { let item=spec(&words[3..],&cwd)?; self.queue_request("source-queue",json!({"source":words[1],"item":words[2],"spec":item})); }
            }
            _ => return Err("queue list|add|add-json|cancel|reorder|enable|disable|sources|refresh|source; :queue help lists syntax".into()),
        }
        Ok(())
    }

    fn chore_command(&mut self, rest: &str, words: &[String], cwd: &str) -> Result<(), String> {
        match words.first().map(String::as_str).unwrap_or("list") {
            "list" if words.len()<=1 => { self.open_work_queue(Tab::Chores); self.queue_request("chore-list",json!({})); }
            "enable"|"disable" if words.len()==2 => self.queue_request("chore-enable",json!({"id":words[1],"enabled":words[0]=="enable"})),
            "run" if (2..=3).contains(&words.len()) => self.queue_request("chore-run",json!({"id":words[1],"key":words.get(2).cloned().unwrap_or_else(generated_key)})),
            "schedule" if (3..=4).contains(&words.len()) => self.queue_request("chore-update",json!({"id":words[1],"schedule":{"cron":words[2],"timezone":words.get(3).map_or("local",String::as_str)}})),
            "remove" if words.len()==2 => self.queue_request("chore-remove",json!({"id":words[1]})),
            "register-json" => { let value: Value = serde_json::from_str(rest.strip_prefix("register-json").unwrap_or("").trim()).map_err(|e|e.to_string())?; self.queue_request("chore-register",value); }
            "register" if words.len()==1 => self.queue_entry("chores register ID --owner OWNER --repo REPO --cron '0 8 * * *' --title 'Daily job' -- ".into()),
            "register" if words.len()>1 => {
                let mut request=json!({"id":words[1],"enabled":false,"schedule":{"timezone":"local"}});
                let mut add=Vec::new(); let mut i=2;
                while i<words.len() {
                    let field=match words[i].as_str() { "--owner"=>Some("owner"),"--repo"=>Some("repository"),"--target"=>Some("target"),"--cron"=>Some("cron"),"--timezone"=>Some("timezone"),_=>None };
                    if let Some(field)=field { let value=words.get(i+1).ok_or("Chore option needs a value")?; if matches!(field,"cron"|"timezone") { request["schedule"][field]=json!(value); } else { request[field]=json!(value); } i+=2; }
                    else if words[i]=="--" { add.extend_from_slice(&words[i..]); break; }
                    else { let count=if words[i]=="--after-task" {4} else {2}; let end=(i+count).min(words.len()); add.extend_from_slice(&words[i..end]); i=end; }
                }
                if request["owner"].as_str().is_none() || request["repository"].as_str().is_none() || request["schedule"]["cron"].as_str().is_none() { return Err("register requires ID --owner OWNER --repo REPO --cron 'm h * * dow' and an explicit executable spec".into()); }
                let item=spec(&add,cwd)?;
                request["title"]=json!(item.title); if request["target"].is_null() {request["target"]=json!(item.kind);}
                request["spec"]=json!(item); self.queue_request("chore-register",request);
            }
            _ => return Err("chores list|register|register-json|schedule|enable|disable|run|remove; :queue help lists syntax".into()),
        }
        Ok(())
    }

    pub(super) fn work_queue_key(&mut self, tab: Tab, sel: usize, k: KeyEvent) -> Option<Dialog> {
        // Menu letters are explicit unmodified actions; Ctrl-C must never cancel work.
        if k.modifiers != KeyModifiers::NONE && k.modifiers != KeyModifiers::SHIFT {
            return Some(Dialog::WorkQueue { tab, sel });
        }
        if sel == usize::MAX
            && (matches!(k.code, KeyCode::Enter | KeyCode::Char('c'))
                || (tab == Tab::Tasks && k.code == KeyCode::Char('a')))
        {
            self.set_notice("Selected entry is no longer listed. Choose a row before acting.", false);
            return Some(Dialog::WorkQueue { tab, sel });
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return None,
            KeyCode::Tab => {
                if tab.next() == Tab::Tasks {
                    self.list_source_rows(false);
                }
                return Some(Dialog::WorkQueue { tab: tab.next(), sel: 0 });
            }
            KeyCode::Char(':') => {
                self.open_command_line();
                return None;
            }
            KeyCode::Char('r') => {
                self.queue_request(
                    match tab {
                        Tab::Queue => "queue-list",
                        Tab::Tasks => "source-list",
                        Tab::Chores => "chore-list",
                    },
                    json!({}),
                );
            }
            KeyCode::Char('a') => {
                let line = match tab {
                    Tab::Queue => "queue add --title 'New job' -- ".into(),
                    Tab::Tasks => {
                        let task = *self.source_rows().get(sel)?;
                        match source_form(&task.source_id, &task.id) {
                            Ok(line) => line,
                            Err(why) => {
                                self.set_notice(why, true);
                                return Some(Dialog::WorkQueue { tab, sel });
                            }
                        }
                    }
                    Tab::Chores => {
                        "chores register ID --owner OWNER --repo REPO --cron '0 8 * * *' --title 'Daily job' -- ".into()
                    }
                };
                self.queue_entry(line);
                return None;
            }
            KeyCode::Char('c') if tab == Tab::Queue => {
                if let Some(item) = self.state.work_queue.items.get(sel).filter(|i| i.cancelable) {
                    self.queue_request("queue-cancel", json!({"id":item.id}));
                }
            }
            KeyCode::Enter if tab == Tab::Queue => {
                if let Some(dispatch) = self.state.work_queue.items.get(sel).and_then(|i| i.dispatch.clone()) {
                    match dispatch.kind.as_str() {
                        "job" => self.select(Sel::Job(dispatch.id)),
                        "session" => self.select(Sel::Session(dispatch.id)),
                        _ => return Some(Dialog::WorkQueue { tab, sel }),
                    };
                    return None;
                }
            }
            _ => {}
        }
        Some(Dialog::WorkQueue {
            tab,
            sel: if sel == usize::MAX { sel } else { sel.min(self.queue_rows(tab).saturating_sub(1)) },
        })
    }
}

#[cfg(test)]
#[path = "../tests/tui_queue.rs"]
mod tests;
