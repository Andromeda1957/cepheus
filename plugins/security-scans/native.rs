//! Codex Security scans run by the deck.
//!
//! A scan checks out the start repository's current commit, detached, in a scratch worktree under the scans
//! folder (`Config::scans_dir`), and runs a hosted Codex there that is asked to run the codex-security plugin's
//! scan and write its files to the scan's `report` folder. Once the report is written and Codex's turn is over,
//! or the session ends, the worktree is removed and the report stays, with `scan.json` describing it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::protocol::ScanInfo;
use crate::{run, util};

/// The scan's own description, beside its report.
pub const SCAN_FILE: &str = "scan.json";
/// The report codex-security writes, which marks the scan saved.
pub const REPORT_FILE: &str = "report.md";
/// At most this many scans are listed.
pub const MAX_LISTED: usize = 50;
/// A `scan.json` larger than this is not read.
const SCAN_FILE_LIMIT: u64 = 64 * 1024;
/// How long git may take to check out or remove the scratch worktree.
const GIT_LIMIT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScanJob {
    /// The folder's name: when it started, and a random suffix.
    pub id: String,
    /// The repository scanned and the commit checked out.
    pub repo: PathBuf,
    #[serde(default)]
    pub head: Option<String>,
    /// A path inside the repository the scan is limited to.
    #[serde(default)]
    pub scope: Option<String>,
    pub started_at: u64,
    #[serde(default)]
    pub finished_at: Option<u64>,
    /// The hosted Codex running it.
    #[serde(default)]
    pub session: Option<u64>,
    /// "preparing", "running", "saved", "stopped" or "failed: why".
    pub state: String,
    /// Where it is kept.
    pub dir: PathBuf,
}

impl ScanJob {
    pub fn worktree(&self) -> PathBuf {
        self.dir.join("src")
    }

    pub fn out(&self) -> PathBuf {
        self.dir.join("report")
    }

    pub fn report(&self) -> PathBuf {
        self.out().join(REPORT_FILE)
    }

    /// The tag on its session (`SessionInfo::job`).
    pub fn tag(&self) -> String {
        format!("scan:{}", self.id)
    }

    pub fn running(&self) -> bool {
        matches!(self.state.as_str(), "preparing" | "running")
    }

    /// Whether Codex has written a report that is not empty.
    pub fn report_written(&self) -> bool {
        fs::metadata(self.report()).is_ok_and(|m| m.is_file() && m.len() > 0)
    }

    pub fn save(&self) -> Result<()> {
        let data = serde_json::to_vec_pretty(self)?;
        util::write_atomic(&self.dir.join(SCAN_FILE), &data).with_context(|| format!("saving {}", self.dir.display()))
    }

    pub fn info(&self, host: Option<&str>) -> ScanInfo {
        ScanInfo {
            id: self.id.clone(),
            repo: self.repo.to_string_lossy().into_owned(),
            started_at: self.started_at,
            finished_at: self.finished_at,
            dir: self.dir.to_string_lossy().into_owned(),
            report: self.report_written().then(|| self.report().to_string_lossy().into_owned()),
            session: self.session.filter(|_| self.running()),
            state: self.state.clone(),
            host: host.map(str::to_string),
        }
    }
}

/// A scan's id: its start time in UTC, then four random hex digits.
pub fn new_id(now: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(now);
    let random = util::uuid_v4().map(|u| u[..4].to_string()).unwrap_or_else(|_| format!("{:04x}", now % 0xffff));
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{random}")
}

/// Unix seconds as a UTC date and time.
fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's days-from-civil, inverted.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

/// A scope as the scan may take it: a relative path inside the repository,
/// without `..`, of ordinary characters, which an existing path matches.
pub fn valid_scope(repo: &Path, scope: &str) -> Result<String> {
    let scope = scope.trim().trim_matches('/');
    if scope.is_empty() || scope.len() > 200 {
        bail!("the scope must be a path inside the repository");
    }
    let ordinary = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_');
    if !scope.chars().all(ordinary) || scope.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
        bail!("the scope {scope:?} is not a plain path inside the repository");
    }
    if !repo.join(scope).exists() {
        bail!("{scope} is not in {}", repo.display());
    }
    Ok(scope.to_string())
}

