//! Splitting a test selection across the build hosts' free cores.
//!
//! The selection is either the targets given, or what the project's CI would
//! select for the worktree's changes: the `offload.plan` command, run on a
//! build host, turns the changed paths into suites of test targets and names
//! what it cannot split. Each host takes one shard process per free
//! offload slot within the half of its cores offloaded work may use
//! (`slots_sh`); every shard process holds its slot while it runs, so callers
//! together never exceed that half. Targets are dealt longest first, by the
//! durations recorded on earlier runs, to the least loaded shard. Every target is in exactly one
//! shard, and a shard that does not reach its runner's summary leaves its
//! targets reported as not run: none passes by being dropped.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::hosts::{self, BuildHost, Load};
use super::lease::Lease;
use super::results::{self, Outcome, Runner};
use super::sync::{self, PythonLock, Worktree};
use super::{assignment, offload_config, option_value, remote_command, slots_sh, ssh_command, RemoteRun, Slots};
use crate::config::OffloadConfig;

/// Tests run one way with one environment.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Suite {
    pub name: String,
    pub runner: Runner,
    pub targets: Vec<String>,
    /// `NAME=VALUE` for every shard and setup command.
    #[serde(default, deserialize_with = "assignments")]
    pub environment: Vec<String>,
    /// Commands each host runs once before its shards of this suite.
    #[serde(default)]
    pub setup: Vec<Vec<String>>,
}

fn assignments<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    let map = BTreeMap::<String, String>::deserialize(d)?;
    Ok(map.into_iter().map(|(k, v)| format!("{k}={v}")).collect())
}

/// What the plan selected but this command does not run, and how to run it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NotRun {
    pub name: String,
    pub reason: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Plan {
    pub version: u32,
    pub suites: Vec<Suite>,
    #[serde(default)]
    pub not_run: Vec<NotRun>,
}

/// One shard: a host and the targets it runs, as (suite, target) indices.
#[derive(Debug, Clone, PartialEq)]
pub struct Shard {
    pub host: usize,
    pub items: Vec<(usize, usize)>,
    pub estimate: f64,
}

/// Deal targets to shards. `slots[h]` is how many shards host `h` takes,
/// `eligible[s]` the hosts that may run suite `s`, and `estimates[s][t]` the
/// expected seconds of target `t` of suite `s`. Longest first, each to the
/// least loaded shard that may take it; shards are interleaved across hosts
/// so a small selection still spreads out. Empty shards are left out.
pub fn split(slots: &[usize], eligible: &[Vec<usize>], estimates: &[Vec<f64>]) -> Vec<Shard> {
    let deepest = slots.iter().copied().max().unwrap_or(0);
    let mut shards: Vec<Shard> = Vec::new();
    for depth in 0..deepest {
        for (host, count) in slots.iter().enumerate() {
            if depth < *count {
                shards.push(Shard { host, items: Vec::new(), estimate: 0.0 });
            }
        }
    }
    let mut items: Vec<(usize, usize, f64)> = Vec::new();
    for (suite, targets) in estimates.iter().enumerate() {
        for (target, estimate) in targets.iter().enumerate() {
            items.push((suite, target, *estimate));
        }
    }
    // Longest first; equal ones in selection order, so a split repeats.
    items.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal).then((a.0, a.1).cmp(&(b.0, b.1))));
    for (suite, target, estimate) in items {
        let allowed = &eligible[suite];
        let best = shards
            .iter()
            .enumerate()
            .filter(|(_, s)| allowed.contains(&s.host))
            .min_by(|a, b| a.1.estimate.partial_cmp(&b.1.estimate).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i);
        if let Some(i) = best {
            shards[i].items.push((suite, target));
            shards[i].estimate += estimate;
        }
    }
    shards.retain(|s| !s.items.is_empty());
    for shard in &mut shards {
        shard.items.sort();
    }
    shards
}

