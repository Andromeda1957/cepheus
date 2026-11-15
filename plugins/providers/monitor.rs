//! Background collection of per-session facts (conversation, title, busy
//! state, worktree, containers, reviews), sessions running outside the deck,
//! and usage limits.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::activity;
use crate::antigravity;
use crate::claude::{self, ClaudeProc};
use crate::codex::{self, RolloutMeta};
use crate::external;
use crate::gitwt::{self, WorktreeEntry};
use crate::other;
use crate::procs::{self, ProcInfo};
use crate::protocol::{ContainerInfo, OutsideInfo, Subagent, Tool, Usage, UsageWindow, Worktree};
use crate::service::Ev;
use crate::util;

#[path = "memory.rs"]
mod memory;
pub use memory::{safe_rerun, MemoryKill};

#[derive(Debug, Clone)]
pub struct HostedRef {
    pub id: u64,
    pub tool: Tool,
    pub pid: Option<u32>,
    pub conversation: Option<String>,
    /// When it started, or started a new chat: where its conversation is
    /// looked for from.
    pub spawned_ms: u64,
    pub alive: bool,
    /// Conversations it has left (`/clear`, `/new`); never its current one.
    pub left: Vec<String>,
    /// What a Codex's status line shows, among it its thread's name
    /// (`status::codex_footer`).
    pub footer: Vec<String>,
    /// The directory it started in when that is not the start directory.
    pub cwd: Option<String>,
    /// The provider config root selected by this session account profile.
    /// None preserves the configured default account.
    pub account_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionFacts {
    /// The process whose terminal and records this snapshot describes.
    #[serde(default)]
    pub known_pid: Option<u32>,
    /// The conversation the service had recorded when the monitor took the
    /// snapshot these facts come from.
    pub known_conversation: Option<String>,
    pub conversation: Option<String>,
    /// AGY recorded `/clear` in this conversation after the session started.
    #[serde(default)]
    pub cleared_ms: Option<u64>,
    pub meta_title: Option<String>,
    /// The title Claude generated for the conversation (its latest
    /// `ai-title` record), when it made one.
    #[serde(default)]
    pub ai_title: Option<String>,
    /// The agent Claude runs as (`agent-setting`), whose name Claude puts in
    /// the terminal title.
    #[serde(default)]
    pub agent: Option<String>,
    pub first_prompt: Option<String>,
    /// The task the owner gave last (`util::task_title`): a session's title follows it.
    #[serde(default)]
    pub latest_prompt: Option<String>,
    pub busy: Option<bool>,
    /// The latest turn the program's own record shows completed, for the
    /// programs whose hook does not report it (`Tracker::turn_recorded`).
    #[serde(default)]
    pub turn_end: Option<TurnEnd>,
    /// Persistent Codex goal status; None means the database was unavailable.
    #[serde(default)]
    pub goal_active: Option<bool>,
    pub worktree: Option<Worktree>,
    pub containers: Vec<ContainerInfo>,
    pub review: bool,
    /// Its sub-agents (`activity.rs`). A Claude session's are its
    /// `claude_agents`, which the service keeps from Claude's hook reports.
    #[serde(default)]
    pub subagents: Vec<Subagent>,
    /// What Claude's hooks reported of its sub-agents (`activity::claude_hook`);
    /// the monitor never sets it.
    #[serde(default)]
    pub claude_agents: Vec<activity::ClaudeAgent>,
    /// Its last tool call, as the row shows it.
    #[serde(default)]
    pub activity: Option<String>,
    /// When its conversation last changed (unix seconds).
    #[serde(default)]
    pub last_active: u64,
    /// Private test/build rerun facts; command text must never enter event logs.
    #[serde(default)]
    pub memory_kills: Vec<MemoryKill>,
}

/// A completed turn in a program's own record: Codex's `task_complete`,
/// Grok's `turn_ended` "completed", opencode's "succeeded" idle message, and
/// an Antigravity conversation summary gone idle.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnEnd {
    /// What tells it from the record's other turn ends: Codex's turn id,
    /// Grok's time stamp, opencode's message number or Antigravity's step
    /// count.
    pub id: String,
    /// When it ended (unix ms; 0 when the record does not say).
    pub at_ms: u64,
    /// The record shows a turn begun, or ended otherwise, after it.
    pub superseded: bool,
}

/// Grok, opencode or Antigravity's latest completed turn in one
/// conversation (`Monitor::recorded_turn`), and what it was read from.
#[derive(Default)]
struct TurnRead {
    /// Grok's `events.jsonl` for the conversation, once found.
    source: Option<PathBuf>,
    /// The modification time of the record when it was read.
    stamp: Option<u64>,
    read_at: Option<Instant>,
    end: Option<TurnEnd>,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub facts: HashMap<u64, SessionFacts>,
    pub outside: Vec<OutsideInfo>,
    pub usage: Usage,
}

#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// Grok's and Gemini's allowances and Claude's per-model limits
    /// (`quotas::Refresh`); none in tests.
    pub quotas: Option<Arc<crate::providers::quotas::Refresh>>,
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
    pub antigravity_dir: PathBuf,
    /// Grok's home and opencode's session database, for their titles.
    pub grok_home: PathBuf,
    pub opencode_db: PathBuf,
    pub start_dir: PathBuf,
    pub usage_file: PathBuf,
    pub service_pid: u32,
    /// Hosted Codex runs embedded with a `notify` hook (`Config::codex_notify`).
    pub codex_notify: bool,
    /// Worktree task names, task containers, reviews and command wrappers.
    pub conventions: crate::config::Conventions,
    pub desktop: bool,
}

#[derive(Debug, Clone, Default)]
struct Slow {
    containers: HashMap<String, Vec<ContainerInfo>>,
    workspaces: HashMap<u32, String>,
}

#[derive(Default)]
struct AccountCodex {
    names: HashMap<String, codex::ThreadName>,
    names_at: Option<Instant>,
    goals: Option<HashSet<String>>,
    recent: Vec<(PathBuf, u64)>,
}

struct Scan {
    path: PathBuf,
    offset: u64,
    worktree: Option<WorktreeEntry>,
    /// Claude's generated title and agent, read in the same pass.
    ai_title: Option<String>,
    agent: Option<String>,
    /// The task the owner gave last, and where its line starts: a rescan
    /// from further back never replaces it with one before it.
    task: Option<util::TaskPrompt>,
    task_at: u64,
}

impl Scan {
    fn new(path: PathBuf) -> Scan {
        Scan { path, offset: 0, worktree: None, ai_title: None, agent: None, task: None, task_at: 0 }
    }

    fn found_task(&mut self, at: u64, task: util::TaskPrompt) {
        if self.task.is_none() || at >= self.task_at {
            self.task_at = at;
            self.task = Some(task);
        }
    }
}

