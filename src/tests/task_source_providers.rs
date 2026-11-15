use super::*;
use crate::testsupport::TempDir;

const SOURCE: &str = "host:re\u{301}pository:markdown";

fn no_gh(_: &[String], _: &Path) -> Result<Vec<u8>, SourceError> {
    panic!("no command may run")
}

fn call(root: &Path, provider: &str, extra: &[&str], gh: Gh) -> (i32, String, String) {
    let mut args: Vec<String> =
        ["--provider", provider, "--source-id", SOURCE, "--repository-root"].iter().map(|s| s.to_string()).collect();
    args.push(root.to_string_lossy().into_owned());
    args.extend(extra.iter().map(|s| s.to_string()));
    invoke(&args, gh)
}

fn tasks(dir: &TempDir, text: &str) -> Vec<serde_json::Value> {
    dir.write("repository/TASKS.md", text);
    let (code, out, err) = call(&dir.path().join("repository"), "markdown", &[], &no_gh);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.ends_with('\n'));
    serde_json::from_str(&out).unwrap()
}

#[test]
fn markdown_identity_state_fences_and_metadata_projection() {
    let dir = TempDir::new("task-source-markdown");
    let first = tasks(
        &dir,
        "# Tasks\n- [ ] Étudier ♞\n  1. [x] Ship `$(untrusted)` [link](../../secret)\n\
         \x20 * [ ] Duplicate\n+ [X] Duplicate\n```md\n- [ ] hidden\n```\n\
         ~~~\n2) [ ] hidden too\n~~~~\n",
    );
    let states: Vec<_> = first.iter().map(|t| t["state"].as_str().unwrap()).collect();
    assert_eq!(states, ["ready", "stopped", "ready", "stopped"]);
    assert_eq!(first[0]["title"], "Étudier ♞");
    let ids: HashSet<_> = first.iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 4);
    for task in &first {
        let keys: Vec<_> = task.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys.len(), 7);
        assert_eq!(task["source_id"], SOURCE);
        assert!(task["next_step"].is_null() && task["worktree"].is_null() && task["branch"].is_null());
    }
    let second = tasks(
        &dir,
        "1) [ ] Ship `$(untrusted)` [link](../../secret)\n\
         - [X] E\u{301}tudier   ♞\n- [x] Duplicate\n- [ ] Duplicate\n",
    );
    assert_eq!(first[0]["id"], second[1]["id"], "checking and NFC keep an identity");
    assert_eq!(first[1]["id"], second[0]["id"], "reordering keeps an identity");
    assert_eq!(first[2]["id"], second[2]["id"]);
    assert_eq!(first[3]["id"], second[3]["id"]);
}

#[test]
fn identities_match_the_documented_hash() {
    let dir = TempDir::new("task-source-hash");
    let found = tasks(&dir, "- [ ] Write the plan\n- [ ] Write the plan\n");
    let digest = ring::digest::digest(&ring::digest::SHA256, b"TASKS.md\0Write the plan");
    let hex: String = digest.as_ref()[..12].iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(found[0]["id"], format!("md-{hex}-1"));
    assert_eq!(found[1]["id"], format!("md-{hex}-2"));
}

#[test]
fn markdown_lines_follow_the_checklist_grammar() {
    assert_eq!(checkbox("- [ ] a"), Some((false, "a")));
    assert_eq!(checkbox("\t123456789) [X]\tb  "), Some((true, "b  ")));
    assert_eq!(checkbox("1234567890. [ ] a"), None);
    assert_eq!(checkbox("-[ ] a"), None);
    assert_eq!(checkbox("- [ ]a"), None);
    assert_eq!(checkbox("- [y] a"), None);
    assert_eq!(checkbox("- [ ]"), Some((false, "")));
    assert_eq!(lines("a\r\nb\rc\u{2028}d\n"), ["a", "b", "c", "d"]);
    assert_eq!(fence_opening("  ```rust"), Some(('`', 3, "rust")));
    assert!(fence_closing(" ~~~~ ", '~', 3) && !fence_closing("~~", '~', 3));
    assert_eq!(normalized_title(" a\u{202e}b\u{7}c\u{200d}d \u{2003} e "), "ab cd e".replace("cd", "c\u{200d}d"));
}

#[test]
fn an_empty_checklist_entry_fails_the_snapshot() {
    let dir = TempDir::new("task-source-empty");
    dir.write("repository/TASKS.md", "- [ ] fine\n- [x]   \n");
    let (code, out, err) = call(&dir.path().join("repository"), "markdown", &[], &no_gh);
    assert_eq!((code, out.as_str(), err.as_str()), (1, "", "task-source: malformed-content\n"));
}

