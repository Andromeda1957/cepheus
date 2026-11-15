use super::*;
use crate::testsupport::TempDir;

/// A repository with one commit holding `src/main.rs` and `README.md`.
fn repo(dir: &TempDir) -> PathBuf {
    let root = dir.path().join("repo");
    dir.write("repo/src/main.rs", "fn main() {}\n");
    dir.write("repo/README.md", "readme\n");
    let env: Vec<(String, String)> = [
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@example.invalid"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@example.invalid"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let git = |args: &[&str]| {
        let mut argv = vec!["git".to_string(), "-C".into(), root.to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|a| a.to_string()));
        let ran = run::run(&argv, None, &env, Duration::from_secs(30)).unwrap();
        assert!(ran.ok(), "git {args:?}: {}", ran.reason());
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "first"]);
    root.canonicalize().unwrap()
}

fn worktrees(repo: &Path) -> String {
    run::git(repo, &["worktree", "list", "--porcelain"], Duration::from_secs(30)).unwrap()
}

#[test]
fn an_id_is_the_start_time_in_utc_and_a_random_suffix() {
    assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
    assert_eq!(civil(951_868_798), (2000, 2, 29, 23, 59, 58));
    assert_eq!(civil(1_793_505_906), (2026, 11, 1, 4, 5, 6));
    assert_eq!(civil(4_107_585_600), (2100, 3, 1, 12, 0, 0));
    let id = new_id(1_793_505_906);
    assert!(id.starts_with("20261101-040506-") && id.len() == 20, "{id}");
    assert!(id[16..].chars().all(|c| c.is_ascii_hexdigit()), "{id}");
}

#[test]
fn a_scope_is_a_plain_path_inside_the_repository() {
    let dir = TempDir::new("scan-scope");
    let root = repo(&dir);
    assert_eq!(valid_scope(&root, " src/ ").unwrap(), "src");
    assert_eq!(valid_scope(&root, "src/main.rs").unwrap(), "src/main.rs");
    for scope in [
        "",
        "../repo",
        "src/../..",
        "./src",
        "src//main.rs",
        "src main",
        "src;rm",
        "$(id)",
        "missing",
        &"a".repeat(201),
    ] {
        assert!(valid_scope(&root, scope).is_err(), "{scope:?} was taken");
    }
}

#[test]
fn a_scan_checks_the_commit_out_apart_and_finishing_removes_only_that_checkout() {
    let dir = TempDir::new("scan-run");
    let root = repo(&dir);
    // Git names worktrees by their real path.
    let scans = dir.path().canonicalize().unwrap().join("scans");
    let now = 1_793_505_906;
    // Started from inside the repository, the scan takes its top.
    let job = prepare(&root.join("src"), &scans, Some("src"), now).unwrap();
    assert_eq!(
        (job.repo.as_path(), job.state.as_str(), job.scope.as_deref()),
        (root.as_path(), "running", Some("src"))
    );
    assert!(job.head.as_deref().is_some_and(|h| h.len() >= 7));
    assert_eq!(fs::read_to_string(job.worktree().join("src/main.rs")).unwrap(), "fn main() {}\n");
    assert!(job.out().is_dir());
    assert!(worktrees(&root).contains(&job.worktree().to_string_lossy().into_owned()));
    assert_eq!(read(&job.dir).as_ref(), Some(&job));
    assert_eq!(job.tag(), format!("scan:{}", job.id));
    let info = job.info(Some("fedora"));
    assert_eq!((info.report, info.host.as_deref()), (None, Some("fedora")));

    fs::write(job.report(), "# Findings\n").unwrap();
    let done = finish(job.clone(), false);
    assert_eq!(done.state, "saved");
    assert!(done.finished_at.is_some() && done.session.is_none());
    assert!(!done.worktree().exists());
    assert!(!worktrees(&root).contains(&done.worktree().to_string_lossy().into_owned()));
    assert_eq!(fs::read_to_string(done.report()).unwrap(), "# Findings\n");
    assert_eq!(read(&done.dir).map(|j| j.state), Some("saved".into()));
    assert_eq!(done.info(None).report, Some(done.report().to_string_lossy().into_owned()));
    // The repository's own files are untouched.
    assert_eq!(fs::read_to_string(root.join("README.md")).unwrap(), "readme\n");
}

#[test]
fn a_scan_without_a_report_ends_stopped_or_failed() {
    let dir = TempDir::new("scan-noreport");
    let root = repo(&dir);
    let scans = dir.path().join("scans");
    let stopped = finish(prepare(&root, &scans, None, 1).unwrap(), true);
    assert_eq!(stopped.state, "stopped");
    let failed = finish(prepare(&root, &scans, None, 2).unwrap(), false);
    assert_eq!(failed.state, "failed: no report was written");
    // Newest first.
    let listed: Vec<u64> = list(&scans).iter().map(|j| j.started_at).collect();
    assert_eq!(listed, [2, 1]);
}

#[test]
fn nothing_is_made_for_a_folder_outside_git_or_a_bad_scope() {
    let dir = TempDir::new("scan-refuse");
    let scans = dir.path().join("scans");
    let plain = dir.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    assert!(prepare(&plain, &scans, None, 1).is_err());
    let root = repo(&dir);
    assert!(prepare(&root, &scans, Some("../.."), 1).is_err());
    assert!(list(&scans).is_empty());
    assert!(fs::read_dir(&scans).map_or(true, |mut d| d.next().is_none()));
}

#[test]
fn a_description_copied_from_another_folder_or_too_large_is_not_read() {
    let dir = TempDir::new("scan-read");
    let root = repo(&dir);
    let scans = dir.path().join("scans");
    let job = prepare(&root, &scans, None, 5).unwrap();
    let job = finish(job, true);
    let copy = scans.join("copy");
    fs::create_dir_all(&copy).unwrap();
    fs::copy(job.dir.join(SCAN_FILE), copy.join(SCAN_FILE)).unwrap();
    assert_eq!(read(&copy), None);
    let linked = scans.join("linked");
    fs::create_dir_all(&linked).unwrap();
    std::os::unix::fs::symlink(job.dir.join(SCAN_FILE), linked.join(SCAN_FILE)).unwrap();
    assert_eq!(read(&linked), None);
    let big = scans.join("big");
    fs::create_dir_all(&big).unwrap();
    fs::write(big.join(SCAN_FILE), " ".repeat(SCAN_FILE_LIMIT as usize + 1)).unwrap();
    assert_eq!(read(&big), None);
    assert_eq!(list(&scans).len(), 1);
}

#[test]
fn codex_is_asked_to_write_the_report_to_the_scans_folder() {
    let job = ScanJob {
        id: "x".into(),
        repo: "/r".into(),
        scope: Some("core/api".into()),
        dir: "/scans/x".into(),
        state: "running".into(),
        session: Some(4),
        ..ScanJob::default()
    };
    let asked = prompt(&job);
    assert!(asked.contains("security-scan") && asked.contains("Limit the scan to `core/api`."), "{asked}");
    assert!(asked.contains("/scans/x/report"), "{asked}");
    assert_eq!(codex_args(&job), vec!["--add-dir".to_string(), "/scans/x/report".into()]);
    assert_eq!(job.info(None).session, Some(4));
    let ended = ScanJob { state: "saved".into(), ..job };
    assert_eq!(ended.info(None).session, None);
}