/// Each host's shards (`Load::slots`), for hosts in ranked order. A host
/// named with `--host` takes at least one. When no host has a slot to spare,
/// the first one `auto` may use takes one, which waits for a slot there; when
/// none may be used, every share is 0.
pub fn slots(loads: &[Load], named: bool) -> Vec<usize> {
    let mut slots: Vec<usize> =
        loads.iter().map(|load| if named { load.slots().max(1) } else { load.slots() }).collect();
    if slots.iter().all(|s| *s == 0) {
        if let Some(first) = loads.iter().position(Load::eligible) {
            slots[first] = 1;
        }
    }
    slots
}

/// Recorded seconds per target, by runner, from earlier shards.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    pub seconds: BTreeMap<String, f64>,
}

/// What a target nothing is known about is expected to take.
const UNKNOWN_SECONDS: f64 = 10.0;

impl History {
    fn key(runner: Runner, target: &str) -> String {
        format!("{} {target}", runner.name())
    }

    pub fn load(path: &Path) -> History {
        let seconds = fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        History { seconds }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::util::write_atomic(path, &serde_json::to_vec(&self.seconds)?)?;
        Ok(())
    }

    /// The recorded time, else the median of the runner's recorded times.
    pub fn estimate(&self, runner: Runner, target: &str) -> f64 {
        if let Some(seconds) = self.seconds.get(&Self::key(runner, target)) {
            return *seconds;
        }
        let prefix = format!("{} ", runner.name());
        let mut known: Vec<f64> =
            self.seconds.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(_, v)| *v).collect();
        if known.is_empty() {
            return UNKNOWN_SECONDS;
        }
        known.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        known[known.len() / 2]
    }

    /// Share a finished shard's time among its targets in proportion to what
    /// they were expected to take, and average that into the record.
    pub fn record(&mut self, runner: Runner, targets: &[(String, f64)], seconds: f64) {
        let expected: f64 = targets.iter().map(|(_, e)| e.max(0.001)).sum();
        for (target, estimate) in targets {
            let observed = seconds * estimate.max(0.001) / expected;
            let key = Self::key(runner, target);
            let value = match self.seconds.get(&key) {
                Some(old) => (old + observed) / 2.0,
                None => observed,
            };
            self.seconds.insert(key, value);
        }
    }
}

const USAGE: &str = "\
usage: cepheus shard [--host auto|NAME[,NAME]...] --select ci [--base REV]
       cepheus shard [--host ...] --runner unittest|django|pytest [--env NAME=VALUE]...
                     [--targets-from FILE|-] [TARGET]... [-- RUNNER-ARG...]

Split tests across the build hosts and report every failure. Each host runs
one shard per offload slot it has free, within half its cores; --host auto
leaves out a host whose own work already keeps half its cores busy.
--select ci runs the suites CI would select for this worktree's changes
against REV (default: where HEAD left the offload.base branch), as the command in the config's
offload.plan setting lists them, and names the selected checks it cannot
split. Otherwise the targets given (dotted names, or files and node ids for
pytest) run with the chosen runner from the worktree root. It exits 0 only
when every target it ran finished and passed; the checks it names as not run
leave that to CI. Results are never verification receipts and satisfy no CI
gate.";

#[derive(Debug, Default, PartialEq)]
pub struct ShardArgs {
    pub host: String,
    pub select: Option<String>,
    pub base: Option<String>,
    pub runner: Option<Runner>,
    pub environment: Vec<String>,
    pub targets: Vec<String>,
    pub targets_from: Option<String>,
    pub extra: Vec<String>,
}

