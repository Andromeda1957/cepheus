use std::os::unix::fs::symlink;

use super::*;
use crate::testsupport::TempDir;

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| w.to_string()).collect()
}

fn git(url: &str, name: &str) -> Source {
    Source::Git { url: url.into(), name: name.into() }
}

#[test]
fn sources_read_as_github_git_npm_mcp_or_a_folder() {
    let home = Path::new("/home/owner");
    let parse = |text: &str| parse_source(text, home).unwrap();
    assert_eq!(parse("anthropics/skills"), git("https://github.com/anthropics/skills.git", "skills"));
    assert_eq!(parse("gh:owner/repo.git"), git("https://github.com/owner/repo.git", "repo"));
    assert_eq!(parse("https://github.com/owner/repo/tree/main/x"), git("https://github.com/owner/repo.git", "repo"));
    assert_eq!(parse("https://gitlab.com/group/sub/proj.git/"), git("https://gitlab.com/group/sub/proj.git", "proj"));
    assert_eq!(parse("git@github.com:owner/tool.git"), git("git@github.com:owner/tool.git", "tool"));
    let npx = |pkg: &str| McpServer::Stdio { command: "npx".into(), args: words(&["-y", pkg]), env: Vec::new() };
    assert_eq!(
        parse("npm:@modelcontextprotocol/server-filesystem"),
        Source::Mcp { name: "filesystem".into(), server: npx("@modelcontextprotocol/server-filesystem") }
    );
    assert_eq!(
        parse("npm:mcp-server-fetch@1.2"),
        Source::Mcp { name: "fetch".into(), server: npx("mcp-server-fetch@1.2") }
    );
    assert_eq!(
        parse("mcp:files= uvx files-server --root /srv "),
        Source::Mcp {
            name: "files".into(),
            server: McpServer::Stdio {
                command: "uvx".into(),
                args: words(&["files-server", "--root", "/srv"]),
                env: Vec::new()
            },
        }
    );
    assert_eq!(
        parse("mcp:remote=https://mcp.example.com/sse"),
        Source::Mcp {
            name: "remote".into(),
            server: McpServer::Http { url: "https://mcp.example.com/sse".into(), headers: Vec::new() }
        }
    );
    let dir = TempDir::new("installer-folder");
    let real = dir.path().canonicalize().unwrap();
    let name = clean_name(&real.file_name().unwrap().to_string_lossy());
    assert_eq!(parse(&dir.path().to_string_lossy()), Source::Dir { path: real, name });
}

#[test]
fn sources_that_could_reach_a_flag_a_shell_or_somewhere_else_are_refused() {
    let home = Path::new("/home/owner");
    for text in [
        "",
        "   ",
        "a/b/c",
        "-x/repo",
        "owner/-repo",
        "owner/.hidden",
        "gh:owner",
        "mcp:-bad=x",
        "mcp:a b=x",
        "mcp:x=-rf /",
        "mcp:x=",
        "mcp:x=https://h/a b",
        "mcp:noequals",
        "npm:-g",
        "npm:.hidden",
        "npm:pkg;rm",
        "npm:",
        "https://-upload-pack=touch/x",
        "https://github.com/owner",
        "https:// spaced/url",
        "git@-oops:x/y.git",
        "git@host:-x",
        "git@host:a b",
        "ext::sh -c touch% /tmp/pwned",
        "file:///etc",
        "ssh://host/repo",
        "line\nbreak/repo",
        "/nonexistent/deck/folder",
    ] {
        assert!(parse_source(text, home).is_err(), "{text:?} was taken");
    }
    assert!(parse_source(&format!("owner/{}", "r".repeat(600)), home).is_err());
}

#[test]
fn names_from_a_source_are_ordinary_and_never_a_flag() {
    assert_eq!(clean_name("../../etc"), "etc");
    assert_eq!(clean_name("-rf"), "rf");
    assert_eq!(clean_name("my plugin!"), "my-plugin");
    assert_eq!(clean_name(".."), "source");
    assert_eq!(clean_name(""), "source");
    assert_eq!(clean_name(&"x".repeat(100)).len(), 64);
}

