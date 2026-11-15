use super::*;
use crate::testsupport::TempDir;

fn put(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn skill(root: &Path, path: &str, name: &str, description: &str) {
    put(root, path, &format!("---\nname: {name}\ndescription: {description}\n---\nPRIVATE BODY NEVER SENT"));
}
fn entries(root: &Path, tool: Tool) -> Vec<Entry> {
    all(tool, &root.join("project/child"), &root.join("home"), &root.join("home/.claude"), &root.join("home/.codex"))
        .unwrap()
}
#[test]
fn discovers_project_user_synced_system_and_plugins_for_both_agents() {
    let tmp = TempDir::new("command-catalog");
    let root = tmp.path();
    for (path, name) in [
        ("project/.claude/skills/project/SKILL.md", "project"),
        ("home/.claude/skills/synced/account/chess/SKILL.md", "chess"),
        ("home/.claude/skills/global/SKILL.md", "global"),
        ("project/.agents/skills/project/SKILL.md", "project"),
        ("home/.agents/skills/shared/SKILL.md", "shared"),
        ("home/.codex/skills/.system/docs/SKILL.md", "docs"),
    ] {
        skill(root, path, name, "A useful skill");
    }
    put(root, "project/.claude/commands/nested/check.md", "---\ndescription: Check changes\n---\nPRIVATE COMMAND BODY");
    skill(root, "home/.codex/plugins/cache/market/dev/1/.codex-skills/fix/SKILL.md", "fix", "Repair code");
    put(root, "home/.codex/plugins/cache/market/dev/1/.codex-plugin/plugin.json", r#"{"skills":"./.codex-skills"}"#);
    skill(root, "claude-plugin/skills/fix/SKILL.md", "fix", "Repair code");
    put(root, "home/.claude/settings.json", r#"{"enabledPlugins":{"dev@market":true}}"#);
    put(
        root,
        "home/.claude/plugins/installed_plugins.json",
        &format!(
            r#"{{"plugins":{{"dev@market":[{{"installPath":{:?}}}]}}}}"#,
            root.join("claude-plugin").to_string_lossy()
        ),
    );
    let claude = entries(root, Tool::Claude);
    for name in ["/project", "/global", "/chess", "/dev:fix", "/nested:check", "/model", "/compact", "/skills"] {
        assert!(claude.iter().any(|e| e.name == name), "Claude missing {name}");
    }
    let codex = entries(root, Tool::Codex);
    for name in ["$project", "$shared", "$docs", "$dev:fix", "/model", "/goal", "/permissions", "/skills"] {
        assert!(codex.iter().any(|e| e.name == name), "Codex missing {name}");
    }
    assert!(!serde_json::to_string(&claude).unwrap().contains("PRIVATE"));
    assert!(!serde_json::to_string(&codex).unwrap().contains("PRIVATE"));
}
#[test]
fn nearest_definition_wins_and_no_list_is_cut_to_a_few_results() {
    let tmp = TempDir::new("command-precedence");
    let root = tmp.path();
    skill(root, "home/.claude/skills/review/SKILL.md", "review-work", "Global");
    skill(root, "project/.claude/skills/review/SKILL.md", "review-work", "Project");
    for i in 0..140 {
        skill(root, &format!("project/.claude/skills/skill-{i}/SKILL.md"), &format!("skill-{i}"), "A skill");
    }
    let out = entries(root, Tool::Claude);
    assert_eq!(out.iter().find(|e| e.name == "/review-work").unwrap().description, "Project");
    assert_eq!(out.iter().filter(|e| e.kind == "skill").count(), 141);
    assert_eq!(entries(root, Tool::Grok), Vec::new());
}
#[test]
fn disabled_plugins_hidden_skills_cycles_and_hostile_names_do_not_leak() {
    let tmp = TempDir::new("command-boundaries");
    let root = tmp.path();
    skill(root, "home/.codex/plugins/cache/market/off/1/skills/fix/SKILL.md", "fix", "Disabled");
    put(root, "home/.codex/plugins/cache/market/off/1/.codex-plugin/plugin.json", "{}");
    put(root, "home/.codex/config.toml", "[plugins.\"off@market\"]\nenabled = false\n");
    put(root, "home/.claude/skills/hidden/SKILL.md", "---\nname: hidden\nuser-invocable: false\n---\nPRIVATE");
    skill(root, "home/.claude/skills/bad/SKILL.md", "bad\\nname", "Bad");
    skill(root, "home/.claude/skills/good/SKILL.md", "good", ">-\n  A folded\n  description");
    std::os::unix::fs::symlink(root.join("home/.claude/skills"), root.join("home/.claude/skills/cycle")).unwrap();
    let out = entries(root, Tool::Claude);
    assert!(out.iter().all(|e| e.name != "/hidden" && !e.name.contains('\\')));
    assert_eq!(out.iter().find(|e| e.name == "/good").unwrap().description, "A folded description");
    assert!(!entries(root, Tool::Codex).iter().any(|e| e.name == "$off:fix"));
}