/// The task the owner gave last in a transcript between `floor` and `to`,
/// read backwards a chunk at a time, with where its line starts. The scan
/// reads only the end of a transcript, and one turn can write more than that.
fn last_task_between(path: &Path, tool: Tool, floor: u64, mut to: u64) -> Option<(u64, util::TaskPrompt)> {
    let find = match tool {
        Tool::Claude => claude::last_task,
        Tool::Codex => codex::last_task,
        Tool::Antigravity | Tool::Grok | Tool::OpenCode | Tool::Shell | Tool::Dynamic(_) => return None,
    };
    while to > floor {
        let from = to.saturating_sub(SCAN_LIMIT).max(floor);
        let chunk = util::read_between(path, from, to).ok()?;
        // Whole lines only: the first one read may have begun before `from`.
        let skip =
            if from == floor { 0 } else { chunk.iter().position(|&b| b == b'\n').map_or(chunk.len(), |i| i + 1) };
        if let Some(task) = find(&chunk[skip..]) {
            return Some((from + (skip + task.pos) as u64, task));
        }
        to = if skip < chunk.len() { from + skip as u64 } else { from };
    }
    None
}

const RETRY: Duration = Duration::from_secs(10);
/// A shared-server Codex creates its thread a few seconds after it starts
/// (about 5 s on 2026-10-25); its rollout is matched within this window.
const CODEX_MATCH_WINDOW_MS: u64 = 30_000;
/// Clock allowance for a rollout stamped just before the session's spawn time.
const CODEX_CLOCK_SLACK_MS: u64 = 2_000;
const SCAN_LIMIT: u64 = 8 << 20;
const RESCAN_ON_CHANGE: u64 = 2 << 20;

/// A Codex rate-limit sample: when Codex recorded it (unix ms) and its windows.
type UsageSample = (u64, Vec<UsageWindow>);

pub struct Monitor {
    cfg: MonitorConfig,
    hosted: Arc<Mutex<Vec<HostedRef>>>,
    slow: Arc<Mutex<Slow>>,
    worktrees: Vec<WorktreeEntry>,
    worktrees_at: Option<Instant>,
    scans: HashMap<String, Scan>,
    prompts: HashMap<PathBuf, (Option<String>, Instant)>,
    metas: HashMap<PathBuf, RolloutMeta>,
    rollout_paths: HashMap<(PathBuf, String), (Option<PathBuf>, Instant)>,
    discovered: HashMap<u64, String>,
    /// Hosted Codex sessions now in another conversation than the recorded one.
    followed: HashMap<u64, String>,
    names: HashMap<String, codex::ThreadName>,
    names_at: Option<Instant>,
    /// Each recent rollout's latest usage sample, with the modification time
    /// it was read at.
    codex_samples: HashMap<PathBuf, (u64, Option<UsageSample>)>,
    /// The newest Codex usage sample seen.
    codex_usage: Option<UsageSample>,
    /// Claude's usage snapshot file version and its windows.
    claude_snapshot: (Option<(u64, i64, i64)>, Vec<UsageWindow>),
    /// Each Codex rollout's last known turn state and completed turn
    /// (`codex_turn`).
    turns: HashMap<PathBuf, (Option<bool>, Option<TurnEnd>)>,
    /// Grok, opencode and Antigravity conversations' completed turns.
    turn_reads: HashMap<String, TurnRead>,
    active_goals: Option<HashSet<String>>,
    /// Antigravity's prompt history and the modification time it was read at.
    agy_prompts: (Option<u64>, Vec<antigravity::Prompt>),
    /// Grok and opencode titles by conversation, and when each was read.
    other_titles: HashMap<String, (Option<String>, Instant)>,
    /// Each transcript's last tool call, by the modification time it was
    /// read at.
    activities: HashMap<PathBuf, (u64, Option<String>)>,
    /// The recent Codex rollouts of this cycle, for sub-agent threads.
    recent: Vec<(PathBuf, u64)>,
    accounts: HashMap<PathBuf, AccountCodex>,
    memory_scans: HashMap<u64, memory::Scanner>,
}

/// How much of a transcript's end is read for its last tool call.
const ACTIVITY_TAIL: u64 = 128 * 1024;

/// How often a Grok or opencode conversation's title is read again.
const OTHER_TITLE_INTERVAL: Duration = Duration::from_secs(10);

/// How much of Grok's `events.jsonl` is read for its latest turn end: Grok
/// writes a line at each change of phase while it streams, so a turn's end
/// is at the end of the file when the turn is over.
const GROK_EVENTS_TAIL: u64 = 64 * 1024;

/// How much of a rollout's end `codex_turn` reads on each cycle.
const TURN_TAIL: u64 = 256 * 1024;
/// How far back it reads once for a rollout whose turn state is unknown.
const TURN_DEEP_TAIL: u64 = 8 << 20;