/// A fetched repository with a marketplace, MCP servers and skills, and
/// links and paths that try to lead outside it.
fn hostile_repo(dir: &TempDir) -> PathBuf {
    let outside = dir.path().join("outside");
    fs::create_dir_all(outside.join("stolen")).unwrap();
    fs::write(outside.join("stolen/SKILL.md"), "---\nname: stolen\n---\n").unwrap();
    fs::write(outside.join("plugin.json"), r#"{"name":"from-outside"}"#).unwrap();
    fs::write(outside.join(".mcp.json"), r#"{"mcpServers":{"outside":{"command":"x"}}}"#).unwrap();

    let root = dir.path().join("repo");
    dir.write(
        "repo/.claude-plugin/marketplace.json",
        r#"{"name":"tools market","plugins":[
            {"name":"good","source":"./plugins/good"},
            {"name":"up","source":"../outside"},
            {"name":"abs","source":"/etc"},
            {"name":"linked","source":{"source":"local","path":"./plugins/linked"}},
            {"source":"./plugins/nameless"}
        ]}"#,
    );
    fs::create_dir_all(root.join("plugins/good/skills/alpha")).unwrap();
    symlink(&outside, root.join("plugins/linked")).unwrap();
    dir.write(
        "repo/.mcp.json",
        r#"{"mcpServers":{
            "files":{"command":"uvx","args":["files-server","--root","/srv"],"env":{"TOKEN":"from-env"}},
            "flag":{"command":"--upload-pack=evil"},
            "web":{"url":"https://mcp.example.com/mcp","headers":{"X-Key":"k"}},
            "flagurl":{"url":"--header=evil"},
            "shellurl":{"url":"https://a b"}
        }}"#,
    );
    dir.write("repo/plugins/good/skills/alpha/SKILL.md", "---\nname: alpha-skill\ndescription: A\n---\n");
    // A link to a skill folder outside, and a link where the Codex
    // manifest folder would be.
    fs::create_dir_all(root.join("skills")).unwrap();
    symlink(outside.join("stolen"), root.join("skills/stolen")).unwrap();
    symlink(&outside, root.join(".codex-plugin")).unwrap();
    root
}

#[test]
fn detect_reads_only_what_stays_inside_the_source() {
    let dir = TempDir::new("installer-detect");
    let root = hostile_repo(&dir);
    let found = detect(&root);
    let market = found.claude_marketplace.clone().expect("the marketplace");
    assert_eq!(market.name, "tools-market");
    let good = root.join("plugins/good").canonicalize().unwrap();
    assert_eq!(
        market.plugins,
        vec![("good".to_string(), Some(good)), ("up".into(), None), ("abs".into(), None), ("linked".into(), None)]
    );
    assert_eq!(found.codex_plugin, None, "the Codex manifest behind a link out is not read");
    let servers: Vec<&str> = found.mcp.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(servers, ["files", "web"], "flag-like commands and URLs are dropped");
    assert_eq!(found.skills.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["alpha-skill"]);
}