pub fn parse_args(args: &[String]) -> Result<ShardArgs> {
    let mut parsed = ShardArgs { host: "auto".into(), ..ShardArgs::default() };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                parsed.extra = args[i + 1..].to_vec();
                break;
            }
            "--host" => parsed.host = option_value(args, &mut i)?,
            "--select" => {
                let value = option_value(args, &mut i)?;
                if value != "ci" {
                    bail!("--select takes ci");
                }
                parsed.select = Some(value);
            }
            "--base" => parsed.base = Some(option_value(args, &mut i)?),
            "--runner" => {
                let value = option_value(args, &mut i)?;
                parsed.runner = Some(Runner::parse(&value).with_context(|| format!("unknown runner {value}"))?);
            }
            "--env" => parsed.environment.push(assignment(args, &mut i)?),
            "--targets-from" => parsed.targets_from = Some(option_value(args, &mut i)?),
            other if other.starts_with('-') => bail!("unknown option {other}"),
            target => parsed.targets.push(target.to_string()),
        }
        i += 1;
    }
    match (&parsed.select, parsed.runner) {
        (Some(_), Some(_)) => bail!("--select ci chooses its own runners; leave out --runner"),
        (Some(_), None) if !parsed.targets.is_empty() || parsed.targets_from.is_some() || !parsed.extra.is_empty() => {
            bail!("--select ci chooses its own targets")
        }
        (None, None) => bail!("give --select ci or --runner"),
        (None, Some(_)) if parsed.base.is_some() => bail!("--base goes with --select ci"),
        _ => {}
    }
    Ok(parsed)
}

pub fn cli(args: &[String]) -> i32 {
    if args.iter().take_while(|a| *a != "--").any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return 0;
    }
    match parse_args(args).and_then(|parsed| shard(&parsed, &offload_config()?)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("cepheus shard: {err:#}");
            1
        }
    }
}

/// Targets one per line, from a file or standard input.
fn read_targets(source: &str) -> Result<Vec<String>> {
    let text = if source == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        text
    } else {
        fs::read_to_string(source).with_context(|| format!("reading {source}"))?
    };
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect())
}

/// The paths the worktree changed from `base`: committed, uncommitted,
/// deleted and new, as CI's selector reads them (both sides of renames).
fn changed_paths(tree: &Worktree, base: Option<&str>, branch: &str) -> Result<Vec<u8>> {
    let git = |args: &[&str]| -> Result<Vec<u8>> {
        let out = Command::new("git").args(args).current_dir(&tree.root).stdin(Stdio::null()).output()?;
        if !out.status.success() {
            bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    };
    let base = match base {
        Some(base) => base.to_string(),
        None => String::from_utf8_lossy(&git(&["merge-base", "HEAD", branch])?).trim().to_string(),
    };
    let mut paths = git(&["diff", "--name-only", "--no-renames", "-z", &base])?;
    paths.extend(git(&["ls-files", "-z", "-o", "--exclude-standard"])?);
    Ok(paths)
}

/// The `offload.plan` command, which `--select ci` needs.
fn plan_command(cfg: &OffloadConfig) -> Result<&[String]> {
    if cfg.plan.is_empty() {
        bail!(
            "--select ci needs the offload.plan setting in {}: the command that prints the shard plan, \
             reading the changed paths on standard input",
            crate::paths::config_path().display()
        );
    }
    Ok(cfg.plan.as_slice())
}

/// Runs the plan command from a mirror, in the project's locked Python
/// environment as `RUN` gives a `--python` command, holding a slot while it
/// imports the tests. Its standard input is the caller's, carrying the
/// changed paths. `@STATE@` in the command is the mirror's own state
/// directory.
const PLAN: &str = concat!(
    r#"set -euo pipefail
container=$1; shift
"#,
    slots_sh!(),
    r#"take_slots one
state=/scratch/source/.cepheus-offload/state
podman --remote=false exec --interactive --workdir /scratch/source "$container" bash -c '
export VIRTUAL_ENV=/scratch/source/.venv PATH="/scratch/source/.venv/bin:$PATH" PYTHONPATH="/scratch/source${PYTHONPATH:+:$PYTHONPATH}"
exec "$@"' cepheus-offload "${@//@STATE@/$state}""#
);

/// `PLAN`'s positional parameters: the mirror, then `command` after `wrap`.
fn plan_args<'a>(mirror: &'a str, wrap: &'a [String], command: &'a [String]) -> Vec<&'a str> {
    let mut args: Vec<&str> = vec![mirror];
    args.extend(wrap.iter().chain(command).map(String::as_str));
    args
}