#[test]
fn markdown_rejects_unsafe_or_unreadable_files_without_echoing_payload() {
    let dir = TempDir::new("task-source-unsafe");
    let root = dir.path().join("repository");
    dir.write("outside.md", "- [ ] private content");
    dir.write("repository/keep", "");
    std::os::unix::fs::symlink(dir.path().join("outside.md"), root.join("linked.md")).unwrap();
    std::fs::write(root.join("bad.md"), b"- [ ] \xff").unwrap();
    std::fs::write(root.join("large.md"), vec![b'a'; MAX_BYTES + 1]).unwrap();
    std::fs::create_dir(root.join("folder")).unwrap();
    let fifo = CString::new(root.join("pipe").as_os_str().as_bytes()).unwrap();
    // SAFETY: a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let outside = dir.path().join("outside.md").to_string_lossy().into_owned();
    for name in ["../outside.md", outside.as_str(), "linked.md", "bad.md", "large.md", "folder", "pipe", "missing", ""]
    {
        let (code, out, err) = call(&root, "markdown", &["--file", name], &no_gh);
        assert_ne!(code, 0, "{name}");
        assert_eq!(out, "");
        let category = err.strip_prefix("task-source: ").and_then(|e| e.strip_suffix('\n')).unwrap();
        assert!(category.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'), "{err}");
        assert!(!err.contains("private"));
    }
}

#[test]
fn contained_links_are_read_through_their_canonical_path() {
    let dir = TempDir::new("task-source-link");
    let root = dir.path().join("repository");
    dir.write("repository/docs/list.md", "- [ ] linked");
    std::os::unix::fs::symlink(root.join("docs/list.md"), root.join("TASKS.md")).unwrap();
    let (code, out, _) = call(&root, "markdown", &[], &no_gh);
    assert_eq!(code, 0);
    let found: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    let digest = ring::digest::digest(&ring::digest::SHA256, b"docs/list.md\0linked");
    let hex: String = digest.as_ref()[..12].iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(found[0]["id"], format!("md-{hex}-1"));
}

#[test]
fn markdown_and_serialized_snapshot_limits_fail_atomically() {
    let dir = TempDir::new("task-source-limits");
    let many = "- [ ] short\n".repeat(MAX_TASKS + 1);
    let wide: String = (0..1000).map(|i| format!("- [ ] {i} {}\n", "界".repeat(80))).collect();
    for text in [many, wide] {
        dir.write("repository/TASKS.md", &text);
        let (code, out, err) = call(&dir.path().join("repository"), "markdown", &[], &no_gh);
        assert_eq!((code, out.as_str(), err.as_str()), (1, "", "task-source: snapshot-too-large\n"));
    }
}

#[test]
fn arguments_are_validated_before_anything_is_read() {
    let dir = TempDir::new("task-source-arguments");
    dir.write("repository/TASKS.md", "- [ ] a\n");
    let root = dir.path().join("repository").to_string_lossy().into_owned();
    let invalid = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            invoke(&args, &no_gh),
            (1, String::new(), "task-source: invalid-configuration\n".into()),
            "{args:?}"
        );
    };
    invalid(&["--provider", "markdown", "--source-id", "s"]);
    invalid(&["--provider", "jira", "--source-id", "s", "--repository-root", &root]);
    invalid(&["--provider", "markdown", "--source-id", "a\nb", "--repository-root", &root]);
    invalid(&["--provider", "markdown", "--source-id", &"s".repeat(257), "--repository-root", &root]);
    invalid(&["--provider", "markdown", "--source-id", "s", "--repository-root", &root, "--github-repo", "a/b"]);
    invalid(&["--provider", "github", "--source-id", "s", "--repository-root", &root, "--file", "TASKS.md"]);
    invalid(&["--provider", "github", "--source-id", "s", "--repository-root", &root]);
    invalid(&["--provider", "markdown", "--source-id", "s", "--repository-root", "/nonexistent-root"]);
    invalid(&["--provider", "markdown", "--source-id", "s", "--repository-root", &root, "extra"]);
    invalid(&["--provider", "markdown", "--source-id", "s", "--repository-root", &root, "--prov", "x"]);
    let args: Vec<String> =
        ["--provider=markdown", "--source-id", "s", "--repository-root", &root].iter().map(|s| s.to_string()).collect();
    assert_eq!(invoke(&args, &no_gh).0, 0);
}

