use super::*;
struct Proof;
impl PathResolver for Proof {
    fn resolve(&self, path: &Path) -> Option<PathBuf> {
        let text = path.to_str()?;
        if text.ends_with("/unresolved") {
            return None;
        }
        if text.ends_with("/link") || text.ends_with("/link/new.txt") {
            return Some("/outside/target".into());
        }
        let mut parts = Vec::new();
        for c in path.components() {
            match c {
                Component::RootDir => {}
                Component::CurDir => {}
                Component::ParentDir => {
                    parts.pop()?;
                }
                Component::Normal(s) => parts.push(s),
                _ => return None,
            }
        }
        Some(Path::new("/").join(parts.iter().collect::<PathBuf>()))
    }
}
fn intent(command: &str) -> Intent<'_> {
    Intent {
        session: 7,
        prompt_identity: "opaque-1",
        tool: "Bash",
        command,
        cwd: Path::new("/work/tree"),
        worktree: Path::new("/work/tree"),
        complete: true,
        single_prompt: true,
        allow_once_selected: true,
        resolved_simple_shell: true,
    }
}
fn exact(command: &str) -> Vec<Rule> {
    vec![Rule { id: "tests".into(), tool: "Bash".into(), pattern: Pattern::Exact(command.into()) }]
}
fn allowed(command: &str) -> bool {
    matches!(evaluate(&intent(command), &exact(command), &Proof), Decision::AllowOnce { .. })
}
#[test]
fn exact_rule_requires_whole_command_tool_and_once_intent() {
    assert!(allowed("cargo test --offline"));
    assert!(!allowed("cargo test --unknown"));
    assert_eq!(evaluate(&intent("cargo test"), &[], &Proof), Decision::Decline(Decline::NoRule));
    let mut request = intent("cargo test");
    let rules = exact("cargo test");
    request.tool = "Other";
    assert_eq!(evaluate(&request, &rules, &Proof), Decision::Decline(Decline::NoRule));
    request.tool = "Bash";
    request.allow_once_selected = false;
    assert_eq!(evaluate(&request, &rules, &Proof), Decision::Decline(Decline::NotAllowOnce));
}
#[test]
fn every_forbidden_class_declines_even_exact_owner_rules() {
    for tool in ["ReadCredentials", "DeleteFile", "Push"] {
        let mut request = intent("cat config.json");
        request.tool = tool;
        let rules =
            vec![Rule { id: "denied".into(), tool: tool.into(), pattern: Pattern::Exact("cat config.json".into()) }];
        assert_eq!(evaluate(&request, &rules, &Proof), Decision::Decline(Decline::Forbidden));
    }
    for command in [
        "git push",
        "tool deploy",
        "tool delete file",
        "rm file",
        "rmdir dir",
        "git reset HEAD",
        "git diff --force",
        "tool --force-with-lease",
        "cat .env",
        "cat .ssh/id_rsa",
        "cat credentials.json",
        "tool --password=value",
        "tool auth",
        "tool token",
        "tool merge",
        "tool publish",
        "tool kill 42",
    ] {
        assert!(!allowed(command), "{command}");
    }
}
#[test]
fn compound_clipped_free_text_unresolved_and_persistent_choices_decline() {
    for command in [
        "cargo test && cargo check",
        "cargo test; git status",
        "cargo test\n",
        "cargo test > log",
        "cargo test | cat",
        "echo $(cargo test)",
        "echo `id`",
        "env X=1 cargo test",
        "bash -c 'cargo test'",
        "python3 -c 'print(1)'",
        "cargo test *",
        "cargo test # comment",
        "cargo test \\",
        "cargo test '",
    ] {
        assert!(!allowed(command), "{command}");
    }
    for field in 0..4 {
        let mut request = intent("cargo test");
        match field {
            0 => request.complete = false,
            1 => request.single_prompt = false,
            2 => request.resolved_simple_shell = false,
            _ => request.prompt_identity = "",
        }
        assert!(matches!(evaluate(&request, &exact("cargo test"), &Proof), Decision::Decline(_)));
    }
}
#[test]
fn all_path_forms_require_canonical_worktree_proof_before_matching() {
    for command in [
        "cat /etc/passwd",
        "cat ../outside",
        "cat link",
        "cat link/new.txt",
        "cat unresolved",
        "tool --output=/outside/file",
        "tool --output ../outside",
        "tool --config=parent=/outside",
        "tool --unknown-path=/outside",
        "tool --mystery /outside",
    ] {
        assert!(!allowed(command), "{command}");
    }
    for command in [
        "cat relative.txt",
        "cat 'file with spaces.txt'",
        "rustc source.rs -o new.txt",
        "rustc source.rs --edition=2021",
        "python3 -m unittest tests.test_one",
        "cargo test --manifest-path=child/Cargo.toml",
    ] {
        assert!(allowed(command), "{command}");
    }
    let mut request = intent("cargo test");
    request.cwd = Path::new("/work/tree-other");
    assert_eq!(evaluate(&request, &exact("cargo test"), &Proof), Decision::Decline(Decline::OutsideWorktree));
}
#[test]
fn argv_globs_are_anchored_and_never_override_forbidden_or_path_checks() {
    assert!(glob("case_?", "case_é"));
    assert!(!glob("case_?", "case_éx"));
    let rules = vec![Rule {
        id: "one".into(),
        tool: "Bash".into(),
        pattern: Pattern::ArgvGlob(vec!["cargo".into(), "test".into(), "case_?*".into()]),
    }];
    assert_eq!(evaluate(&intent("cargo test case_one"), &rules, &Proof), Decision::AllowOnce { rule_id: "one".into() });
    for command in
        ["cargo test", "cargo test case_one extra", "cargo test case_foo/../../../outside", "cargo test case_delete"]
    {
        assert!(matches!(evaluate(&intent(command), &rules, &Proof), Decision::Decline(_)), "{command}");
    }
    let invalid = vec![Rule { id: "bad".into(), tool: "Bash".into(), pattern: Pattern::ArgvGlob(vec!["*".into()]) }];
    assert_eq!(evaluate(&intent("cargo test"), &invalid, &Proof), Decision::Decline(Decline::InvalidRules));
}
#[test]
fn bounded_inputs_and_rule_identity_fail_closed_without_echoing_commands() {
    assert!(!allowed(&"x".repeat(MAX_COMMAND + 1)));
    assert!(!allowed(&format!("cargo test {}", "x".repeat(MAX_TOKEN + 1))));
    let mut rules = exact("cargo test");
    rules.push(Rule { id: "tests".into(), tool: "Bash".into(), pattern: Pattern::Exact("cargo test".into()) });
    assert_eq!(evaluate(&intent("cargo test"), &rules, &Proof), Decision::Decline(Decline::InvalidRules));
    let rendered = format!(
        "{:?}",
        evaluate(&intent("cat credentials_SECRET_CANARY.json"), &exact("cat credentials_SECRET_CANARY.json"), &Proof)
    );
    assert!(!rendered.contains("CANARY"));
}