/// Ask a host for the plan: `command` (`offload.plan`) after `wrap`
/// (`offload.wrap`), reading the changed paths on standard input (`PLAN`).
fn plan(host: &BuildHost, mirror: &str, wrap: &[String], command: &[String], paths: &[u8]) -> Result<Plan> {
    let name = command.join(" ");
    let args = plan_args(mirror, wrap, command);
    let mut child = ssh_command(host)
        .arg(remote_command(PLAN, &args))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut pipe = child.stdin.take().expect("piped");
    let input = paths.to_vec();
    let writer = thread::spawn(move || {
        let _ = std::io::Write::write_all(&mut pipe, &input);
    });
    let out = child.wait_with_output()?;
    let _ = writer.join();
    if !out.status.success() {
        bail!("the shard plan ({name}) on {} failed ({})", host.name, out.status);
    }
    let plan: Plan = serde_json::from_slice(&out.stdout).with_context(|| format!("reading the shard plan ({name})"))?;
    if plan.version != 1 {
        bail!("the shard plan ({name}) is version {}; this build reads version 1", plan.version);
    }
    Ok(plan)
}

/// A finished process in this run, for the report.
struct Ran {
    label: String,
    host: String,
    log: PathBuf,
    code: Option<i32>,
    outcome: Outcome,
    seconds: f64,
}

impl Ran {
    fn passed(&self) -> bool {
        self.code == Some(0) && self.outcome.complete
    }
}

/// Where this run's logs go: a new folder under the deck's state, keeping
/// the newest twenty runs.
fn log_dir() -> Result<PathBuf> {
    let runs = crate::paths::state_dir().join("offload/runs");
    fs::create_dir_all(&runs)?;
    let mut old: Vec<PathBuf> = fs::read_dir(&runs)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    old.sort();
    while old.len() >= 20 {
        let _ = fs::remove_dir_all(old.remove(0));
    }
    let stamp = time::OffsetDateTime::now_utc();
    let name = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{}",
        stamp.year(),
        stamp.month() as u8,
        stamp.day(),
        stamp.hour(),
        stamp.minute(),
        stamp.second(),
        std::process::id()
    );
    let dir = runs.join(name);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Run `argv` on a host in the mirror's locked Python environment, after
/// `wrap` (`offload.wrap`), all output to `log`.
fn run_logged(
    host: &BuildHost,
    mirror: &str,
    environment: &[String],
    wrap: &[String],
    argv: &[String],
    log: &Path,
) -> Result<(Option<i32>, f64)> {
    let file = fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
    let started = Instant::now();
    let remote = RemoteRun { host, mirror, cwd: "", python: Some(wrap), slots: Slots::One, environment, argv };
    let status = remote.status(Stdio::from(file.try_clone()?), Stdio::from(file))?;
    Ok((status.code(), started.elapsed().as_secs_f64()))
}

