//! Git worktrees of the start repository and detection of which one a
//! session is working in, from the paths its transcript mentions.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::protocol::{BranchInfo, Worktree};
use crate::run;

pub mod watch;

#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeEntry {
    pub path: String,
    pub branch: Option<String>,
}

pub fn parse_porcelain(text: &str) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                out.push(entry);
            }
            current = Some(WorktreeEntry { path: path.to_string(), branch: None });
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(entry) = current.as_mut() {
                entry.branch = Some(branch.strip_prefix("refs/heads/").unwrap_or(branch).to_string());
            }
        }
    }
    if let Some(entry) = current {
        out.push(entry);
    }
    out
}

/// Linked worktrees of the repository at `start_dir` (the primary is excluded).
pub fn list(start_dir: &Path) -> Vec<WorktreeEntry> {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(start_dir)
        .args(["worktree", "list", "--porcelain"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let primary = start_dir.to_string_lossy();
    parse_porcelain(&String::from_utf8_lossy(&out.stdout)).into_iter().filter(|e| e.path != primary).collect()
}

/// Where a checkout's branch stands against its upstream, as the owner's
/// shell prompt shows it ("project main ⇡3").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BranchStatus {
    /// The branch, or the short commit when none is checked out.
    pub branch: String,
    /// Commits it has that its upstream lacks, and the reverse; both 0
    /// without an upstream.
    pub ahead: u32,
    pub behind: u32,
    /// The repository's linked worktrees whose folders exist beside its main checkout.
    pub worktrees: usize,
}

/// How many of `entries`, in `git worktree list` order (the main checkout
/// first), are linked worktrees whose folder exists beside the main checkout.
pub fn sibling_worktrees(entries: &[WorktreeEntry]) -> usize {
    let Some((main, linked)) = entries.split_first() else { return 0 };
    let parent = Path::new(&main.path).parent();
    linked
        .iter()
        .filter(|e| {
            let path = Path::new(&e.path);
            path.parent() == parent && path.is_dir()
        })
        .count()
}

/// `git rev-list --left-right --count` output: "<ahead>\t<behind>".
pub fn parse_counts(text: &str) -> Option<(u32, u32)> {
    let mut counts = text.split_whitespace().map(|n| n.parse().ok());
    Some((counts.next()??, counts.next()??))
}

/// The branch checked out in `dir` and where it stands against its upstream.
pub fn branch_status(dir: &Path) -> Option<BranchStatus> {
    let git = |args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output();
        out.ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let mut branch = git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        branch = git(&["rev-parse", "--short", "HEAD"])?;
    }
    let (ahead, behind) = git(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        .and_then(|c| parse_counts(&c))
        .unwrap_or((0, 0));
    let worktrees =
        git(&["worktree", "list", "--porcelain"]).map_or(0, |list| sibling_worktrees(&parse_porcelain(&list)));
    Some(BranchStatus { branch, ahead, behind, worktrees })
}

fn is_path_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
}

/// Whether a path mention ends at `rest[0]`: the next byte cannot continue a
/// file name, or it is a sentence-ending period.
fn ends_mention(rest: &[u8]) -> bool {
    match rest.first() {
        None => true,
        Some(b'.') => rest.get(1).is_none_or(|&b| !is_path_char(b)),
        Some(&b) => !is_path_char(b),
    }
}

/// The directory prefix (ending in `/`) shared by every worktree path.
fn common_dir_prefix(worktrees: &[WorktreeEntry]) -> Option<String> {
    let (first, rest) = worktrees.split_first()?;
    let bytes = first.path.as_bytes();
    let mut len = bytes.len();
    for wt in rest {
        let common = bytes.iter().zip(wt.path.as_bytes()).take_while(|(a, b)| a == b).count();
        len = len.min(common);
    }
    Some(match bytes[..len].iter().rposition(|&b| b == b'/') {
        Some(slash) => first.path[..=slash].to_string(),
        None => "/".to_string(),
    })
}