pub fn spawn(cfg: MonitorConfig, hosted: Arc<Mutex<Vec<HostedRef>>>, tx: Sender<Ev>) {
    let slow = Arc::new(Mutex::new(Slow::default()));
    let label = cfg.conventions.container_task_label.clone();
    let desktop = cfg.desktop;
    {
        let slow = slow.clone();
        let _ = thread::Builder::new().name("monitor-slow".into()).spawn(move || loop {
            let fresh = Slow {
                containers: external::containers(&label),
                workspaces: if desktop { external::hypr_workspaces() } else { HashMap::new() },
            };
            if let Ok(mut guard) = slow.lock() {
                *guard = fresh;
            }
            thread::sleep(Duration::from_secs(5));
        });
    }
    let _ = thread::Builder::new().name("monitor".into()).spawn(move || {
        let mut monitor = Monitor::new(cfg, hosted, slow);
        loop {
            let report = monitor.cycle();
            if tx.send(Ev::Monitor(Box::new(report))).is_err() {
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// The thread id passed to `codex resume <id>`, if any.
pub fn resume_id(args: &[String]) -> Option<String> {
    let pos = args.iter().position(|a| a == "resume")?;
    let id = args.get(pos + 1)?;
    (id.len() == 36 && id.chars().filter(|&c| c == '-').count() == 4).then(|| id.clone())
}

impl Monitor {
    fn new(cfg: MonitorConfig, hosted: Arc<Mutex<Vec<HostedRef>>>, slow: Arc<Mutex<Slow>>) -> Self {
        Monitor {
            cfg,
            hosted,
            slow,
            worktrees: Vec::new(),
            worktrees_at: None,
            scans: HashMap::new(),
            prompts: HashMap::new(),
            metas: HashMap::new(),
            rollout_paths: HashMap::new(),
            discovered: HashMap::new(),
            followed: HashMap::new(),
            names: HashMap::new(),
            names_at: None,
            codex_samples: HashMap::new(),
            codex_usage: None,
            claude_snapshot: (None, Vec::new()),
            turns: HashMap::new(),
            turn_reads: HashMap::new(),
            active_goals: None,
            agy_prompts: (None, Vec::new()),
            other_titles: HashMap::new(),
            activities: HashMap::new(),
            recent: Vec::new(),
            accounts: HashMap::new(),
            memory_scans: HashMap::new(),
        }
    }

    /// The last tool call in the transcript at `path`, read again only after
    /// it changed.
    fn activity_of(&mut self, path: &Path, tool: Tool) -> (Option<String>, u64) {
        let Some(modified) = util::modified_ms(path) else { return (None, 0) };
        if let Some((at, activity)) = self.activities.get(path) {
            if *at == modified {
                return (activity.clone(), modified / 1000);
            }
        }
        let tail = util::read_tail(path, ACTIVITY_TAIL).unwrap_or_default();
        let activity = match tool {
            Tool::Claude => activity::claude_activity(&tail),
            Tool::Codex => activity::codex_activity(&tail),
            _ => None,
        };
        self.activities.insert(path.to_path_buf(), (modified, activity.clone()));
        (activity, modified / 1000)
    }

    /// The threads Codex spawned as sub-agents of thread `conv`, among the
    /// recent rollouts.
    fn codex_subagents(&mut self, conv: &str, recent: Vec<(PathBuf, u64)>) -> Vec<Subagent> {
        let now = util::now_secs();
        let mut out = Vec::new();
        for (path, modified_ms) in recent {
            let Some(meta) = self.meta(&path) else { continue };
            if meta.parent.as_deref() != Some(conv) {
                continue;
            }
            let last_active = modified_ms / 1000;
            let running = if now.saturating_sub(last_active) < activity::FINISHED_KEEP_SECS {
                self.codex_turn(&path)
            } else {
                Some(false)
            };
            out.push(activity::codex_subagent(
                meta.nickname.as_deref(),
                meta.agent_path.as_deref(),
                meta.created_ms,
                last_active,
                running,
                now,
            ));
        }
        activity::keep_recent(out, now)
    }

    /// The title Grok or opencode gave a conversation, read again every
    /// `OTHER_TITLE_INTERVAL`: opencode's comes from its database through a
    /// `sqlite3` process.
    fn other_title(&mut self, tool: Tool, conv: &str) -> Option<String> {
        if let Some((title, at)) = self.other_titles.get(conv) {
            if at.elapsed() < OTHER_TITLE_INTERVAL {
                return title.clone();
            }
        }
        let title = match tool {
            Tool::Grok => other::grok_title(&self.cfg.grok_home, conv),
            Tool::OpenCode => other::opencode_title(&self.cfg.opencode_db, conv),
            _ => None,
        };
        self.other_titles.insert(conv.to_string(), (title.clone(), Instant::now()));
        title
    }

    /// Antigravity's prompt history, read again only after it changed.
    fn agy_prompts(&mut self) -> &[antigravity::Prompt] {
        let modified = util::modified_ms(&self.cfg.antigravity_dir.join("history.jsonl"));
        if modified != self.agy_prompts.0 {
            self.agy_prompts = (modified, antigravity::history(&self.cfg.antigravity_dir));
        }
        &self.agy_prompts.1
    }

    /// Find the conversation each hosted Antigravity session is in: the one
    /// conversation database its processes hold open, or else, for a single
    /// new session, the one conversation prompted in the start directory
    /// since it started. Another session's conversation never counts.
    fn discover_antigravity(&mut self, hosted: &[HostedRef], procs: &HashMap<u32, ProcInfo>) {
        let agy: Vec<&HostedRef> = hosted.iter().filter(|h| h.tool == Tool::Antigravity && h.alive).collect();
        if agy.is_empty() {
            return;
        }
        let known = |m: &Self, h: &HostedRef| {
            h.conversation.clone().or_else(|| m.discovered.get(&h.id).cloned()).filter(|c| !h.left.contains(c))
        };
        let mut pending = Vec::new();
        for h in &agy {
            let recorded = known(self, h);
            let others: HashSet<String> =
                hosted.iter().filter(|o| o.id != h.id && o.alive).filter_map(|o| known(self, o)).collect();
            let open: Vec<String> = h
                .pid
                .map(|pid| {
                    let mut tree = procs::descendants(procs, pid);
                    tree.push(pid);
                    tree.into_iter()
                        .flat_map(|p| antigravity::open_conversations(&self.cfg.antigravity_dir, p))
                        .filter(|c| !others.contains(c) && !h.left.contains(c))
                        .collect()
                })
                .unwrap_or_default();
            let mut open = open;
            open.sort_unstable();
            open.dedup();
            match (open.as_slice(), &recorded) {
                ([only], None) => {
                    self.discovered.insert(h.id, only.clone());
                }
                ([only], Some(r)) if only != r => {
                    self.followed.insert(h.id, only.clone());
                }
                ([], None) => pending.push(*h),
                _ => {}
            }
        }
        if let [h] = pending.as_slice() {
            let claimed: HashSet<String> = agy
                .iter()
                .filter(|o| o.id != h.id)
                .filter_map(|o| known(self, o))
                .chain(h.left.iter().cloned())
                .collect();
            let start_dir = h.cwd.clone().unwrap_or_else(|| self.cfg.start_dir.to_string_lossy().into_owned());
            let spawned_ms = h.spawned_ms;
            let id = h.id;
            let prompts = self.agy_prompts();
            if let Some(conv) = antigravity::started_conversation(prompts, &start_dir, spawned_ms, &claimed) {
                self.discovered.insert(id, conv);
            }
        }
    }

    /// Whether a Codex turn is running in the rollout at `path`. A long turn writes more than `TURN_TAIL` after
    /// its `task_started`, and a tail with no turn event left the state unknown; the screen's working line then
    /// decided it, and that line comes and goes mid-turn. The last state read stays until a newer turn event
    /// replaces it, and an unknown rollout is read further back once. Its latest completed turn
    /// (`codex::turn_end`) is kept from the same read, the last one found staying until a newer one replaces
    /// it.
    fn codex_turn(&mut self, path: &Path) -> Option<bool> {
        let tail = util::read_tail(path, TURN_TAIL).ok();
        let recent = tail.as_deref().and_then(codex::turn_state);
        let end = tail.as_deref().and_then(codex::turn_end);
        let known = self.turns.remove(path);
        let state = match (recent, &known) {
            (Some(state), _) => Some(state),
            (None, Some((known, _))) => *known,
            (None, None) => util::read_tail(path, TURN_DEEP_TAIL).ok().and_then(|t| codex::turn_state(&t)),
        };
        let end = end.or_else(|| known.and_then(|(_, end)| end));
        self.turns.insert(path.to_path_buf(), (state, end));
        state
    }

    /// The latest completed turn `codex_turn` last read in a rollout.
    fn codex_turn_end(&self, path: &Path) -> Option<TurnEnd> {
        self.turns.get(path).and_then(|(_, end)| end.clone())
    }

    /// The latest completed turn in a Grok, opencode or Antigravity
    /// conversation's own record. Grok's `events.jsonl` is read again only
    /// once it changed. opencode's and Antigravity's databases are read with
    /// a `sqlite3` process, once they changed and then went a second without
    /// change (a turn writes them all the while it runs), or at most every
    /// `OTHER_TITLE_INTERVAL` while they keep changing. A read that finds no
    /// end keeps the last one found.
    fn recorded_turn(&mut self, tool: Tool, conv: &str) -> Option<TurnEnd> {
        let grok_home = &self.cfg.grok_home;
        let read = self.turn_reads.entry(conv.to_string()).or_default();
        let found = match tool {
            Tool::Grok => {
                if read.source.is_none() {
                    read.source = other::grok_events(grok_home, conv);
                }
                let path = read.source.as_deref()?;
                let stamp = util::modified_ms(path);
                if stamp == read.stamp {
                    return read.end.clone();
                }
                read.stamp = stamp;
                util::read_tail(path, GROK_EVENTS_TAIL).ok().and_then(|tail| other::grok_turn_end(&tail))
            }
            Tool::OpenCode | Tool::Antigravity => {
                let db = if tool == Tool::OpenCode {
                    self.cfg.opencode_db.clone()
                } else {
                    antigravity::summaries_db(&self.cfg.antigravity_dir)
                };
                let wal = PathBuf::from(format!("{}-wal", db.display()));
                let stamp = [&db, &wal].iter().filter_map(|f| util::modified_ms(f)).max();
                let settled = stamp.is_some_and(|at| util::now_ms().saturating_sub(at) >= 1000);
                let overdue = read.read_at.is_none_or(|at| at.elapsed() >= OTHER_TITLE_INTERVAL);
                if stamp.is_none() || stamp == read.stamp || !(settled || overdue) {
                    return read.end.clone();
                }
                read.stamp = stamp;
                read.read_at = Some(Instant::now());
                if tool == Tool::OpenCode {
                    other::opencode_turn_end(&db, conv)
                } else {
                    antigravity::turn_end(&db, conv)
                }
            }
            _ => return None,
        };
        if found.is_some() {
            read.end = found;
        }
        read.end.clone()
    }

    fn cycle(&mut self) -> Report {
        let hosted = lock(&self.hosted).clone();
        // A conversation found for a session that has left it since.
        self.discovered.retain(|id, conv| !hosted.iter().any(|h| h.id == *id && h.left.contains(conv)));
        let procs = procs::scan();
        let claude_files = claude::session_files(&self.cfg.claude_dir);
        let slow = lock(&self.slow).clone();
        self.refresh_worktrees();
        self.refresh_names();
        self.active_goals = codex::active_goals(&self.cfg.codex_dir);
        let recent = codex::rollouts_in(&codex::day_dirs(&self.cfg.codex_dir, 2));
        self.recent = recent.clone();
        let default_refs: Vec<HostedRef> = hosted
            .iter()
            .filter(|h| h.tool == Tool::Codex && h.account_root.as_ref().is_none_or(|root| root == &self.cfg.codex_dir))
            .cloned()
            .collect();
        self.discover_codex(&default_refs, &recent, &procs, util::now_ms());
        self.follow_codex(&default_refs, &procs);
        let roots: HashSet<PathBuf> = hosted
            .iter()
            .filter(|h| h.alive && h.tool == Tool::Codex)
            .filter_map(|h| h.account_root.clone())
            .filter(|root| root != &self.cfg.codex_dir)
            .collect();
        self.accounts.retain(|root, _| roots.contains(root));
        for root in roots {
            let account = self.accounts.entry(root.clone()).or_default();
            if account.names_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(5)) {
                account.names = codex::thread_names(&root);
                account.names_at = Some(Instant::now());
            }
            account.goals = codex::active_goals(&root);
            account.recent = codex::rollouts_in(&codex::day_dirs(&root, 2));
            let recent = account.recent.clone();
            let refs: Vec<HostedRef> = hosted
                .iter()
                .filter(|h| h.tool == Tool::Codex && h.account_root.as_ref() == Some(&root))
                .cloned()
                .collect();
            self.discover_codex_in(&root, &refs, &recent, &procs, util::now_ms());
            self.follow_codex_in(&root, &refs, &procs);
        }
        let claude_accounts: HashMap<PathBuf, Vec<ClaudeProc>> = hosted
            .iter()
            .filter(|h| h.alive && h.tool == Tool::Claude)
            .filter_map(|h| h.account_root.clone())
            .filter(|root| root != &self.cfg.claude_dir)
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|root| {
                let files = claude::session_files(&root);
                (root, files)
            })
            .collect();
        self.discover_antigravity(&hosted, &procs);
        let hosted_pids: HashSet<u32> = hosted.iter().filter(|h| h.alive).filter_map(|h| h.pid).collect();

        let mut facts = HashMap::new();
        for h in hosted.iter().filter(|h| h.alive) {
            let files = h
                .account_root
                .as_ref()
                .filter(|_| h.tool == Tool::Claude)
                .and_then(|root| claude_accounts.get(root))
                .unwrap_or(&claude_files);
            facts.insert(h.id, self.session_facts(h, &procs, files, &slow));
        }
        let live: HashSet<u64> = hosted.iter().filter(|h| h.alive).map(|h| h.id).collect();
        self.discovered.retain(|id, _| live.contains(id));
        self.memory_scans.retain(|id, _| live.contains(id));
        // Bounded caches: what a session no longer reads is read again.
        if self.activities.len() > 256 {
            self.activities.clear();
        }

        let mut outside = self.outside(&procs, &claude_files, &hosted_pids, &slow);
        for session in &mut outside {
            if !self.cfg.desktop {
                session.kitty_pid = None;
                session.kitty_window = None;
                session.kitty_socket = false;
                session.workspace = None;
            }
        }
        let live_keys: HashSet<String> = hosted
            .iter()
            .filter(|h| h.alive)
            .map(|h| format!("s{}", h.id))
            .chain(outside.iter().map(|o| format!("p{}", o.pid)))
            .collect();
        self.scans.retain(|k, _| live_keys.contains(k));
        let live_conversations: HashSet<&str> = hosted
            .iter()
            .filter(|h| h.alive)
            .flat_map(|h| [h.conversation.as_deref(), self.discovered.get(&h.id).map(String::as_str)])
            .chain(self.followed.values().map(|c| Some(c.as_str())))
            .flatten()
            .collect();
        self.turn_reads.retain(|conv, _| live_conversations.contains(conv.as_str()));

        let candidates = self.usage_candidates(&recent, &procs);
        let mut usage = Usage::default();
        if let Some(quotas) = &self.cfg.quotas {
            quotas.fill(&mut usage, util::now_secs());
        }
        usage.claude = self.claude_usage();
        usage.codex = self.codex_usage(&candidates);
        Report { facts, outside, usage }
    }

    fn session_facts(
        &mut self,
        h: &HostedRef,
        procs: &HashMap<u32, ProcInfo>,
        claude_files: &[ClaudeProc],
        slow: &Slow,
    ) -> SessionFacts {
        let mut f =
            SessionFacts { known_pid: h.pid, known_conversation: h.conversation.clone(), ..SessionFacts::default() };
        let mut transcript = None;
        let mut thread_name = None;
        match h.tool {
            Tool::Claude => {
                let file = h.pid.and_then(|pid| claude_files.iter().find(|c| c.pid == pid));
                let conv = file.and_then(|c| c.session_id.clone()).or_else(|| h.conversation.clone());
                if let Some(c) = file {
                    f.busy = c.busy;
                    f.meta_title = c.name.clone();
                }
                if let Some(conv) = &conv {
                    let cwd = file
                        .and_then(|c| c.cwd.clone())
                        .or_else(|| h.cwd.clone())
                        .unwrap_or_else(|| self.cfg.start_dir.to_string_lossy().into_owned());
                    let root = h.account_root.as_ref().unwrap_or(&self.cfg.claude_dir);
                    let path = claude::transcript_path(root, &cwd, conv);
                    f.first_prompt = self.prompt_for(&path, Tool::Claude);
                    transcript = Some(path);
                }
                f.conversation = conv;
            }
            Tool::Codex => {
                let conv = self
                    .followed
                    .get(&h.id)
                    .cloned()
                    .or_else(|| h.conversation.clone())
                    .or_else(|| self.discovered.get(&h.id).cloned());
                if let Some(conv) = &conv {
                    let root = h.account_root.as_ref().unwrap_or(&self.cfg.codex_dir).clone();
                    if let Some(path) = self.rollout_path_in(&root, conv) {
                        f.busy = self.codex_turn(&path);
                        f.turn_end = self.codex_turn_end(&path);
                        f.first_prompt = self.prompt_for(&path, Tool::Codex);
                        transcript = Some(path);
                    }
                    let (goals, names, recent) = if root == self.cfg.codex_dir {
                        (self.active_goals.as_ref(), Some(&self.names), self.recent.clone())
                    } else {
                        let account = self.accounts.get(&root);
                        (
                            account.and_then(|a| a.goals.as_ref()),
                            account.map(|a| &a.names),
                            account.map(|a| a.recent.clone()).unwrap_or_default(),
                        )
                    };
                    f.goal_active = goals.map(|goals| goals.contains(conv));
                    thread_name = names.and_then(|names| names.get(conv)).cloned();
                    f.subagents = self.codex_subagents(conv, recent);
                }
                f.conversation = conv;
            }
            Tool::Antigravity => {
                let conv = self
                    .followed
                    .get(&h.id)
                    .cloned()
                    .or_else(|| h.conversation.clone())
                    .or_else(|| self.discovered.get(&h.id).cloned());
                if let Some(conv) = &conv {
                    let workspace = h.cwd.clone().unwrap_or_else(|| self.cfg.start_dir.to_string_lossy().into_owned());
                    let prompts = self.agy_prompts();
                    f.first_prompt = antigravity::first_prompt(prompts, conv);
                    // AGY records /clear against the conversation it leaves;
                    // the empty replacement may not have a discoverable database yet.
                    f.cleared_ms = prompts
                        .iter()
                        .filter(|p| {
                            p.conversation == *conv
                                && p.workspace == workspace
                                && p.at_ms >= h.spawned_ms
                                && p.command
                                && p.text.trim() == "/clear"
                        })
                        .map(|p| p.at_ms)
                        .max();
                    f.turn_end = self.recorded_turn(Tool::Antigravity, conv);
                }
                f.conversation = conv;
            }
            // The deck chose the conversation when it started the program.
            Tool::Grok | Tool::OpenCode => {
                if let Some(conv) = &h.conversation {
                    f.ai_title = self.other_title(h.tool, conv);
                    f.turn_end = self.recorded_turn(h.tool, conv);
                }
                f.conversation = h.conversation.clone();
                // opencode shows its turns on screen; Grok does not.
                if let Some(pid) = h.pid.filter(|_| h.tool == Tool::Grok) {
                    f.busy = Some(other::grok_turn_running(procs, pid));
                }
            }
            // The service keeps terminals out of the monitor's list.
            Tool::Shell | Tool::Dynamic(_) => {}
        }
        let mut task = None;
        if let Some(path) = transcript {
            (f.activity, f.last_active) = self.activity_of(&path, h.tool);
            f.memory_kills = self.memory_scans.entry(h.id).or_default().read(
                &path,
                h.tool,
                util::now_secs(),
                &self.cfg.conventions.command_wrappers,
            );
            let key = format!("s{}", h.id);
            f.worktree = self.scan_worktree(key.clone(), path, h.tool);
            if let Some(scan) = self.scans.get(&key) {
                (f.ai_title, f.agent, task) = (scan.ai_title.clone(), scan.agent.clone(), scan.task.clone());
            }
        }
        f.meta_title =
            f.meta_title.or(thread_name.filter(|n| !task.as_ref().is_some_and(|t| n.outdated_by(t))).map(|n| n.name));
        f.latest_prompt = task.map(|t| t.title);
        crate::extensions::conventions::enrich(
            &mut f,
            h.pid,
            procs,
            &slow.containers,
            &self.cfg.conventions.review_commands,
        );
        f
    }

    fn refresh_worktrees(&mut self) {
        if self.worktrees_at.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        self.worktrees_at = Some(Instant::now());
        let fresh = gitwt::list(&self.cfg.start_dir);
        if fresh != self.worktrees {
            self.worktrees = fresh;
            for scan in self.scans.values_mut() {
                scan.offset = scan.offset.saturating_sub(RESCAN_ON_CHANGE);
            }
        }
    }

    fn refresh_names(&mut self) {
        if self.names_at.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        self.names_at = Some(Instant::now());
        self.names = codex::thread_names(&self.cfg.codex_dir);
    }

    fn meta(&mut self, path: &PathBuf) -> Option<RolloutMeta> {
        if let Some(meta) = self.metas.get(path) {
            return Some(meta.clone());
        }
        let meta = codex::read_meta(path)?;
        self.metas.insert(path.clone(), meta.clone());
        Some(meta)
    }

    /// Learn the conversation of hosted Codex sessions that have not reported
    /// one. A guess is worse than not knowing: the deck would show another
    /// agent's title and worktree and reopen its conversation.
    ///
    /// An embedded Codex (the `notify` hook) holds its rollout open from its
    /// first turn, so the user thread its own process tree has open is the
    /// answer; until then it stays unknown, and its first notify names it. A
    /// Codex attached to the shared background server holds nothing open, so
    /// its thread is matched by creation time, but only once the match window
    /// has closed, and only when exactly one candidate falls in the session's
    /// window and in no other unmatched session's window. Two sessions started
    /// together, or another Codex starting a thread in the same checkout,
    /// leave it unknown.
    fn discover_codex(
        &mut self,
        hosted: &[HostedRef],
        recent: &[(PathBuf, u64)],
        procs: &HashMap<u32, ProcInfo>,
        now_ms: u64,
    ) {
        let root = self.cfg.codex_dir.clone();
        self.discover_codex_in(&root, hosted, recent, procs, now_ms);
    }

    fn discover_codex_in(
        &mut self,
        root: &Path,
        hosted: &[HostedRef],
        recent: &[(PathBuf, u64)],
        procs: &HashMap<u32, ProcInfo>,
        now_ms: u64,
    ) {
        let pending: Vec<&HostedRef> = hosted
            .iter()
            .filter(|h| {
                h.tool == Tool::Codex && h.alive && h.conversation.is_none() && !self.discovered.contains_key(&h.id)
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        // A session's conversation, or one it has left: after `/clear` or
        // `/new` Codex keeps the old rollout open beside the new one.
        let claimed: HashSet<String> = hosted
            .iter()
            .flat_map(|h| h.conversation.iter().chain(&h.left).cloned())
            .chain(hosted.iter().filter_map(|h| self.discovered.get(&h.id)).cloned())
            .collect();
        if self.cfg.codex_notify {
            for h in pending {
                let Some(pid) = h.pid else { continue };
                let own: Vec<String> = self
                    .held_threads_in(root, pid, procs)
                    .into_iter()
                    .map(|(id, _)| id)
                    .filter(|id| !claimed.contains(id))
                    .collect();
                if let [only] = own.as_slice() {
                    self.discovered.insert(h.id, only.clone());
                }
            }
            return;
        }
        // Each session's own directory: the start directory, or the one it
        // was started in (a worktree, a security scan's scratch tree).
        let start = self.cfg.start_dir.to_string_lossy().into_owned();
        let dir_of = |h: &HostedRef| h.cwd.clone().unwrap_or_else(|| start.clone());
        let dirs: HashSet<String> = pending.iter().map(|h| dir_of(h)).collect();
        let mut candidates: Vec<RolloutMeta> = Vec::new();
        for (path, _) in recent {
            if let Some(meta) = self.meta(path) {
                if dirs.contains(&meta.cwd) && meta.user_thread && !claimed.contains(&meta.id) {
                    self.rollout_paths
                        .insert((root.to_path_buf(), meta.id.clone()), (Some(path.clone()), Instant::now()));
                    candidates.push(meta);
                }
            }
        }
        let in_window = |h: &HostedRef, m: &RolloutMeta| {
            m.cwd == dir_of(h)
                && m.created_ms + CODEX_CLOCK_SLACK_MS >= h.spawned_ms
                && m.created_ms <= h.spawned_ms + CODEX_MATCH_WINDOW_MS
        };
        for h in &pending {
            if now_ms < h.spawned_ms + CODEX_MATCH_WINDOW_MS {
                continue;
            }
            let mine: Vec<&RolloutMeta> = candidates.iter().filter(|m| in_window(h, m)).collect();
            let [only] = mine.as_slice() else { continue };
            if pending.iter().all(|o| o.id == h.id || !in_window(o, only)) {
                self.discovered.insert(h.id, only.id.clone());
            }
        }
    }

    /// Rollouts under the Codex directory that `pid` holds open for writing.
    fn rollouts_written_by(&self, root: &Path, pid: u32) -> Vec<PathBuf> {
        // `/proc` reports resolved targets, so a symlinked or descriptor-aliased
        // Codex home (agent shells' TMPDIR is a `/proc/<pid>/fd/<n>` alias since
        // 2026-10-26) never prefixed them and no hosted Codex was matched.
        let dir = root;
        let real = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        procs::files_open_for_writing(pid, |p| {
            codex::is_rollout_path(p) && (p.starts_with(&real) || p.starts_with(dir))
        })
    }

    /// The user threads whose rollouts a hosted Codex holds open for writing,
    /// with when each file was last written (unix ms). Only the session's own
    /// Codex counts: the process the deck started and the `codex` it runs
    /// when that is a launcher. A `codex exec` it runs as a tool writes a
    /// thread of its own, and a tool such as `tail -f` reading another
    /// conversation's rollout holds it read-only (review
    /// CR-20261026-d5c080aeb2).
    fn held_threads(&mut self, pid: u32, procs: &HashMap<u32, ProcInfo>) -> Vec<(String, u64)> {
        let root = self.cfg.codex_dir.clone();
        self.held_threads_in(&root, pid, procs)
    }

    fn held_threads_in(&mut self, root: &Path, pid: u32, procs: &HashMap<u32, ProcInfo>) -> Vec<(String, u64)> {
        let owners: Vec<u32> = procs::program_tree(procs, pid, "codex")
            .into_iter()
            .filter(|p| *p == pid || procs.get(p).is_some_and(|i| i.comm == "codex"))
            .collect();
        let mut held: Vec<(String, u64)> = Vec::new();
        for p in owners {
            for path in self.rollouts_written_by(root, p) {
                let Some(meta) = self.meta(&path).filter(|m| m.user_thread) else { continue };
                if held.iter().any(|(id, _)| *id == meta.id) {
                    continue;
                }
                let written = util::modified_ms(&path).unwrap_or(0);
                self.rollout_paths.insert((root.to_path_buf(), meta.id.clone()), (Some(path), Instant::now()));
                held.push((meta.id, written));
            }
        }
        held
    }

    /// Follow embedded hosted Codex sessions into the conversation they are
    /// in now. After `/resume` or `/new` Codex keeps the earlier
    /// conversation's rollout open beside the current one (verified live
    /// 2026-10-25), and its notify names the thread only when a turn ends, so
    /// a reopen before then resumed the old conversation (review
    /// CR-20261026-3fdeb2041f). The thread written to last is the current
    /// one: resuming appends to its rollout at once, and every message and
    /// reply after that lands there. Another live session's conversation and
    /// Codex's own sub-agent threads never count.
    fn follow_codex(&mut self, hosted: &[HostedRef], procs: &HashMap<u32, ProcInfo>) {
        self.followed.clear();
        let root = self.cfg.codex_dir.clone();
        self.follow_codex_in(&root, hosted, procs);
    }

    fn follow_codex_in(&mut self, root: &Path, hosted: &[HostedRef], procs: &HashMap<u32, ProcInfo>) {
        if !self.cfg.codex_notify {
            self.follow_codex_by_name(root, hosted);
            return;
        }
        for h in hosted.iter().filter(|h| h.tool == Tool::Codex && h.alive) {
            let recorded = h.conversation.clone().or_else(|| self.discovered.get(&h.id).cloned());
            let (Some(pid), Some(recorded)) = (h.pid, recorded) else { continue };
            let others: HashSet<String> = hosted
                .iter()
                .filter(|o| o.id != h.id && o.alive)
                .filter_map(|o| o.conversation.clone().or_else(|| self.discovered.get(&o.id).cloned()))
                .collect();
            let held = self.held_threads_in(root, pid, procs);
            let recorded_at = held.iter().find(|(id, _)| *id == recorded).map(|(_, at)| *at);
            let latest = held.into_iter().filter(|(id, _)| !others.contains(id)).max_by_key(|(_, at)| *at);
            if let Some((id, at)) = latest {
                if id != recorded && recorded_at.is_none_or(|r| at > r) {
                    self.followed.insert(h.id, id);
                }
            }
        }
    }

    /// Follow hosted Codex sessions into the thread identified by their status line. Attached clients need not
    /// hold a rollout open, and a reopened client may acquire a different thread. A title is not a unique
    /// identity: accept it only when exactly one eligible user thread in the start directory matches, no other
    /// live session owns it, and its start time is compatible.
    fn follow_codex_by_name(&mut self, root: &Path, hosted: &[HostedRef]) {
        let names = if root == self.cfg.codex_dir {
            self.names.clone()
        } else {
            self.accounts.get(root).map(|a| a.names.clone()).unwrap_or_default()
        };
        for h in hosted.iter().filter(|h| h.tool == Tool::Codex && h.alive && !h.footer.is_empty()) {
            let start = h.cwd.clone().unwrap_or_else(|| self.cfg.start_dir.to_string_lossy().into_owned());
            let shows = |name: &str| h.footer.iter().any(|item| names_thread(item, name));
            let recorded = h.conversation.clone().or_else(|| self.discovered.get(&h.id).cloned());
            if recorded.as_ref().and_then(|c| names.get(c)).is_some_and(|n| shows(&n.name)) {
                continue;
            }
            let others: HashSet<String> = hosted
                .iter()
                .filter(|o| o.id != h.id && o.alive)
                .filter_map(|o| o.conversation.clone().or_else(|| self.discovered.get(&o.id).cloned()))
                .collect();
            let named: Vec<String> = names
                .iter()
                .filter(|(id, n)| {
                    recorded.as_ref() != Some(*id) && !h.left.contains(*id) && !others.contains(*id) && shows(&n.name)
                })
                .map(|(id, _)| id.clone())
                .collect();
            let mut threads = Vec::new();
            for id in named {
                let Some(path) = self.rollout_path_in(root, &id) else { continue };
                if let Some(meta) = self.meta(&path).filter(|m| m.cwd == start && m.user_thread) {
                    threads.push(meta);
                }
            }
            let since: Vec<&RolloutMeta> =
                threads.iter().filter(|m| m.created_ms + CODEX_CLOCK_SLACK_MS >= h.spawned_ms).collect();
            let pick = if since.is_empty() { threads.iter().collect() } else { since };
            if let [only] = pick.as_slice() {
                self.followed.insert(h.id, only.id.clone());
            }
        }
    }

    fn rollout_path(&mut self, id: &str) -> Option<PathBuf> {
        let root = self.cfg.codex_dir.clone();
        self.rollout_path_in(&root, id)
    }

    fn rollout_path_in(&mut self, root: &Path, id: &str) -> Option<PathBuf> {
        let key = (root.to_path_buf(), id.to_string());
        if let Some((path, at)) = self.rollout_paths.get(&key) {
            if path.is_some() || at.elapsed() < Duration::from_secs(30) {
                return path.clone();
            }
        }
        let path = codex::find_rollout(root, id);
        self.rollout_paths.insert(key, (path.clone(), Instant::now()));
        path
    }

    fn prompt_for(&mut self, path: &PathBuf, tool: Tool) -> Option<String> {
        if let Some((prompt, at)) = self.prompts.get(path) {
            if prompt.is_some() || at.elapsed() < RETRY {
                return prompt.clone();
            }
        }
        let prompt = match tool {
            Tool::Claude => claude::first_prompt(path),
            Tool::Codex => codex::first_user_message(path),
            Tool::Antigravity | Tool::Grok | Tool::OpenCode | Tool::Shell | Tool::Dynamic(_) => None,
        };
        self.prompts.insert(path.clone(), (prompt.clone(), Instant::now()));
        prompt
    }

    /// The worktree a transcript last mentions in its conversation records,
    /// read incrementally in whole lines; the task the owner gave last, and a
    /// Claude transcript's generated title and agent, are read in the same
    /// pass (`Scan`).
    fn scan_worktree(&mut self, key: String, path: PathBuf, tool: Tool) -> Option<Worktree> {
        let marker = self.cfg.conventions.worktree_task_marker.as_str();
        let describe = |entry: &WorktreeEntry| gitwt::describe(entry, marker);
        let scan = self.scans.entry(key).or_insert_with(|| Scan::new(path.clone()));
        if scan.path != path {
            *scan = Scan::new(path.clone());
        }
        let Ok((chunk, end)) = util::read_range(&path, scan.offset, SCAN_LIMIT) else {
            return scan.worktree.as_ref().map(describe);
        };
        if end > scan.offset {
            // A line still being written is read again next time; one longer
            // than the whole read is skipped.
            let complete = match chunk.iter().rposition(|&b| b == b'\n') {
                Some(last) => last + 1,
                None if chunk.len() as u64 >= SCAN_LIMIT => chunk.len(),
                None => 0,
            };
            let conversation: fn(Value) -> Option<Value> = match tool {
                Tool::Claude => claude::conversation_part,
                Tool::Codex => codex::conversation_part,
                // Never scanned as lines: these have no transcript here.
                Tool::Antigravity | Tool::Grok | Tool::OpenCode | Tool::Shell | Tool::Dynamic(_) => |_: Value| None,
            };
            let counted =
                |line: &str| serde_json::from_str::<Value>(line).ok().and_then(conversation).map(|v| v.to_string());
            if let Some(i) = gitwt::last_mention_in_lines(&chunk[..complete], &self.worktrees, counted) {
                scan.worktree = Some(self.worktrees[i].clone());
            }
            let start = end - chunk.len() as u64;
            let lines = &chunk[..complete];
            let task = match tool {
                Tool::Claude => {
                    claude::title_records(lines, &mut scan.ai_title, &mut scan.agent);
                    claude::last_task(lines)
                }
                Tool::Codex => codex::last_task(lines),
                Tool::Antigravity | Tool::Grok | Tool::OpenCode | Tool::Shell | Tool::Dynamic(_) => None,
            };
            match task {
                Some(task) => scan.found_task(start + task.pos as u64, task),
                // The read began past what was scanned before; the task
                // given last may be in what it passed over.
                None if start > scan.offset => {
                    if let Some((at, task)) = last_task_between(&path, tool, scan.offset, start) {
                        scan.found_task(at, task);
                    }
                }
                None => {}
            }
            scan.offset = start + complete as u64;
        }
        scan.worktree.as_ref().map(describe)
    }

    fn outside(
        &mut self,
        procs: &HashMap<u32, ProcInfo>,
        claude_files: &[ClaudeProc],
        hosted_pids: &HashSet<u32>,
        slow: &Slow,
    ) -> Vec<OutsideInfo> {
        let mut out = Vec::new();
        let service_pid = self.cfg.service_pid;
        for c in claude_files {
            if !c.interactive || hosted_pids.contains(&c.pid) {
                continue;
            }
            let Some(info) = procs.get(&c.pid) else { continue };
            if c.proc_start.is_some_and(|s| s != info.start_ticks) || procs::is_descendant(procs, c.pid, service_pid) {
                continue;
            }
            let cwd = c.cwd.clone().unwrap_or_default();
            let mut title = c.name.clone();
            let mut worktree = None;
            if let Some(conv) = &c.session_id {
                let path = claude::transcript_path(&self.cfg.claude_dir, &cwd, conv);
                let key = format!("p{}", c.pid);
                worktree = self.scan_worktree(key.clone(), path.clone(), Tool::Claude);
                if title.is_none() {
                    let scan = self.scans.get(&key);
                    title = scan
                        .and_then(|s| s.ai_title.clone().or_else(|| s.task.as_ref().map(|t| t.title.clone())))
                        .or_else(|| self.prompt_for(&path, Tool::Claude));
                }
            }
            out.push(outside_info(
                Tool::Claude,
                info,
                title.unwrap_or_else(|| "Claude".into()),
                c.busy,
                cwd,
                c.session_id.clone(),
                worktree,
                slow,
            ));
        }
        for (pid, info) in procs {
            if info.comm != "codex" || hosted_pids.contains(pid) || !procs::stdin_is_tty(*pid) {
                continue;
            }
            if procs.get(&info.ppid).is_some_and(|p| p.comm == "codex")
                || procs::is_descendant(procs, *pid, service_pid)
            {
                continue;
            }
            let args = procs::cmdline(*pid);
            const SKIP: [&str; 6] = ["exec", "app-server", "mcp-server", "proto", "login", "logout"];
            if args.iter().skip(1).any(|a| SKIP.contains(&a.as_str())) {
                continue;
            }
            // Embedded Codex owns its writable rollout even when launched
            // without `resume`, and after it changes conversation. A shared
            // server owns many clients' rollouts, so never inspect that server
            // as if its newest file belonged to this terminal.
            let held = self.held_threads(*pid, procs);
            let conv = match held.iter().map(|(_, at)| *at).max() {
                Some(latest) => {
                    let mut current = held.iter().filter(|(_, at)| *at == latest);
                    let first = current.next().map(|(id, _)| id.clone());
                    if current.next().is_none() {
                        first
                    } else {
                        None
                    }
                }
                None => resume_id(&args),
            };
            let mut title = None;
            let mut busy = None;
            let mut worktree = None;
            if let Some(conv) = &conv {
                let name = self.names.get(conv).cloned();
                if let Some(path) = self.rollout_path(conv) {
                    busy = self.codex_turn(&path);
                    let key = format!("p{pid}");
                    worktree = self.scan_worktree(key.clone(), path.clone(), Tool::Codex);
                    let task = self.scans.get(&key).and_then(|s| s.task.clone());
                    title = name
                        .filter(|n| !task.as_ref().is_some_and(|t| n.outdated_by(t)))
                        .map(|n| n.name)
                        .or(task.map(|t| t.title))
                        .or_else(|| self.prompt_for(&path, Tool::Codex));
                } else {
                    title = name.map(|n| n.name);
                }
            }
            if conv.as_ref().is_some_and(|id| self.active_goals.as_ref().is_some_and(|goals| goals.contains(id))) {
                busy = Some(true);
            }
            let cwd = procs::cwd(*pid).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            out.push(outside_info(
                Tool::Codex,
                info,
                title.unwrap_or_else(|| "Codex".into()),
                busy,
                cwd,
                conv,
                worktree,
                slow,
            ));
        }
        out.sort_by_key(|o| (o.tool == Tool::Codex, o.started_at, o.pid));
        out
    }

    /// Claude's windows, read again only when the status lines rewrote the
    /// snapshot; each cycle otherwise costs one `stat`.
    fn claude_usage(&mut self) -> Vec<UsageWindow> {
        let stamp = util::file_stamp(&self.cfg.usage_file);
        if stamp != self.claude_snapshot.0 {
            let windows = fs::read(&self.cfg.usage_file)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .map(|v| claude_windows(&v, 0))
                .unwrap_or_default();
            self.claude_snapshot = (stamp, windows);
        }
        let now = util::now_secs();
        self.claude_snapshot.1.iter().filter(|w| w.resets_at.is_none_or(|r| r >= now)).cloned().collect()
    }

    /// Where Codex usage samples can be: the rollouts of the newest days, and
    /// every rollout a `codex` process holds open for writing, from whatever
    /// day it began. Rollouts are filed by the day their conversation began,
    /// so a conversation resumed from last week writes into last week's
    /// directory (review CR-20261026-d5c080aeb2). Newest first.
    fn usage_candidates(&self, recent: &[(PathBuf, u64)], procs: &HashMap<u32, ProcInfo>) -> Vec<(PathBuf, u64)> {
        let mut out = recent.to_vec();
        for info in procs.values().filter(|i| i.comm == "codex") {
            for path in self.rollouts_written_by(&self.cfg.codex_dir, info.pid) {
                if !out.iter().any(|(p, _)| *p == path) {
                    let modified = util::modified_ms(&path).unwrap_or(0);
                    out.push((path, modified));
                }
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.1));
        out
    }

    /// The windows of the newest Codex usage sample in the candidate
    /// rollouts, judged by when Codex recorded it: every session shares the
    /// account's limits, and the file written last can hold an older sample
    /// than another session's (review CR-20261026-3fdeb2041f). Files come
    /// newest first, and one last written before the best sample so far
    /// cannot hold a newer one, so the search stops there and nowhere sooner
    /// (review CR-20261026-d5c080aeb2).
    fn codex_usage(&mut self, recent: &[(PathBuf, u64)]) -> Vec<UsageWindow> {
        let now = util::now_secs();
        let mut best = self.codex_usage.take();
        for (path, modified_ms) in recent {
            if best.as_ref().is_some_and(|(at, _)| modified_ms + CODEX_CLOCK_SLACK_MS < *at) {
                break;
            }
            let sample = match self.codex_samples.get(path) {
                Some((read_at, sample)) if read_at == modified_ms => sample.clone(),
                _ => {
                    let sample = util::read_tail(path, 512 * 1024)
                        .ok()
                        .and_then(|tail| codex::rate_limits(&tail, *modified_ms, now));
                    self.codex_samples.insert(path.clone(), (*modified_ms, sample.clone()));
                    sample
                }
            };
            if let Some(sample) = sample.filter(|s| best.as_ref().is_none_or(|(at, _)| s.0 > *at)) {
                best = Some(sample);
            }
        }
        self.codex_samples.retain(|path, _| recent.iter().any(|(p, _)| p == path));
        self.codex_usage = best;
        self.codex_usage
            .as_ref()
            .map(|(_, w)| w.iter().filter(|w| w.resets_at.is_none_or(|r| r >= now)).cloned().collect())
            .unwrap_or_default()
    }
}

/// Usage windows from the snapshot the status-line helper writes.
pub fn claude_windows(v: &Value, now: u64) -> Vec<UsageWindow> {
    let updated_at = v.get("updated_at").and_then(util::json_epoch_secs).unwrap_or(0);
    let Some(rl) = v.get("rate_limits").filter(|r| r.is_object()) else { return Vec::new() };
    let mut out = Vec::new();
    for (key, label) in [("five_hour", "5h"), ("seven_day", "wk")] {
        let Some(w) = rl.get(key).filter(|w| w.is_object()) else { continue };
        let Some(used) = w.get("used_percentage").and_then(Value::as_f64) else { continue };
        let resets_at = w.get("resets_at").and_then(util::json_epoch_secs);
        if resets_at.is_some_and(|r| r < now) {
            continue;
        }
        out.push(UsageWindow { label: label.to_string(), used_percent: Some(used), resets_at, updated_at });
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn outside_info(
    tool: Tool,
    info: &ProcInfo,
    title: String,
    busy: Option<bool>,
    cwd: String,
    conversation: Option<String>,
    worktree: Option<Worktree>,
    slow: &Slow,
) -> OutsideInfo {
    let terminal = external::terminal_info(info.pid, &slow.workspaces);
    OutsideInfo {
        tool,
        pid: info.pid,
        title,
        busy,
        cwd,
        conversation,
        started_at: procs::start_secs(info.start_ticks),
        start_ticks: info.start_ticks,
        kitty_pid: terminal.pid,
        kitty_window: terminal.window,
        kitty_socket: terminal.socket,
        workspace: terminal.workspace,
        worktree,
        host: None,
    }
}

/// Identify a saved user thread by the ID in the selected terminal's live footer.
pub fn codex_screen_conversation(codex_dir: &Path, cwd: &str, screen: &str) -> Option<String> {
    let lines: Vec<String> = screen.lines().map(str::to_string).collect();
    let footer = crate::status::codex_footer(&lines, None);
    let valid = |id: &str| {
        crate::service::valid_conversation_id(id)
            && codex::user_rollout(codex_dir, id)
                .and_then(|path| codex::read_meta(&path))
                .is_some_and(|meta| meta.cwd == cwd && meta.id == id)
    };
    let mut exact: Vec<String> = footer
        .iter()
        .flat_map(|item| item.split_whitespace())
        .filter(|id| {
            id.len() == 36
                && id.bytes().enumerate().all(|(at, byte)| {
                    if [8, 13, 18, 23].contains(&at) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                })
        })
        .map(str::to_string)
        .collect();
    exact.sort();
    exact.dedup();
    if let [only] = exact.as_slice() {
        return valid(only).then(|| only.clone());
    }
    None
}

/// Whether an item of Codex's status line shows thread `name`, whole or cut
/// short with "…" in a narrow pane.
fn names_thread(item: &str, name: &str) -> bool {
    /// A shorter cut names too many threads.
    const MIN_CUT: usize = 8;
    match item.strip_suffix('…') {
        Some(cut) => cut.chars().count() >= MIN_CUT && name.starts_with(cut.trim_end()),
        None => item == name,
    }
}

#[cfg(test)]
#[path = "../../src/tests/monitor.rs"]
mod tests;