fn shard(args: &ShardArgs, cfg: &OffloadConfig) -> Result<i32> {
    let planned = match args.runner {
        Some(_) => None,
        None => Some(plan_command(cfg)?),
    };
    let tree = Worktree::current()?;
    let explicit = match args.runner {
        Some(runner) => {
            let mut targets = args.targets.clone();
            if let Some(source) = &args.targets_from {
                targets.extend(read_targets(source)?);
            }
            if targets.is_empty() {
                bail!("no targets given");
            }
            let mut seen = std::collections::HashSet::new();
            targets.retain(|target| seen.insert(target.clone()));
            Some(Suite {
                name: runner.name().to_string(),
                runner,
                targets,
                environment: args.environment.clone(),
                setup: Vec::new(),
            })
        }
        None => None,
    };
    let changed = match explicit {
        Some(_) => None,
        None => Some(changed_paths(&tree, args.base.as_deref(), &cfg.base)?),
    };
    let lock = PythonLock::read(&tree.root, cfg)?;
    let chosen = hosts::choose(&args.host, &hosts::configured())?;
    let ranked = hosts::ranked(&chosen, "shard")?;
    let loads: Vec<Load> = ranked.iter().map(|(_, load)| *load).collect();
    let shares = slots(&loads, args.host != "auto");
    let mut taking: Vec<(BuildHost, usize)> = Vec::new();
    for ((host, load), share) in ranked.into_iter().zip(shares) {
        let verdict = if share == 0 { "left out".to_string() } else { format!("takes {share}") };
        eprintln!("[shard] {}; {verdict}", hosts::describe(&host, &load));
        if share > 0 {
            taking.push((host, share));
        }
    }
    if taking.is_empty() {
        bail!("every build host has half its cores or more busy with its own work");
    }

    // Copy the worktree and prepare the environment everywhere at once.
    let files = Arc::new(tree.files(&[])?);
    let mirror = tree.mirror();
    let (tx, rx) = mpsc::channel();
    for (index, (host, _)) in taking.iter().enumerate() {
        let (tx, host, tree, mirror, files, lock, container) = (
            tx.clone(),
            host.clone(),
            tree.clone(),
            mirror.clone(),
            files.clone(),
            lock.clone(),
            cfg.container.clone(),
        );
        thread::spawn(move || {
            let started = Instant::now();
            let ready = (|| {
                let lease = Lease::start(&host, &mirror, &container)?;
                sync::sync(&lease, &tree, &files)?;
                sync::python_env(&lease, &lock)?;
                Ok::<_, anyhow::Error>((lease, started.elapsed().as_secs_f64()))
            })();
            let _ = tx.send((index, ready));
        });
    }
    drop(tx);
    let mut ready = vec![false; taking.len()];
    let mut leases: Vec<Option<Lease>> = (0..taking.len()).map(|_| None).collect();
    for (index, result) in rx {
        match result {
            Ok((lease, seconds)) => {
                leases[index] = Some(lease);
                ready[index] = true;
                eprintln!("[shard] {} ready ({seconds:.0}s)", taking[index].0.name);
            }
            Err(err) => eprintln!("[shard] {} skipped: {err:#}", taking[index].0.name),
        }
    }
    let usable: Vec<(BuildHost, usize)> =
        taking.into_iter().zip(&ready).filter(|(_, ok)| **ok).map(|(host, _)| host).collect();
    if usable.is_empty() {
        bail!("no build host could take the worktree");
    }

    let plan = match (explicit, changed, planned) {
        (Some(suite), _, _) => Plan { version: 1, suites: vec![suite], not_run: Vec::new() },
        (None, Some(paths), Some(command)) => plan(&usable[0].0, &mirror, &cfg.wrap, command, &paths)?,
        (None, _, _) => unreachable!("a selection is either given or planned"),
    };
    let suites: Vec<Suite> = plan.suites.into_iter().filter(|s| !s.targets.is_empty()).collect();
    if suites.is_empty() {
        report_not_run(&plan.not_run);
        println!("No test suites to shard.");
        for lease in leases.iter().flatten() {
            lease.finish()?;
        }
        return Ok(0);
    }
    let logs = log_dir()?;

    // Each suite's setup on every host; a host whose setup fails does not
    // run that suite.
    let mut setups: Vec<Ran> = Vec::new();
    let mut eligible: Vec<Vec<usize>> = vec![Vec::new(); suites.len()];
    let (tx, rx) = mpsc::channel();
    for (h, (host, _)) in usable.iter().enumerate() {
        let (tx, host, mirror, suites, logs, wrap) =
            (tx.clone(), host.clone(), mirror.clone(), suites.clone(), logs.clone(), cfg.wrap.clone());
        thread::spawn(move || {
            for (s, suite) in suites.iter().enumerate() {
                let mut failed = None;
                for (n, argv) in suite.setup.iter().enumerate() {
                    let log = logs.join(format!("setup-{}-{}-{n}.log", safe(&suite.name), safe(&host.name)));
                    let done = run_logged(&host, &mirror, &suite.environment, &wrap, argv, &log);
                    let (code, seconds) = done.unwrap_or((None, 0.0));
                    if code != Some(0) {
                        failed = Some(Ran {
                            label: format!("{} setup: {}", suite.name, argv.join(" ")),
                            host: host.name.clone(),
                            log,
                            code,
                            outcome: Outcome::default(),
                            seconds,
                        });
                        break;
                    }
                }
                let _ = tx.send((h, s, failed));
            }
        });
    }
    drop(tx);
    for (h, s, failed) in rx {
        match failed {
            None => eligible[s].push(h),
            Some(ran) => {
                eprintln!(
                    "[shard] {} cannot run {}: setup failed, see {}",
                    ran.host,
                    suites[s].name,
                    ran.log.display()
                );
                setups.push(ran);
            }
        }
    }
    for list in &mut eligible {
        list.sort();
    }

    let history_path = crate::paths::state_dir().join("offload/durations.json");
    let mut history = History::load(&history_path);
    let estimates: Vec<Vec<f64>> =
        suites.iter().map(|s| s.targets.iter().map(|t| history.estimate(s.runner, t)).collect()).collect();
    let shares: Vec<usize> = usable.iter().map(|(_, share)| *share).collect();
    let shards = split(&shares, &eligible, &estimates);
    let mut placed = vec![vec![false; 0]; suites.len()];
    for (s, suite) in suites.iter().enumerate() {
        placed[s] = vec![false; suite.targets.len()];
    }
    for shard in &shards {
        for (s, t) in &shard.items {
            placed[*s][*t] = true;
        }
    }
    let per_host: Vec<usize> = (0..usable.len()).map(|h| shards.iter().filter(|s| s.host == h).count()).collect();
    let spread: Vec<String> = usable
        .iter()
        .zip(&per_host)
        .filter(|(_, n)| **n > 0)
        .map(|((host, _), n)| format!("{} on {}", n, host.name))
        .collect();
    let total: usize = suites.iter().map(|s| s.targets.len()).sum();
    eprintln!("[shard] {total} targets in {} shards: {}; logs in {}", shards.len(), spread.join(", "), logs.display());

    // Every shard at once; within a shard, one process per suite.
    let (tx, rx) = mpsc::channel();
    for (n, shard) in shards.iter().enumerate() {
        let host = usable[shard.host].0.clone();
        let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
        for (s, t) in &shard.items {
            match groups.last_mut() {
                Some((suite, targets)) if suite == s => targets.push(*t),
                _ => groups.push((*s, vec![*t])),
            }
        }
        let (tx, mirror, suites, logs, extra, wrap) =
            (tx.clone(), mirror.clone(), suites.clone(), logs.clone(), args.extra.clone(), cfg.wrap.clone());
        thread::spawn(move || {
            for (s, targets) in groups {
                let suite = &suites[s];
                let names: Vec<String> = targets.iter().map(|t| suite.targets[*t].clone()).collect();
                let argv = suite.runner.command(&extra, &names);
                let log = logs.join(format!("{}-{n}-{}.log", safe(&suite.name), safe(&host.name)));
                let (code, seconds) =
                    run_logged(&host, &mirror, &suite.environment, &wrap, &argv, &log).unwrap_or((None, 0.0));
                let text = fs::read(&log).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                let outcome = results::parse(suite.runner, &text);
                let ran = Ran {
                    label: format!("{} shard {n}", suite.name),
                    host: host.name.clone(),
                    log,
                    code,
                    outcome,
                    seconds,
                };
                let _ = tx.send((s, targets, ran));
            }
        });
    }
    drop(tx);
    let mut finished: Vec<(usize, Vec<usize>, Ran)> = Vec::new();
    for (s, targets, ran) in rx {
        let state = if ran.passed() { "ok  " } else { "FAIL" };
        let count = ran.outcome.ran.map(|n| format!("{n} tests")).unwrap_or_else(|| "no summary".to_string());
        eprintln!("[shard] {state} {} on {}: {count}, {:.0}s", ran.label, ran.host, ran.seconds);
        if ran.outcome.complete {
            let timed: Vec<(String, f64)> =
                targets.iter().map(|t| (suites[s].targets[*t].clone(), estimates[s][*t])).collect();
            history.record(suites[s].runner, &timed, ran.seconds);
        }
        finished.push((s, targets, ran));
    }
    if let Err(err) = history.save(&history_path) {
        eprintln!("[shard] durations not recorded: {err:#}");
    }

    // The report.
    let mut failed = false;
    let mut failures: Vec<String> = Vec::new();
    let mut unfinished: Vec<String> = Vec::new();
    for (s, targets, ran) in &finished {
        if !ran.passed() {
            failed = true;
            for name in &ran.outcome.failures {
                failures.push(format!("{}: {name}", suites[*s].name));
            }
            if !ran.outcome.complete {
                unfinished.extend(targets.iter().map(|t| format!("{}: {}", suites[*s].name, suites[*s].targets[*t])));
            } else if ran.outcome.failures.is_empty() {
                failures.push(format!("{}: exited {:?} ({})", suites[*s].name, ran.code, ran.label));
            }
            println!("--- {} on {} (exit {:?}), last lines of {}:", ran.label, ran.host, ran.code, ran.log.display());
            println!("{}", tail(&ran.log, 60));
        }
    }
    for (s, suite) in suites.iter().enumerate() {
        for (t, target) in suite.targets.iter().enumerate() {
            if !placed[s][t] {
                failed = true;
                unfinished.push(format!("{}: {target} (no host could run it)", suite.name));
            }
        }
    }
    for ran in &setups {
        println!("--- {} on {} (exit {:?}), last lines of {}:", ran.label, ran.host, ran.code, ran.log.display());
        println!("{}", tail(&ran.log, 30));
    }
    let tests: u64 = finished.iter().filter_map(|(_, _, r)| r.outcome.ran).sum();
    println!();
    println!("{} shards on {} hosts ran {tests} tests; logs in {}", finished.len(), spread.len(), logs.display());
    if !failures.is_empty() {
        println!("Failed ({}):", failures.len());
        for failure in &failures {
            println!("  {failure}");
        }
    }
    if !unfinished.is_empty() {
        println!("Not run to completion ({}):", unfinished.len());
        for target in &unfinished {
            println!("  {target}");
        }
    }
    report_not_run(&plan.not_run);
    let left = if plan.not_run.is_empty() {
        String::new()
    } else {
        format!(" ({} selected checks not run)", plan.not_run.len())
    };
    for lease in leases.iter().flatten() {
        lease.finish()?;
    }
    if failed {
        println!("FAILED{left}");
        Ok(1)
    } else {
        println!("OK{left}");
        Ok(0)
    }
}

fn report_not_run(not_run: &[NotRun]) {
    if not_run.is_empty() {
        return;
    }
    println!("Selected but not run by cepheus shard ({}):", not_run.len());
    for item in not_run {
        println!("  {}: {} ({})", item.name, item.reason, item.command);
    }
}

fn safe(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

fn tail(path: &Path, lines: usize) -> String {
    let bytes = crate::util::read_tail(path, 64 * 1024).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
#[path = "../tests/offload_shard.rs"]
mod tests;
