use super::*;
use crate::attention::policy::PathResolver;
use crate::testsupport::TempDir;
#[test]
fn production_resolver_rejects_dangling_links_and_proves_only_real_directory_parents() {
    let d = TempDir::new("attention-path-proof");
    let outside = TempDir::new("attention-outside");
    std::os::unix::fs::symlink(outside.path().join("new-output"), d.path().join("link")).unwrap();
    assert!(Resolver.resolve(&d.path().join("link")).is_none());
    std::fs::write(d.path().join("file"), "data").unwrap();
    assert!(Resolver.resolve(&d.path().join("file/child")).is_none());
    assert_eq!(Resolver.resolve(&d.path().join("new/path")), Some(d.path().canonicalize().unwrap().join("new/path")));
    let intent = policy::Intent {
        session: 1,
        prompt_identity: "request",
        tool: "Bash",
        command: "rustc source.rs -o link",
        cwd: d.path(),
        worktree: d.path(),
        complete: true,
        single_prompt: true,
        allow_once_selected: true,
        resolved_simple_shell: true,
    };
    let rules =
        [policy::Rule { id: "r".into(), tool: "Bash".into(), pattern: policy::Pattern::Exact(intent.command.into()) }];
    assert!(matches!(policy::evaluate(&intent, &rules, &Resolver), policy::Decision::Decline(_)));
}