/// Check the repository at `start_dir` out in a new scan's scratch worktree
/// under `scans_dir`. Runs on a thread of its own: git may take a while.
pub fn prepare(start_dir: &Path, scans_dir: &Path, scope: Option<&str>, now: u64) -> Result<ScanJob> {
    let top = run::git(start_dir, &["rev-parse", "--show-toplevel"], GIT_LIMIT)
        .map(|t| PathBuf::from(t.trim()))
        .with_context(|| format!("{} is not in a git repository", start_dir.display()))?;
    let scope = scope.map(|s| valid_scope(&top, s)).transpose()?;
    let head = run::git(&top, &["rev-parse", "--short", "HEAD"], GIT_LIMIT).map(|h| h.trim().to_string());
    util::ensure_private_dir(scans_dir).with_context(|| format!("making {}", scans_dir.display()))?;
    let id = new_id(now);
    let dir = scans_dir.join(&id);
    fs::create_dir(&dir).with_context(|| format!("making {}", dir.display()))?;
    let mut job = ScanJob {
        id,
        repo: top.clone(),
        head,
        scope,
        started_at: now,
        finished_at: None,
        session: None,
        state: "preparing".to_string(),
        dir,
    };
    fs::create_dir(job.out())?;
    job.save()?;
    let worktree = job.worktree();
    let argv = [
        "git".to_string(),
        "-C".to_string(),
        top.to_string_lossy().into_owned(),
        "worktree".to_string(),
        "add".to_string(),
        "--detach".to_string(),
        "--".to_string(),
        worktree.to_string_lossy().into_owned(),
        "HEAD".to_string(),
    ];
    let ran = run::run(&argv, None, &[], GIT_LIMIT)?;
    if !ran.ok() {
        job.state = format!("failed: git worktree add: {}", ran.reason());
        job.finished_at = Some(util::now_secs());
        let _ = job.save();
        bail!("could not check the repository out: {}", ran.reason());
    }
    job.state = "running".to_string();
    job.save()?;
    Ok(job)
}

/// What Codex is asked to do.
pub fn prompt(job: &ScanJob) -> String {
    let scope = match &job.scope {
        Some(scope) => format!(" Limit the scan to `{scope}`."),
        None => String::new(),
    };
    format!(
        "Run a Codex Security scan of this repository with the codex-security plugin's security-scan skill.{scope} \
         This folder is a scratch checkout Cepheus removes afterwards: do not change its files. Write the \
         scan's output (report.md, findings.json and the rest) to {out} as its output directory, then stop.",
        out = job.out().display()
    )
}

/// The extra Codex arguments: the report folder is writable to it.
pub fn codex_args(job: &ScanJob) -> Vec<String> {
    vec!["--add-dir".to_string(), job.out().to_string_lossy().into_owned()]
}

/// Remove the scan's scratch worktree and record how it ended. Runs on a
/// thread of its own. Only the scan's own `src` folder is removed, through
/// git, which refuses a folder that is not one of the repository's
/// worktrees.
pub fn finish(mut job: ScanJob, stopped: bool) -> ScanJob {
    let worktree = job.worktree();
    if worktree.exists() {
        let argv = [
            "git".to_string(),
            "-C".to_string(),
            job.repo.to_string_lossy().into_owned(),
            "worktree".to_string(),
            "remove".to_string(),
            "--force".to_string(),
            "--".to_string(),
            worktree.to_string_lossy().into_owned(),
        ];
        match run::run(&argv, None, &[], GIT_LIMIT) {
            Ok(ran) if ran.ok() => {}
            Ok(ran) => job.state = format!("failed: removing its worktree: {}", ran.reason()),
            Err(err) => job.state = format!("failed: removing its worktree: {err}"),
        }
    }
    let _ = run::git(&job.repo, &["worktree", "prune"], GIT_LIMIT);
    if !job.state.starts_with("failed") {
        job.state = if job.report_written() {
            "saved".to_string()
        } else if stopped {
            "stopped".to_string()
        } else {
            "failed: no report was written".to_string()
        };
    }
    job.finished_at = Some(util::now_secs());
    job.session = None;
    let _ = job.save();
    job
}

/// One scan's description, when `dir` holds one.
pub fn read(dir: &Path) -> Option<ScanJob> {
    let path = dir.join(SCAN_FILE);
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > SCAN_FILE_LIMIT {
        return None;
    }
    let job: ScanJob = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
    // A description copied from elsewhere does not point the deck at
    // another folder.
    (job.dir == dir).then_some(job)
}

/// Every scan in `scans_dir`, newest first.
pub fn list(scans_dir: &Path) -> Vec<ScanJob> {
    let Ok(entries) = fs::read_dir(scans_dir) else { return Vec::new() };
    let mut jobs: Vec<ScanJob> =
        entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).filter_map(|e| read(&e.path())).collect();
    jobs.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
    jobs.truncate(MAX_LISTED);
    jobs
}

#[cfg(test)]
#[path = "../../src/tests/service_jobs.rs"]
mod tests;
