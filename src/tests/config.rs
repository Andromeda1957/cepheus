use super::*;
use crate::testsupport::TempDir;

#[test]
fn refresh_requires_distinct_explicit_selected_and_excluded_hosts() {
    let selected: RefreshPolicy =
        serde_json::from_str(r#"{"build_host":"fedora","hosts":["fedora"],"excluded_hosts":["ubuntu"]}"#).unwrap();
    assert!(selected.allows("fedora"));
    assert!(!selected.allows("ubuntu"));
    assert!(!selected.allows("unselected"));
    assert!(Config::default().refresh.is_none());
    for body in [
        r#"{"build_host":"auto","hosts":["auto"],"excluded_hosts":[]}"#,
        r#"{"build_host":"fedora","hosts":["fedora"],"excluded_hosts":["fedora"]}"#,
        r#"{"build_host":"fedora","hosts":["other"],"excluded_hosts":[]}"#,
    ] {
        let policy: RefreshPolicy = serde_json::from_str(body).unwrap();
        assert!(!policy.valid(), "{body}");
        assert!(!policy.allows("fedora"));
    }
    let dir = TempDir::new("config-refresh-selection");
    dir.write("hosts.json", serde_json::json!({"hosts":[], "settings":{"refresh":selected}}).to_string());
    let (cfg, error) = Config::load_from(&dir.path().join("config.json"));
    assert!(error.is_none(), "{error:?}");
    assert!(cfg.refresh.unwrap().allows("fedora"));
}

#[test]
fn partial_file_overrides_and_clamps() {
    let dir = TempDir::new("config-partial");
    dir.write("profile.json", r#"{"codex_command":["codex","--yolo"],"scrollback":2000}"#);
    let path = dir.write(
        "config.json",
        r#"{"claude_command": [], "codex_command": ["codex"], "scrollback": 5, "sidebar_width": 500, "notifications": false}"#,
    );
    let (cfg, err) = Config::load_from(&path);
    assert!(err.is_none());
    assert_eq!(cfg.claude_command, vec!["claude"], "an empty command falls back to the default");
    assert_eq!(cfg.codex_command, vec!["codex"]);
    assert_eq!(cfg.scrollback, 100);
    assert_eq!(cfg.sidebar_width, 60);
    assert!(!cfg.notifications);
}

#[test]
fn invalid_file_reports_error_and_uses_defaults() {
    let dir = TempDir::new("config-invalid");
    let path = dir.write("config.json", "{not json");
    let (cfg, err) = Config::load_from(&path);
    assert!(err.unwrap().contains("config.json"));
    assert_eq!(cfg.sidebar_width, Config::default().sidebar_width);
}

#[test]
fn program_restart_timing_defaults_reads_and_clamps() {
    let dir = TempDir::new("config-program-restart");
    let timing = |body: &str| {
        let (cfg, err) = Config::load_from(&dir.write("config.json", body));
        assert!(err.is_none(), "{body}: {err:?}");
        (cfg.program_restart_check_secs, cfg.program_restart_grace_secs)
    };
    assert_eq!(timing("{}"), (2, 10), "the deck's own timing when unset");
    assert_eq!(timing(r#"{"program_restart_check_secs": 30, "program_restart_grace_secs": 0}"#), (30, 0));
    assert_eq!(
        timing(r#"{"program_restart_check_secs": 0, "program_restart_grace_secs": 100000}"#),
        (1, 3600),
        "a check of every upkeep tick and a grace past an hour are clamped"
    );
}

#[test]
fn sessions_start_in_the_profile_checkout_after_the_build_worktree_is_gone() {
    let dir = TempDir::new("config-start");
    let main = dir.path().join("main");
    let linked = dir.path().join("linked");
    fs::create_dir_all(&main).unwrap();
    git(&["init", "-q", "-b", "master"], &main);
    git(&["commit", "-q", "--allow-empty", "-m", "init"], &main);
    git(&["worktree", "add", "-q", "-b", "side", linked.to_str().unwrap()], &main);
    // The local profile records the primary checkout before a build tree is retired.
    let built_in = primary_checkout(&linked).unwrap();
    // Integration removes the worktree; the deck is then started from home.
    git(&["worktree", "remove", linked.to_str().unwrap()], &main);
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    dir.write("config/profile.json", serde_json::json!({"start_dir":built_in}).to_string());
    let (profile, error) = Config::load_from(&dir.path().join("config/config.json"));
    assert!(error.is_none(), "{error:?}");
    assert_eq!(profile.resolve_start_dir().unwrap(), built_in);
    let searched = [built_in.clone(), linked.clone(), home.clone()];
    assert_eq!(start_dir_from(None, &searched).unwrap(), built_in);
    // Without a repository anywhere it refuses instead of using the home directory.
    let err = start_dir_from(None, &[linked, home.clone()]).unwrap_err().to_string();
    assert!(err.contains("start_dir"), "{err}");
    assert_eq!(start_dir_from(Some(home.clone()), &[]).unwrap(), home);
}

fn git(args: &[&str], cwd: &Path) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn primary_checkout_finds_main_worktree_from_linked_worktree() {
    let dir = TempDir::new("config-git");
    let main = dir.path().join("main");
    let linked = dir.path().join("linked");
    fs::create_dir_all(&main).unwrap();
    git(&["init", "-q", "-b", "master"], &main);
    git(&["commit", "-q", "--allow-empty", "-m", "init"], &main);
    git(&["worktree", "add", "-q", "-b", "side", linked.to_str().unwrap()], &main);
    let want = fs::canonicalize(&main).unwrap();
    assert_eq!(primary_checkout(&linked).map(|p| fs::canonicalize(p).unwrap()), Some(want.clone()));
    assert_eq!(primary_checkout(&main).map(|p| fs::canonicalize(p).unwrap()), Some(want));
    assert_eq!(primary_checkout(&dir.path().join("absent")), None);
}

#[test]
fn parse_hex_accepts_short_and_long_forms() {
    assert_eq!(parse_hex("#1a2B3c"), Some((0x1a, 0x2b, 0x3c)));
    assert_eq!(parse_hex("#fff"), Some((255, 255, 255)));
    assert_eq!(parse_hex("1a2b3c"), None);
    assert_eq!(parse_hex("#12345"), None);
    assert_eq!(parse_hex("#gggggg"), None);
}

#[test]
fn palette_follows_kitty_includes_and_fills_cube() {
    let dir = TempDir::new("config-palette");
    dir.write("theme.conf", "foreground #c0caf5\ncolor1 #f7768e\n# comment\ncursor #ff9e64\n");
    let main = dir.write("kitty.conf", "font_size 11\ninclude theme.conf\nbackground #1a1b26\n");
    let cfg = Config { palette_file: Some(main), ..Config::default() };
    let p = Palette::load(&cfg);
    assert_eq!(p.colors.len(), PALETTE_LEN);
    assert_eq!(p.get(1), (0xf7, 0x76, 0x8e));
    assert_eq!(p.get(2), (0x00, 0xcd, 0x00), "unset colours keep xterm defaults");
    assert_eq!(p.get(256), (0xc0, 0xca, 0xf5));
    assert_eq!(p.get(257), (0x1a, 0x1b, 0x26));
    assert_eq!(p.get(258), (0xff, 0x9e, 0x64));
    assert_eq!(p.get(16), (0, 0, 0));
    assert_eq!(p.get(231), (255, 255, 255));
    assert_eq!(p.get(232), (8, 8, 8));
    assert_eq!(p.get(10_000), (0, 0, 0));
}

#[test]
fn a_host_runs_agents_unless_its_role_says_build() {
    let json = r#"{"name":"controller","tag":2,"addresses":[],"fingerprint":""}"#;
    let host: HostConfig = serde_json::from_str(json).unwrap();
    assert!(host.runs_agents(), "hosts recorded before roles keep running agents");
    assert!(serde_json::to_value(&host).unwrap().get("role").is_none(), "the default is not written");
    let build: HostConfig = serde_json::from_str(&json.replace(r#""tag""#, r#""role":"build","tag""#)).unwrap();
    assert!(!build.runs_agents());
    assert_eq!(serde_json::to_value(&build).unwrap()["role"], "build");
    assert!(serde_json::from_str::<HostConfig>(&json.replace(r#""tag""#, r#""role":"auto","tag""#)).is_err());
}
