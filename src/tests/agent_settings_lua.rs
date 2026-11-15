//! The `agent-limit` and `agent-plugins` Lua packages: Codex's config.toml
//! and Claude's settings.json changed in place, everything else kept.
use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const SOURCES: [&str; 5] = [
    "plugins/agent-limit.lua",
    "plugins/agent-plugins.lua",
    "lua/agent_settings/files.lua",
    "lua/agent_settings/json.lua",
    "lua/agent_settings/toml.lua",
];

/// A deck with both packages installed whose environment names only `env`,
/// so every settings file it can find is inside `dir`.
fn deck(dir: &TempDir, env: &[(&str, &Path)]) -> Engine {
    for source in SOURCES {
        let code = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(source)).unwrap_or_default();
        dir.write(&format!("config/{source}"), code);
    }
    let vars: String = env.iter().map(|(name, path)| format!("{name} = [[{}]], ", path.display())).collect();
    // The packages need no deck subcommand: one they ran would fail.
    dir.write(
        "config/init.lua",
        format!(
            "local env = {{ {vars} }}\nos.getenv = function(name) return env[name] end\nrequire('deck').exe = [[{}]]\n",
            dir.path().join("no-cepheus").display()
        ),
    );
    let (mut e, _rx) = engine(dir);
    e.load();
    assert_eq!(e.take_errors(), Vec::<String>::new());
    e
}

fn command(e: &mut Engine, line: &str) -> Vec<Request> {
    let mut words = line.split_whitespace().map(str::to_string);
    let name = words.next().unwrap();
    assert!(e.run_command(&name, &words.collect::<Vec<_>>()), ":{name} is registered");
    assert_eq!(e.take_errors(), Vec::<String>::new(), ":{line}");
    e.take_requests()
}