// These proofs use real temporary files and symlinks. A missing target is only
// resolved after proving its nearest existing parent; no lexical-only fallback.
struct ActualProof;
impl PathResolver for ActualProof {
    fn resolve(&self, path: &Path) -> Option<PathBuf> {
        let mut at = path.to_path_buf();
        let mut tail = Vec::new();
        loop {
            match std::fs::canonicalize(&at) {
                Ok(mut parent) => {
                    if !tail.is_empty() && !std::fs::metadata(&parent).ok()?.is_dir() {
                        return None;
                    }
                    for part in tail.iter().rev() {
                        parent.push(part);
                    }
                    return Some(parent);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::symlink_metadata(&at) {
                        Ok(_) => return None,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => return None,
                    }
                    tail.push(at.file_name()?.to_os_string());
                    at = at.parent()?.to_path_buf();
                }
                Err(_) => return None,
            }
        }
    }
}
struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("supervisor-policy-{}-{nonce}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        std::fs::create_dir(path.join("tree")).unwrap();
        std::fs::write(path.join("outside"), "fixture").unwrap();
        for name in ["link", "safe\u{a0}name", "safe\u{2003}name"] {
            std::os::unix::fs::symlink(path.join("outside"), path.join("tree").join(name)).unwrap();
        }
        std::fs::write(path.join("tree/--format"), "fixture").unwrap();
        Self(path)
    }
    fn root(&self) -> PathBuf {
        self.0.join("tree")
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn actual_allowed(command: &str, pattern: Pattern, root: &Path) -> bool {
    let mut request = intent(command);
    request.cwd = root;
    request.worktree = root;
    let rules = vec![Rule { id: "regression".into(), tool: "Bash".into(), pattern }];
    matches!(evaluate(&request, &rules, &ActualProof), Decision::AllowOnce { .. })
}
#[test]
fn unsupported_executable_and_subcommand_semantics_decline_both_rule_modes() {
    let sandbox = Sandbox::new();
    let cases = [
        ("cargo uninstall ripgrep", vec!["cargo", "uninstall", "*"]),
        ("cargo install --path .", vec!["cargo", "install", "--path", "*"]),
        ("pip uninstall sample", vec!["pip", "uninstall", "*"]),
        ("rustup toolchain uninstall stable", vec!["rustup", "toolchain", "uninstall", "*"]),
    ];
    let mut unsafe_decisions = Vec::new();
    for (command, glob) in cases {
        unsafe_decisions.push(actual_allowed(command, Pattern::Exact(command.into()), &sandbox.root()));
        unsafe_decisions.push(actual_allowed(
            command,
            Pattern::ArgvGlob(glob.into_iter().map(str::to_string).collect()),
            &sandbox.root(),
        ));
    }
    assert!(unsafe_decisions.iter().all(|v| !v), "unsafe decision matrix: {unsafe_decisions:?}");
}
#[test]
fn end_options_and_program_specific_flags_never_hide_an_outside_symlink() {
    let sandbox = Sandbox::new();
    let cases = [
        ("cat -- --format link", vec!["cat", "--", "--format", "*"]),
        ("cp -p link copied.txt", vec!["cp", "-p", "*", "copied.txt"]),
    ];
    let mut unsafe_decisions = Vec::new();
    for (command, glob) in cases {
        unsafe_decisions.push(actual_allowed(command, Pattern::Exact(command.into()), &sandbox.root()));
        unsafe_decisions.push(actual_allowed(
            command,
            Pattern::ArgvGlob(glob.into_iter().map(str::to_string).collect()),
            &sandbox.root(),
        ));
    }
    assert!(unsafe_decisions.iter().all(|v| !v), "unsafe decision matrix: {unsafe_decisions:?}");
}
#[test]
fn unicode_whitespace_never_changes_the_actual_filename_argv() {
    let sandbox = Sandbox::new();
    let mut unsafe_decisions = Vec::new();
    for space in ['\u{a0}', '\u{2003}'] {
        let command = format!("cat safe{space}name");
        unsafe_decisions.push(actual_allowed(&command, Pattern::Exact(command.clone()), &sandbox.root()));
        unsafe_decisions.push(actual_allowed(
            &command,
            Pattern::ArgvGlob(vec!["cat".into(), "safe*".into(), "name".into()]),
            &sandbox.root(),
        ));
    }
    assert!(unsafe_decisions.iter().all(|v| !v), "unsafe decision matrix: {unsafe_decisions:?}");
}

#[test]
fn finite_option_schemas_decline_unknown_semantics_and_preserve_end_options() {
    let sandbox = Sandbox::new();
    for (command, pattern) in [
        ("cargo test --config build.target-dir=link", vec!["cargo", "test", "--config", "*"]),
        ("cat --format link", vec!["cat", "--format", "*"]),
        ("rustc source.rs -L link", vec!["rustc", "source.rs", "-L", "*"]),
        ("pytest --basetemp scratch", vec!["pytest", "--basetemp", "*"]),
        ("cargo test -- --nocapture", vec!["cargo", "test", "--", "--nocapture"]),
    ] {
        assert!(!actual_allowed(command, Pattern::Exact(command.into()), &sandbox.root()));
        assert!(!actual_allowed(
            command,
            Pattern::ArgvGlob(pattern.into_iter().map(str::to_string).collect()),
            &sandbox.root()
        ));
    }
    // --format here is a proven local filename, never an option consuming link.
    let command = "cat -- --format";
    assert!(actual_allowed(command, Pattern::Exact(command.into()), &sandbox.root()));
    let command = "rustc new.rs -o new-output";
    assert!(actual_allowed(command, Pattern::Exact(command.into()), &sandbox.root()));
}

#[test]
fn rustc_response_files_decline_before_all_option_and_operand_roles() {
    let sandbox = Sandbox::new();
    let cases = [
        ("rustc @link", vec!["rustc", "@*"]),
        ("rustc '@link'", vec!["rustc", "@*"]),
        ("pytest @link", vec!["pytest", "@*"]),
        ("python3 -m pytest @link", vec!["python3", "-m", "pytest", "@*"]),
        ("cargo test --manifest-path @link", vec!["cargo", "test", "--manifest-path", "@*"]),
        ("cat @link", vec!["cat", "@*"]),
        ("rustc -- @link", vec!["rustc", "--", "@*"]),
        ("rustc source.rs -o @link", vec!["rustc", "source.rs", "-o", "@*"]),
        ("rustc --edition @link source.rs", vec!["rustc", "--edition", "@*", "source.rs"]),
    ];
    let mut unsafe_decisions = Vec::new();
    for (command, pattern) in cases {
        unsafe_decisions.push(actual_allowed(command, Pattern::Exact(command.into()), &sandbox.root()));
        unsafe_decisions.push(actual_allowed(
            command,
            Pattern::ArgvGlob(pattern.into_iter().map(str::to_string).collect()),
            &sandbox.root(),
        ));
    }
    // rustc expands @link before option parsing; the literal @link need not
    // exist. The existing link fixture resolves outside the worktree. Nothing
    // is compiled or executed by this test.
    assert!(unsafe_decisions.iter().all(|v| !v), "unsafe response matrix: {unsafe_decisions:?}");
}