/// Index of the worktree mentioned last in the parts of the whole lines of
/// `text` that `counted` returns. Transcripts are JSON lines, so the caller
/// can leave out injected context and tool output, such as a hook's task
/// summary or a `git worktree list` naming every worktree.
pub fn last_mention_in_lines(
    text: &[u8],
    worktrees: &[WorktreeEntry],
    counted: impl Fn(&str) -> Option<String>,
) -> Option<usize> {
    let prefix = common_dir_prefix(worktrees)?;
    // From the end: only the lines after the last mention are parsed.
    String::from_utf8_lossy(text)
        .lines()
        .filter(|line| line.contains(prefix.as_str()))
        .filter_map(counted)
        .filter_map(|part| last_mention(&part, worktrees, &prefix))
        .next_back()
}

/// Index of the worktree whose path appears last in `text`.
fn last_mention(text: &str, worktrees: &[WorktreeEntry], prefix: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut best = None;
    for (pos, _) in text.match_indices(prefix) {
        let rest = &bytes[pos..];
        let mut hit: Option<(usize, usize)> = None;
        for (i, wt) in worktrees.iter().enumerate() {
            let path = wt.path.as_bytes();
            if !path.is_empty()
                && rest.starts_with(path)
                && ends_mention(&rest[path.len()..])
                && hit.is_none_or(|(len, _)| path.len() > len)
            {
                hit = Some((path.len(), i));
            }
        }
        if let Some((_, i)) = hit {
            best = Some(i);
        }
    }
    best
}

/// A linked worktree's task: its folder name after `marker`
/// (`Conventions::worktree_task_marker`), or the whole folder name when
/// `marker` is empty.
pub fn task_of(path: &str, marker: &str) -> Option<String> {
    crate::extensions::conventions::task_of(path, marker)
}

pub fn describe(entry: &WorktreeEntry, marker: &str) -> Worktree {
    Worktree {
        path: entry.path.clone(),
        branch: entry.branch.clone(),
        task: task_of(&entry.path, marker),
        exists: Path::new(&entry.path).is_dir(),
    }
}

/// At most this many branches no worktree has checked out are listed, the
/// most recently committed first; checked-out ones are always listed.
pub const MAX_OTHER_BRANCHES: usize = 120;

/// The `for-each-ref` format `parse_branches` reads; `AB` is replaced by
/// the ahead-behind field, or nothing where git is too old for it.
const BRANCH_FORMAT: &str = "%(refname:short)%00%(objectname:short)%00%(committerdate:unix)%00AB%00%(subject)";

/// The repository's branches from `git for-each-ref` output in
/// `BRANCH_FORMAT`, with the worktree that has each checked out (`worktrees`
/// in `git worktree list` order, the main checkout first): the main
/// checkout's branch first, then the checked-out ones, then the rest, each
/// group newest first.
pub fn parse_branches(text: &str, worktrees: &[WorktreeEntry]) -> Vec<BranchInfo> {
    let main = worktrees.first().and_then(|w| w.branch.clone());
    let mut checked_out = Vec::new();
    let mut others = Vec::new();
    for line in text.lines() {
        let mut fields = line.split('\0');
        let Some(branch) = fields.next().filter(|b| !b.is_empty()) else { continue };
        let head = fields.next().filter(|h| !h.is_empty()).map(str::to_string);
        let committed_at = fields.next().and_then(|t| t.trim().parse().ok());
        let counts = fields.next().and_then(parse_counts);
        let subject = fields.next().map(crate::util::one_line).filter(|s| !s.is_empty());
        let path = worktrees.iter().find(|w| w.branch.as_deref() == Some(branch)).map(|w| w.path.clone());
        let is_main = main.as_deref() == Some(branch);
        let info = BranchInfo {
            branch: branch.to_string(),
            path: path.clone().unwrap_or_default(),
            head,
            subject,
            committed_at,
            ahead: counts.map(|c| c.0).filter(|_| !is_main),
            behind: counts.map(|c| c.1).filter(|_| !is_main),
            main: is_main,
        };
        if path.is_some() {
            checked_out.push(info);
        } else {
            others.push(info);
        }
    }
    let newest = |a: &BranchInfo, b: &BranchInfo| b.committed_at.cmp(&a.committed_at).then(a.branch.cmp(&b.branch));
    checked_out.sort_by(|a, b| b.main.cmp(&a.main).then_with(|| newest(a, b)));
    others.sort_by(newest);
    others.truncate(MAX_OTHER_BRANCHES);
    checked_out.extend(others);
    checked_out
}