#[test]
fn github_fixed_read_only_argv_and_allowlisted_result() {
    let dir = TempDir::new("task-source-github");
    dir.write("repository/keep", "");
    let root = dir.path().join("repository");
    let payload = serde_json::json!([
        {"number": 12, "title": "Fix ♞\n\u{1b} unsafe", "state": "OPEN", "body": "SECRET_BODY"},
        {"number": 2, "title": "Done", "state": "CLOSED", "comments": ["SECRET_BODY"]},
    ])
    .to_string();
    let calls = std::cell::RefCell::new(Vec::new());
    let gh = |argv: &[String], cwd: &Path| -> Result<Vec<u8>, SourceError> {
        calls.borrow_mut().push((argv.to_vec(), cwd.to_path_buf()));
        Ok(payload.clone().into_bytes())
    };
    let (code, out, err) = call(&root, "github", &["--github-repo", "octo/project"], &gh);
    assert_eq!((code, err.as_str()), (0, ""));
    let (argv, cwd) = calls.borrow()[0].clone();
    assert_eq!(
        argv,
        [
            "gh",
            "issue",
            "list",
            "--repo",
            "octo/project",
            "--state",
            "all",
            "--limit",
            "1001",
            "--json",
            "number,title,state"
        ]
    );
    assert_eq!(cwd, std::fs::canonicalize(&root).unwrap());
    let found: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(found[0]["state"], "ready");
    assert_eq!(found[1]["state"], "stopped");
    assert_eq!(found[0]["id"], "12");
    assert_eq!(found[1]["id"], "2");
    assert_eq!(found[0]["title"], "Fix ♞ unsafe");
    assert!(!out.contains("SECRET_BODY"));
}

#[test]
fn github_command_failures_are_categorical() {
    let dir = TempDir::new("task-source-gh-run");
    let sh = |script: &str| vec!["sh".to_string(), "-c".into(), script.into()];
    let run = |argv: Vec<String>, timeout: u64| run_gh(&argv, dir.path(), Duration::from_millis(timeout));
    assert_eq!(run(sh("printf PRIVATE_TOKEN; exit 4"), 10_000), Err(SourceError("authentication-required")));
    assert_eq!(run(sh("printf PRIVATE_TOKEN; exit 1"), 10_000), Err(SourceError("command-failed")));
    assert_eq!(run(vec!["/nonexistent/gh".into()], 10_000), Err(SourceError("command-unavailable")));
    assert_eq!(run(sh("exec sleep 5"), 0), Err(SourceError("timeout")));
    assert_eq!(run(sh("exec sleep 5"), 50), Err(SourceError("timeout")));
    assert_eq!(run(sh(&format!("head -c {} /dev/zero", MAX_BYTES + 1)), 10_000), Err(TOO_LARGE));
    assert_eq!(run(sh("printf '[]'"), 10_000), Ok(b"[]".to_vec()));
}

#[test]
fn github_malformed_or_incomplete_rows_and_configuration_reject() {
    let dir = TempDir::new("task-source-gh-rows");
    dir.write("repository/keep", "");
    let root = dir.path().join("repository");
    let valid = serde_json::json!({"number": 1, "title": "Task", "state": "OPEN"});
    let with = |key: &str, value: serde_json::Value| {
        let mut row = valid.clone();
        row[key] = value;
        serde_json::json!([row]).to_string().into_bytes()
    };
    let payloads: Vec<Vec<u8>> = vec![
        b"invalid PRIVATE_TOKEN".to_vec(),
        b"[".to_vec(),
        b"\xff".to_vec(),
        b"{}".to_vec(),
        b"[null]".to_vec(),
        with("number", true.into()),
        with("number", 1.5.into()),
        with("number", 0.into()),
        with("title", serde_json::Value::Null),
        with("title", "\u{7}".into()),
        with("state", "MERGED".into()),
        with("state", serde_json::json!([])),
        serde_json::json!([valid.clone(), valid.clone()]).to_string().into_bytes(),
        serde_json::Value::Array(
            (1..=MAX_TASKS + 1).map(|i| serde_json::json!({"number": i, "title": "T", "state": "OPEN"})).collect(),
        )
        .to_string()
        .into_bytes(),
        vec![b' '; MAX_BYTES + 1],
    ];
    for payload in payloads {
        let gh = |_: &[String], _: &Path| -> Result<Vec<u8>, SourceError> { Ok(payload.clone()) };
        let (code, out, err) = call(&root, "github", &["--github-repo", "octo/project"], &gh);
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(err.starts_with("task-source: ") && !err.contains("PRIVATE_TOKEN"), "{err}");
    }
    for repo in ["--web", "a/b/c", "https://github.com/a/b", "a/b;write", "a/b\n", "a/..", "-a/b"] {
        assert_eq!(
            call(&root, "github", &["--github-repo", repo], &no_gh),
            (1, String::new(), "task-source: invalid-configuration\n".into()),
            "{repo:?}"
        );
    }
}

#[test]
fn display_clipping_keeps_unicode_and_distinct_full_title_identities() {
    let dir = TempDir::new("task-source-clip");
    let prefix = "♞".repeat(TITLE_LIMIT + 1);
    let found = tasks(&dir, &format!("- [ ] {prefix} first\n- [ ] {prefix} second\n"));
    let title = found[0]["title"].as_str().unwrap();
    assert!(title.ends_with('…'));
    assert!(title.len() <= TITLE_LIMIT);
    assert!(title.contains('♞') && !title.contains('\u{fffd}'));
    assert_eq!(found[0]["title"], found[1]["title"]);
    assert_ne!(found[0]["id"], found[1]["id"]);
}