/// The one notice a command line gives, and whether it is an error.
fn said(e: &mut Engine, line: &str) -> (String, bool) {
    match command(e, line).as_slice() {
        [Request::Notify { text, error }] => (text.clone(), *error),
        other => panic!(":{line} gave {other:?}"),
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The names in a folder, so a staging file left behind shows.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

fn limit_message(n: u32) -> (String, bool) {
    (
        format!("Agent limit {n}: saved in Codex and applied to Claude subagents and new workflows. Reopen existing Codex sessions to apply it."),
        false,
    )
}

fn refused(text: &str) -> (String, bool) {
    (text.to_string(), true)
}

const CODEX: &str = "# Codex settings\nmodel = \"gpt-5\"\n\n[agents]\n# the older spelling stays when it is the only one\nmax_threads = 3 # per session\n\n[plugins.\"github@openai-curated\"]\nenabled = true\n";

#[test]
fn agent_limit_saves_codex_in_place_and_derives_both_claude_limits() {
    let dir = TempDir::new("lua-agent-limit");
    let home = dir.path().join("home");
    let codex = dir.write("home/.codex/config.toml", CODEX);
    let claude =
        dir.write("home/.claude/settings.json", r#"{"z": 1, "env": {"B": "é"}, "a": [1, 2.5, {}], "n": null}"#);
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o640)).unwrap();
    let mut e = deck(&dir, &[("HOME", &home)]);

    assert_eq!(said(&mut e, "agent-limit 6"), limit_message(6));
    assert_eq!(read(&codex), CODEX.replace("max_threads = 3", "max_threads = 6"), "one value changed");
    assert_eq!(
        read(&claude),
        "{\n  \"z\": 1,\n  \"env\": {\n    \"B\": \"é\",\n    \"CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS\": \"6\",\n    \"CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS\": \"6\"\n  },\n  \"a\": [\n    1,\n    2.5,\n    {}\n  ],\n  \"n\": null\n}\n"
    );
    assert_eq!(mode(&claude), 0o640);
    assert_eq!(names(&home.join(".claude")), ["settings.json"]);
    assert_eq!(names(&home.join(".codex")), ["config.toml"]);
    let source = fs::canonicalize(&codex).unwrap();
    assert_eq!(
        said(&mut e, "agent-limit"),
        (format!("Agent limit: 6; source: {}; Claude: synced", source.display()), false)
    );
}

#[test]
fn agent_limit_creates_missing_settings_and_leaves_invalid_ones_untouched() {
    let dir = TempDir::new("lua-agent-limit-new");
    let (codex_home, claude_home) = (dir.path().join("codex"), dir.path().join("claude"));
    let (codex, claude) = (codex_home.join("config.toml"), claude_home.join("settings.json"));
    let mut e = deck(&dir, &[("HOME", dir.path()), ("CODEX_HOME", &codex_home), ("CLAUDE_CONFIG_DIR", &claude_home)]);

    assert_eq!(said(&mut e, "agent-limit 4"), limit_message(4));
    assert_eq!(read(&codex), "[agents]\nmax_concurrent_threads_per_session = 4\n");
    assert_eq!(
        read(&claude),
        "{\n  \"env\": {\n    \"CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS\": \"4\",\n    \"CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS\": \"4\"\n  }\n}\n"
    );
    assert_eq!((mode(&codex), mode(&claude)), (0o600, 0o600), "new settings are private");

    // A key added to an inline table joins it.
    fs::write(&codex, "agents = { other = 1 } # inline\n").unwrap();
    assert_eq!(said(&mut e, "agent-limit 5"), limit_message(5));
    assert_eq!(read(&codex), "agents = { other = 1, max_concurrent_threads_per_session = 5 } # inline\n");

    let range = refused("Agent limit: Use a limit from 1 to 256 (Claude's native Workflow range)");
    let objects = refused("Agent limit: Claude settings and its env entry must be objects");
    let cases = [
        ("[agents]\nmax_threads = 2\n", "{}", "agent-limit 0", range.clone()),
        ("[agents]\nmax_threads = 2\n", "{}", "agent-limit 257", range),
        ("[agents]\nmax_threads = 2\n", "{}", "agent-limit two", refused("Use :agent-limit [1..256]")),
        // Claude is checked before Codex changes.
        ("[agents]\nmax_threads = 2\n", r#"{"env": 3}"#, "agent-limit 8", objects.clone()),
        ("[agents]\nmax_threads = 2\n", "[]", "agent-limit 8", objects),
        (
            "[agents]\nmax_threads = 2\n",
            "{",
            "agent-limit 8",
            refused("Agent limit: Claude settings must contain valid JSON"),
        ),
        ("agents = 5\n", "{}", "agent-limit 8", refused("Agent limit: Codex agents setting must be a table")),
        (
            "[agents]\nmax_threads = \"3\"\n",
            "{}",
            "agent-limit",
            refused("Agent limit: Codex agent concurrency must be a positive integer"),
        ),
        (
            "[agents\nmax_threads = 2\n",
            "{}",
            "agent-limit 8",
            refused("Agent limit: Cannot read agent concurrency from Codex config"),
        ),
    ];
    for (codex_text, claude_text, line, expected) in cases {
        fs::write(&codex, codex_text).unwrap();
        fs::write(&claude, claude_text).unwrap();
        assert_eq!(said(&mut e, line), expected, ":{line}");
        assert_eq!((read(&codex).as_str(), read(&claude).as_str()), (codex_text, claude_text), ":{line} wrote");
    }
}

const CLAUDE_SETTINGS: &str = "{\n  \"model\": \"opus\",\n  \"enabledPlugins\": {\n    \"adversarial-review@anthropic-plugin-directory\": true,\n    \"superpowers@claude-plugins-official\": true\n  },\n  \"env\": {\n    \"A\": \"1\"\n  }\n}\n";

const CODEX_PLUGINS: &str = "model = \"gpt-5\" # default\n\n[plugins.\"github@openai-curated\"]\nenabled = true # keep this comment\n\n[agents]\nmax_concurrent_threads_per_session = 3\n";

/// A home where Claude and Codex each have plugins installed and configured.
fn plugin_home(dir: &TempDir) -> (PathBuf, PathBuf) {
    let claude = dir.write("home/.claude/settings.json", CLAUDE_SETTINGS);
    let installed = [
        "adversarial-review@anthropic-plugin-directory",
        "superpowers@claude-plugins-official",
        "frontend-design@claude-plugins-official",
        "unset@market",
    ];
    let plugins: serde_json::Map<String, serde_json::Value> =
        installed.iter().map(|id| (id.to_string(), serde_json::json!([{"scope": "user"}]))).collect();
    dir.write(
        "home/.claude/plugins/installed_plugins.json",
        serde_json::json!({"version": 2, "plugins": plugins}).to_string(),
    );
    let codex = dir.write("home/.codex/config.toml", CODEX_PLUGINS);
    for plugin in ["openai-curated/github/1.0.0", "market/review/2.0", "market/docs/0.1"] {
        dir.write(&format!("home/.codex/plugins/cache/{plugin}/.codex-plugin/plugin.json"), "{}");
    }
    (claude, codex)
}

fn rows(e: &mut Engine) -> Vec<(ItemKind, String)> {
    match command(e, "agent-plugins").as_slice() {
        [Request::Pick { pick, .. }] => pick.items.iter().map(|item| (item.kind, item.plain())).collect(),
        other => panic!(":agent-plugins gave {other:?}"),
    }
}

fn changed(text: &str) -> (String, bool) {
    (format!("{text}. Reopen running sessions to apply it."), false)
}

#[test]
fn plugin_turns_one_agent_plugin_on_or_off_and_plugins_lists_every_state() {
    let dir = TempDir::new("lua-agent-plugins");
    let (claude, codex) = plugin_home(&dir);
    let mut e = deck(&dir, &[("HOME", &dir.path().join("home"))]);

    assert_eq!(
        said(&mut e, "agent-plugins off adversarial-review@anthropic-plugin-directory claude"),
        changed("Claude: adversarial-review@anthropic-plugin-directory is now off")
    );
    let claude_off = CLAUDE_SETTINGS.replace("directory\": true", "directory\": false");
    assert_eq!(read(&claude), claude_off);
    assert_eq!(read(&codex), CODEX_PLUGINS);

    // A short name finds the one plugin of that name, here only Codex's.
    assert_eq!(said(&mut e, "plugin off github"), changed("Codex: github@openai-curated is now off"));
    let codex_off = CODEX_PLUGINS.replace("enabled = true #", "enabled = false #");
    assert_eq!(read(&codex), codex_off);
    assert_eq!(read(&claude), claude_off);

    // Installed plugins without a setting gain one.
    said(&mut e, "plugin off review codex");
    assert_eq!(read(&codex), codex_off + "\n[plugins.\"review@market\"]\nenabled = false\n");
    said(&mut e, "plugin on frontend-design claude");
    assert_eq!(
        read(&claude),
        claude_off.replace(
            "official\": true\n",
            "official\": true,\n    \"frontend-design@claude-plugins-official\": true\n"
        )
    );
    assert_eq!(
        said(&mut e, "plugin on superpowers"),
        ("Claude: superpowers@claude-plugins-official is already on.".to_string(), false)
    );
    assert_eq!(names(&dir.path().join("home/.claude")), ["plugins", "settings.json"]);
    assert_eq!(names(&dir.path().join("home/.codex")), ["config.toml", "plugins"]);

    let row = |text: &str| (ItemKind::Text, text.to_string());
    assert_eq!(
        rows(&mut e),
        [
            (ItemKind::Heading, "Claude".to_string()),
            row("adversarial-review@anthropic-plugin-directory  off"),
            row("frontend-design@claude-plugins-official  on"),
            row("superpowers@claude-plugins-official  on"),
            row("unset@market  off (not set)"),
            (ItemKind::Heading, "Codex".to_string()),
            row("docs@market  on (default)"),
            row("github@openai-curated  off"),
            row("review@market  off"),
        ]
    );
}

#[test]
fn settings_symlinks_are_preserved_and_dangling_links_are_refused() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new("lua-agent-settings-links");
    let home = dir.path().join("home");
    let codex = dir.write("home/.codex/config.toml", CODEX);
    let target = dir.write("settings/claude.json", CLAUDE_SETTINGS);
    let claude = home.join(".claude/settings.json");
    fs::create_dir_all(claude.parent().unwrap()).unwrap();
    symlink(&target, &claude).unwrap();
    let mut e = deck(&dir, &[("HOME", &home)]);

    assert_eq!(said(&mut e, "agent-limit 6"), limit_message(6));
    assert_eq!(fs::read_link(&claude).unwrap(), target);
    assert!(read(&target).contains("CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS"));

    fs::remove_file(&target).unwrap();
    let before = read(&codex);
    let result = said(&mut e, "agent-limit 7");
    assert!(result.1, "a dangling settings link must be refused: {result:?}");
    assert_eq!(fs::read_link(&claude).unwrap(), target);
    assert!(!target.exists());
    assert_eq!(read(&codex), before, "Claude is checked before Codex changes");
}

#[test]
fn plugin_refuses_invalid_toml_scalars_without_changing_either_agent() {
    let dir = TempDir::new("lua-agent-plugins-scalars");
    let (claude, codex) = plugin_home(&dir);
    let mut e = deck(&dir, &[("HOME", &dir.path().join("home"))]);

    for value in [
        "2026-99-99",
        "2025-02-29",
        "25:00:00",
        "2026-01-01T12:00:00+24:00",
        "9223372036854775808",
        "-9223372036854775809",
        "0xffffffffffffffff",
    ] {
        let text = format!("{CODEX_PLUGINS}\nother = {value}\n");
        fs::write(&codex, &text).unwrap();
        let result = said(&mut e, "plugin off superpowers");
        assert!(result.1, "invalid TOML {value} must be refused: {result:?}");
        assert_eq!(read(&codex), text);
        assert_eq!(read(&claude), CLAUDE_SETTINGS);
    }

    for value in [
        "2024-02-29",
        "23:59:59.123",
        "2026-01-01T12:00:00+23:59",
        "9223372036854775807",
        "-9223372036854775808",
        "0x7fffffffffffffff",
    ] {
        let text = format!("{CODEX_PLUGINS}\nother = {value}\n");
        fs::write(&codex, &text).unwrap();
        let result = said(&mut e, "plugin off github codex");
        assert!(!result.1, "valid TOML {value} must remain editable: {result:?}");
        assert_eq!(read(&codex), text.replace("enabled = true #", "enabled = false #"));
    }
}

#[test]
fn plugin_refuses_unknown_ambiguous_or_unreadable_settings_without_writing() {
    let dir = TempDir::new("lua-agent-plugins-refuse");
    let (claude, codex) = plugin_home(&dir);
    let claude_text =
        CLAUDE_SETTINGS.replace("\"superpowers", "\"dup@a\": true,\n    \"dup@b\": false,\n    \"superpowers");
    let codex_text = format!("{CODEX_PLUGINS}\n[plugins.\"odd@market\"]\nenabled = \"yes\"\n");
    fs::write(&claude, &claude_text).unwrap();
    fs::write(&codex, &codex_text).unwrap();
    let mut e = deck(&dir, &[("HOME", &dir.path().join("home"))]);

    for (line, expected) in [
        ("plugin maybe github", "Use :plugin on|off NAME [claude|codex]"),
        ("plugin off github cursor", "Use :plugin on|off NAME [claude|codex]"),
        ("plugin off nope", "Plugin: No Claude or Codex plugin is named nope"),
        ("plugin off github claude", "Plugin: Claude has no plugin named github"),
        ("plugin off dup", "Plugin: Claude has several plugins named dup: dup@a, dup@b; use the full name"),
        ("plugin off odd codex", "Plugin: Codex's enabled setting for odd@market is not true or false"),
    ] {
        assert_eq!(said(&mut e, line), refused(expected), ":{line}");
    }
    assert_eq!((read(&claude), read(&codex)), (claude_text.clone(), codex_text));

    // Unreadable Codex settings stop a change that might be Codex's; naming
    // Claude still changes Claude's.
    fs::write(&codex, "[plugins\nenabled = true\n").unwrap();
    assert_eq!(said(&mut e, "plugin off superpowers"), refused("Plugin: Codex config is not valid TOML (line 1)"));
    assert_eq!(read(&claude), claude_text);
    said(&mut e, "plugin off superpowers claude");
    assert_eq!(read(&claude), claude_text.replace("official\": true", "official\": false"));
    let codex_rows: Vec<_> = rows(&mut e).into_iter().skip_while(|(_, text)| text != "Codex").collect();
    assert_eq!(
        codex_rows,
        [
            (ItemKind::Heading, "Codex".to_string()),
            (ItemKind::Text, "Codex config is not valid TOML (line 1)".to_string())
        ]
    );

    fs::write(&claude, "{\"enabledPlugins\": []}\n").unwrap();
    assert_eq!(
        said(&mut e, "plugin on superpowers claude"),
        refused("Plugin: Claude settings and its enabledPlugins entry must be objects")
    );
    assert_eq!(read(&claude), "{\"enabledPlugins\": []}\n");
}