#[test]
fn a_source_whose_folders_lead_out_is_not_read_through_them() {
    let dir = TempDir::new("installer-linked-dirs");
    let outside = dir.path().join("outside");
    fs::create_dir_all(outside.join("skills/stolen")).unwrap();
    fs::write(outside.join("skills/stolen/SKILL.md"), "---\nname: stolen\n---\n").unwrap();
    fs::write(outside.join("marketplace.json"), r#"{"name":"m","plugins":[]}"#).unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    symlink(&outside, root.join(".claude-plugin")).unwrap();
    symlink(outside.join("skills"), root.join("skills")).unwrap();
    fs::write(outside.join("SKILL.md"), "x").unwrap();
    symlink(outside.join("SKILL.md"), root.join("SKILL.md")).unwrap();
    assert!(detect(&root).is_empty(), "{:?}", detect(&root));
}

#[test]
fn the_plan_gives_each_agent_the_form_it_understands() {
    let dir = TempDir::new("installer-plan");
    let root = hostile_repo(&dir);
    let found = detect(&root);
    let source = git("https://github.com/owner/tools.git", "tools");
    let steps = plan(&source, &found, Some(&root), &["claude", "codex", "agy"]);
    let runs: Vec<(&str, Vec<String>)> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Run { agent, argv, .. } => Some((*agent, argv.clone())),
            _ => None,
        })
        .collect();
    let good = root.join("plugins/good").canonicalize().unwrap();
    assert_eq!(
        runs,
        vec![
            ("claude", words(&["claude", "plugin", "marketplace", "add", "owner/tools"])),
            ("claude", words(&["claude", "plugin", "install", "-s", "user", "good@tools-market"])),
            ("claude", words(&["claude", "plugin", "install", "-s", "user", "up@tools-market"])),
            ("claude", words(&["claude", "plugin", "install", "-s", "user", "abs@tools-market"])),
            ("claude", words(&["claude", "plugin", "install", "-s", "user", "linked@tools-market"])),
            // Codex has no marketplace here: the MCP servers, then skills.
            (
                "codex",
                words(&[
                    "codex",
                    "mcp",
                    "add",
                    "--env",
                    "TOKEN=from-env",
                    "files",
                    "--",
                    "uvx",
                    "files-server",
                    "--root",
                    "/srv"
                ])
            ),
            ("codex", words(&["codex", "mcp", "add", "web", "--url", "https://mcp.example.com/mcp"])),
            ("agy", vec!["agy".into(), "plugin".into(), "install".into(), good.to_string_lossy().into_owned()]),
        ]
    );
    let skills: Vec<(&str, Vec<String>)> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Skill { name, agents, .. } => Some((name.as_str(), agents.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(skills, vec![("alpha-skill", words(&["codex"]))]);
}

#[test]
fn an_mcp_server_given_whole_is_added_for_every_agent() {
    let source = parse_source("mcp:files=uvx files-server", Path::new("/h")).unwrap();
    let steps = plan(&source, &Found::default(), None, &["claude", "codex", "agy"]);
    let argvs: Vec<Vec<String>> = steps
        .iter()
        .map(|s| match s {
            Step::Run { argv, .. } => argv.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(argvs[0][..6], words(&["claude", "mcp", "add-json", "-s", "user", "files"]));
    let spec: Value = serde_json::from_str(&argvs[0][6]).unwrap();
    assert_eq!(spec, json!({"type": "stdio", "command": "uvx", "args": ["files-server"], "env": {}}));
    assert_eq!(argvs[1], words(&["codex", "mcp", "add", "files", "--", "uvx", "files-server"]));
    assert_eq!(argvs[2], words(&["agy", "mcp", "add", "files", "--", "uvx", "files-server"]));
    let http = parse_source("mcp:web=https://h.example/mcp", Path::new("/h")).unwrap();
    let Step::Run { argv, .. } = &plan(&http, &Found::default(), None, &["agy"])[0] else { panic!() };
    assert_eq!(*argv, words(&["agy", "mcp", "add", "web", "https://h.example/mcp"]));
}

#[test]
fn undoing_an_install_names_only_what_the_deck_recorded() {
    assert_eq!(
        undo_argv("claude-plugin", "good@m"),
        Some(words(&["claude", "plugin", "uninstall", "-s", "user", "good@m"]))
    );
    assert_eq!(undo_argv("codex-mcp", "files"), Some(words(&["codex", "mcp", "remove", "files"])));
    assert_eq!(undo_argv("agy-plugin", "p"), Some(words(&["agy", "plugin", "uninstall", "p"])));
    assert_eq!(undo_argv("claude-plugin", "--all"), None);
    assert_eq!(undo_argv("shell", "x"), None);
}

#[test]
fn a_dry_run_says_what_it_would_do_and_changes_nothing() {
    let dir = TempDir::new("installer-dry");
    let root = hostile_repo(&dir);
    let home = dir.path().join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let data = dir.path().join("data");
    let request = InstallRequest {
        source: root.to_string_lossy().into_owned(),
        agents: words(&["claude"]),
        dry_run: true,
        ..InstallRequest::default()
    };
    let report = install_in(&request, &home, &data);
    assert!(report.ok, "{:?}", report.lines);
    assert!(report.lines.iter().any(|l| l.ends_with("would install:")), "{:?}", report.lines);
    assert!(report.lines.iter().any(|l| l.contains("claude: install plugin good@tools-market")), "{:?}", report.lines);
    assert!(read_records(&data).is_empty());
    assert!(!home.join(".claude/skills").exists());
}

#[test]
fn a_skill_folder_installs_for_the_agents_asked_and_uninstalls() {
    let dir = TempDir::new("installer-skill");
    let home = dir.path().join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    let data = dir.path().join("data");
    dir.write("review/SKILL.md", "---\nname: review\ndescription: Review a diff\n---\n");
    let source = dir.path().join("review").to_string_lossy().into_owned();
    let request = InstallRequest { source: source.clone(), agents: words(&["claude"]), ..InstallRequest::default() };
    let report = install_in(&request, &home, &data);
    assert!(report.ok, "{:?}", report.lines);
    assert_eq!(report.lines, vec!["✓ skill review for claude".to_string()]);
    assert!(fs::symlink_metadata(home.join(".claude/skills/review")).is_ok_and(|m| m.file_type().is_symlink()));
    assert!(fs::symlink_metadata(home.join(".codex/skills/review")).is_err());
    let records = read_records(&data);
    assert_eq!(
        records["review"],
        Record { source: source.clone(), installed: vec![("skill".into(), "review".into())] }
    );

    // `:uninstall` without `--yes` previews first: closing that dialog
    // must leave the install as it was (audit ROOT-036).
    let preview = install_in(
        &InstallRequest { source: source.clone(), remove: true, dry_run: true, ..InstallRequest::default() },
        &home,
        &data,
    );
    assert!(preview.ok, "{:?}", preview.lines);
    assert!(preview.lines.iter().any(|l| l.contains("skill review")), "{:?}", preview.lines);
    assert!(fs::symlink_metadata(home.join(".claude/skills/review")).is_ok(), "the preview removed the skill");
    assert_eq!(read_records(&data), records);

    let removed = install_in(&InstallRequest { source, remove: true, ..InstallRequest::default() }, &home, &data);
    assert!(removed.ok, "{:?}", removed.lines);
    assert!(fs::symlink_metadata(home.join(".claude/skills/review")).is_err());
    assert!(read_records(&data).is_empty());
}

#[test]
fn an_uninstall_that_fails_keeps_the_receipt_of_what_is_still_installed() {
    let dir = TempDir::new("installer-retry");
    let home = dir.path().join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let data = dir.path().join("data");
    dir.write("review/SKILL.md", "---\nname: review\ndescription: Review a diff\n---\n");
    let source = dir.path().join("review").to_string_lossy().into_owned();
    let request = InstallRequest { source: source.clone(), agents: words(&["claude"]), ..InstallRequest::default() };
    assert!(install_in(&request, &home, &data).ok);
    // A second recorded item whose removal fails: the skills manifest has no such skill.
    let mut records = read_records(&data);
    records.get_mut("review").unwrap().installed.push(("skill".into(), "stuck".into()));
    write_records(&data, &records).unwrap();

    let removed = install_in(&InstallRequest { source, remove: true, ..InstallRequest::default() }, &home, &data);
    assert!(!removed.ok, "{:?}", removed.lines);
    assert!(fs::symlink_metadata(home.join(".claude/skills/review")).is_err(), "what could go went");
    // The failed item keeps its receipt so the next uninstall can retry it (audit ROOT-037).
    assert_eq!(read_records(&data)["review"].installed, vec![("skill".to_string(), "stuck".to_string())]);
}

#[test]
fn installs_running_at_once_keep_every_receipt() {
    // The service runs two installs at once and `cepheus add` can run beside
    // it; each must keep the others' receipts (audit OPS-S11601).
    let dir = TempDir::new("installer-concurrent");
    let home = dir.path().join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let data = dir.path().join("data");
    let names: Vec<String> = (0..8).map(|i| format!("skill{i}")).collect();
    for name in &names {
        dir.write(&format!("{name}/SKILL.md"), format!("---\nname: {name}\ndescription: d\n---\n"));
    }
    let start = std::sync::Barrier::new(names.len());
    std::thread::scope(|scope| {
        for name in &names {
            let source = dir.path().join(name).to_string_lossy().into_owned();
            let (home, data, start) = (&home, &data, &start);
            scope.spawn(move || {
                let request = InstallRequest { source, agents: words(&["claude"]), ..InstallRequest::default() };
                start.wait();
                let report = install_in(&request, home, data);
                assert!(report.ok, "{:?}", report.lines);
            });
        }
    });
    let records = read_records(&data);
    let manifest = skills::read_manifest(&skills::store(&data));
    for name in &names {
        assert!(records.contains_key(name), "{name} lost its receipt: {:?}", records.keys());
        assert!(manifest.installed.contains_key(name), "{name} left the skills manifest");
        assert!(fs::symlink_metadata(home.join(".claude/skills").join(name)).is_ok(), "{name} is not linked");
    }
}

#[test]
fn a_source_holding_nothing_installable_or_an_agent_not_here_is_reported() {
    let dir = TempDir::new("installer-empty");
    let home = dir.path().join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let data = dir.path().join("data");
    dir.write("plain/README.md", "nothing here");
    let source = dir.path().join("plain").to_string_lossy().into_owned();
    let report =
        install_in(&InstallRequest { source, agents: words(&["claude"]), ..InstallRequest::default() }, &home, &data);
    assert!(!report.ok && report.lines[0].contains("holds no plugin"), "{:?}", report.lines);

    let (chosen, missing) = agents_for(&words(&["antigravity"]), &dir.path().join("nobody"));
    if !on_path("agy") {
        assert!(chosen.is_empty());
        assert_eq!(missing, vec!["agy is not installed here".to_string()]);
    }
}

#[test]
fn fetching_refuses_local_and_command_transports() {
    let dir = TempDir::new("installer-fetch");
    let local = dir.path().join("local");
    run::run_args(&["git", "init", "-q", &local.to_string_lossy()], None, Duration::from_secs(20)).unwrap();
    let data = dir.path().join("data");
    assert!(fetch(&format!("file://{}", local.display()), "local", &data).is_err());
    let marker = dir.path().join("pwned");
    let ext = format!("ext::sh -c touch% {}", marker.display());
    assert!(fetch(&ext, "ext", &data).is_err());
    assert!(!marker.exists(), "the ext transport ran a command");
    assert!(!data.join("sources/local").exists() && !data.join("sources/.local.new").exists());
}