/// Every branch of the repository at `dir` (`parse_branches`), for the branches list.
///
/// A repository holds well over a thousand branches, so the work is kept to what the list shows: the list is
/// read again only when a ref, a HEAD or a worktree changed (`refs_stamp`), one caller at a time per repository
/// (a scan in progress is waited for, never started twice), and ahead and behind counts are computed only for
/// the branches listed.
pub fn branches(dir: &Path) -> Vec<BranchInfo> {
    let slot = {
        let mut all = SCANS.lock().unwrap_or_else(|e| e.into_inner());
        all.entry(dir.to_path_buf()).or_default().clone()
    };
    let mut scan = slot.lock().unwrap_or_else(|e| e.into_inner());
    let limit = Duration::from_secs(15);
    if scan.common.is_none() {
        scan.common = run::git(dir, &["rev-parse", "--git-common-dir"], limit)
            .map(|text| dir.join(text.trim()))
            .filter(|path| path.is_dir());
    }
    let stamp = scan.common.as_deref().map(refs_stamp);
    if let (Some(stamp), Some((seen, branches))) = (&stamp, &scan.last) {
        if stamp == seen {
            return branches.clone();
        }
    }
    let branches = read_branches(dir, limit);
    scan.last = stamp.map(|stamp| (stamp, branches.clone()));
    branches
}

#[derive(Default)]
struct Scan {
    common: Option<std::path::PathBuf>,
    last: Option<(Vec<(std::path::PathBuf, Option<std::time::SystemTime>)>, Vec<BranchInfo>)>,
}

type ScanSlot = std::sync::Arc<std::sync::Mutex<Scan>>;
static SCANS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, ScanSlot>>> =
    std::sync::LazyLock::new(Default::default);

/// What the branch list depends on, from metadata alone: the packed refs
/// (or a reftable's table list), every directory of loose branch refs (a
/// commit or a new or deleted branch renames a file in one), the main HEAD,
/// and each linked worktree's HEAD.
fn refs_stamp(common: &Path) -> Vec<(std::path::PathBuf, Option<std::time::SystemTime>)> {
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut paths = vec![
        common.join("packed-refs"),
        common.join("reftable/tables.list"),
        common.join("HEAD"),
        common.join("worktrees"),
    ];
    let mut dirs = vec![common.join("refs/heads")];
    while let Some(dir) = dirs.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            dirs.extend(entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()));
        }
        paths.push(dir);
    }
    if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
        paths.extend(entries.flatten().map(|e| e.path().join("HEAD")));
    }
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let at = modified(&path);
            (path, at)
        })
        .collect()
}

fn read_branches(dir: &Path, limit: Duration) -> Vec<BranchInfo> {
    let Some(list) = run::git(dir, &["worktree", "list", "--porcelain"], limit) else { return Vec::new() };
    let worktrees = parse_porcelain(&list);
    let format = format!("--format={}", BRANCH_FORMAT.replace("AB", ""));
    let Some(text) = run::git(dir, &["for-each-ref", "--sort=-committerdate", &format, "refs/heads"], limit) else {
        return Vec::new();
    };
    let mut branches = parse_branches(&text, &worktrees);
    // Ahead and behind the main branch, for the listed branches only.
    let main = worktrees.first().and_then(|w| w.branch.clone()).filter(|m| !m.starts_with('-'));
    let refs: Vec<String> = branches.iter().filter(|b| !b.main).map(|b| format!("refs/heads/{}", b.branch)).collect();
    if let Some(main) = main.filter(|_| !refs.is_empty()) {
        let format = format!("--format=%(refname:short)%00%(ahead-behind:{main})");
        let mut args = vec!["for-each-ref", format.as_str()];
        args.extend(refs.iter().map(String::as_str));
        // `ahead-behind` is new in git 2.41; an older git lists no counts.
        if let Some(text) = run::git(dir, &args, limit) {
            let counts: std::collections::HashMap<&str, (u32, u32)> = text
                .lines()
                .filter_map(|line| line.split_once('\0'))
                .filter_map(|(name, counts)| Some((name, parse_counts(counts)?)))
                .collect();
            for branch in branches.iter_mut().filter(|b| !b.main) {
                if let Some((ahead, behind)) = counts.get(branch.branch.as_str()) {
                    branch.ahead = Some(*ahead);
                    branch.behind = Some(*behind);
                }
            }
        }
    }
    branches
}

#[cfg(test)]
#[path = "tests/gitwt.rs"]
mod tests;
