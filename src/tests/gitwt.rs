use super::*;

#[test]
fn branch_status_counts_commits_against_the_upstream() {
    assert_eq!(parse_counts("961\t0\n"), Some((961, 0)));
    assert_eq!(parse_counts("2 3"), Some((2, 3)));
    assert_eq!(parse_counts("x\t1"), None);
    let dir = crate::testsupport::TempDir::new("gitwt-branch");
    let git = |cwd: &Path, args: &[&str]| {
        let ok = Command::new("git")
            .current_dir(cwd)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=master"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    let remote = dir.path().join("remote");
    let local = dir.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q"]);
    git(&remote, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git(dir.path(), &["clone", "-q", remote.to_str().unwrap(), "local"]);
    let status =
        |branch: &str, ahead, behind, worktrees| BranchStatus { branch: branch.into(), ahead, behind, worktrees };
    assert_eq!(branch_status(&local), Some(status("master", 0, 0, 0)));
    git(&local, &["commit", "-q", "--allow-empty", "-m", "two"]);
    git(&local, &["commit", "-q", "--allow-empty", "-m", "three"]);
    git(&remote, &["commit", "-q", "--allow-empty", "-m", "elsewhere"]);
    git(&local, &["fetch", "-q"]);
    assert_eq!(branch_status(&local), Some(status("master", 2, 1, 0)));
    // Worktrees beside the checkout count; one inside it or whose folder is
    // gone does not, nor does a folder beside it that is no worktree.
    for path in ["../local-a", "../local-b", "../local-gone", "inner/c"] {
        git(&local, &["worktree", "add", "-q", path]);
    }
    std::fs::remove_dir_all(dir.path().join("local-gone")).unwrap();
    std::fs::create_dir_all(dir.path().join("unrelated")).unwrap();
    assert_eq!(branch_status(&local), Some(status("master", 2, 1, 2)));
    assert_eq!(branch_status(&dir.path().join("local-a")).map(|s| s.worktrees), Some(2), "read from a worktree too");
    // No upstream: the branch alone.
    git(&local, &["checkout", "-q", "-b", "topic"]);
    assert_eq!(branch_status(&local), Some(status("topic", 0, 0, 2)));
    assert_eq!(branch_status(&dir.path().join("missing")), None);
}

fn wt(path: &str) -> WorktreeEntry {
    WorktreeEntry { path: path.to_string(), branch: None }
}

fn mentioned(text: &[u8], wts: &[WorktreeEntry]) -> Option<usize> {
    last_mention_in_lines(text, wts, |line| Some(line.to_string()))
}

#[test]
fn porcelain_parsing() {
    let text = "worktree /r/project\nHEAD abc\nbranch refs/heads/master\n\nworktree /r/project-cepheus\nHEAD def\nbranch refs/heads/agent/20261025-deck\n\nworktree /r/detached\nHEAD 123\ndetached\n";
    let got = parse_porcelain(text);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], WorktreeEntry { path: "/r/project".into(), branch: Some("master".into()) });
    assert_eq!(got[1].branch.as_deref(), Some("agent/20261025-deck"));
    assert_eq!(got[2].branch, None);
}

#[test]
fn last_mention_prefers_latest_and_longest_path() {
    let wts = vec![wt("/r/project-agent-a"), wt("/r/project-agent-ab"), wt("/r/project-agent-c")];
    let text = br#"cd /r/project-agent-c && ls; then "/r/project-agent-ab/src/x.rs" and /r/project-agent-a."#;
    assert_eq!(mentioned(text, &wts), Some(0), "a sentence-ending period still ends the path");
    let text = br#"cd /r/project-agent-c && ls; then "/r/project-agent-ab/src/x.rs""#;
    assert_eq!(mentioned(text, &wts), Some(1));
    let text = b"edit /r/project-agent-a/file then /r/project-agent-ab";
    assert_eq!(mentioned(text, &wts), Some(1), "a name sharing a prefix is told apart");
    assert_eq!(mentioned(b"/r/project-agent-a.bak/x", &wts), None, "a dotted sibling name is not a match");
    let text = b"/r/project-agent-ab/x then /r/project-agent-a/y";
    assert_eq!(mentioned(text, &wts), Some(0));
    assert_eq!(mentioned(b"/r/project/src", &wts), None, "the primary checkout is not a worktree");
    assert_eq!(mentioned(b"/r/project-agent-abc/x", &wts), None, "a longer sibling name is not a match");
    assert_eq!(mentioned(b"anything", &[]), None);
}

#[test]
fn last_mention_with_single_worktree() {
    let wts = vec![wt("/r/project-cepheus")];
    assert_eq!(mentioned(b"\"cwd\":\"/r/project-cepheus\"", &wts), Some(0));
}

#[test]
fn only_counted_parts_are_mentions() {
    let wts = vec![wt("/r/project-agent-a"), wt("/r/project-agent-b")];
    let text = b"ran: cd /r/project-agent-a | printed: /r/project-agent-b\nhook: tasks /r/project-agent-a /r/project-agent-b\nran: echo done\n";
    let ran = |line: &str| line.strip_prefix("ran: ").and_then(|l| l.split(" | printed:").next()).map(str::to_string);
    assert_eq!(last_mention_in_lines(text, &wts, ran), Some(0), "the part of a line left out is not a mention");
    assert_eq!(last_mention_in_lines(text, &wts, |line| line.starts_with("hook").then(|| line.to_string())), Some(1));
    assert_eq!(last_mention_in_lines(text, &wts, |_| None), None);
}

#[test]
fn a_worktree_task_is_its_folder_name_or_the_part_after_the_configured_marker() {
    // The generic default: no marker, the whole folder name.
    let marker = crate::config::Conventions::default().worktree_task_marker;
    assert_eq!(marker, "");
    assert_eq!(task_of("/r/project-agent-deck", &marker).as_deref(), Some("project-agent-deck"));
    assert_eq!(task_of("/r/feature-x/", &marker).as_deref(), Some("feature-x"));
    assert_eq!(task_of("/", &marker), None);
    // A project's `<repo>-agent-<task>` scheme.
    assert_eq!(task_of("/r/project-agent-deck", "-agent-").as_deref(), Some("deck"));
    assert_eq!(task_of("/r/project-agent-a-agent-b", "-agent-").as_deref(), Some("a-agent-b"));
    assert_eq!(task_of("/r/project-agent-", "-agent-"), None, "an empty task");
    assert_eq!(task_of("/r/project-cepheus", "-agent-"), None, "a folder without the marker");
    assert_eq!(task_of("/r/agent-x/project", "-agent-"), None, "only the folder's own name counts");
    let entry = WorktreeEntry { path: "/r/project-agent-deck".into(), branch: Some("agent/deck".into()) };
    assert_eq!(describe(&entry, "-agent-").task.as_deref(), Some("deck"));
    assert_eq!(describe(&entry, "").task.as_deref(), Some("project-agent-deck"));
}
