use super::*;
use crate::testsupport::TempDir;
use serde_json::json;

#[test]
fn profile_configuration_rejects_overlapping_or_ambiguous_credential_roots() {
    let dir = TempDir::new("accounts-config");
    let mut config = Config::default();
    let root = dir.path().join("codex");
    config.plugin_settings.insert(
        "provider-accounts".into(),
        json!({"rotate":true,"profiles":{
            "work":{"provider":"codex","directory":root},
            "second":{"provider":"claude","directory":dir.path().join("claude")}
        }}),
    );
    let settings = Settings::from_config(&config).unwrap();
    assert_eq!(
        settings.profile("work", Tool::Codex).unwrap().launch_env(),
        vec![
            ("CODEX_HOME".into(), root.to_str().unwrap().into()),
            ("CEPHEUS_ACCOUNT_DIRECTORY".into(), root.to_str().unwrap().into()),
            ("AGENT_DECK_ACCOUNT_DIRECTORY".into(), root.to_str().unwrap().into()), // legacy-name
            ("CEPHEUS_ACCOUNT_ID".into(), String::new()),
            ("AGENT_DECK_ACCOUNT_ID".into(), String::new()) // legacy-name
        ]
    );
    assert!(settings.profile("work", Tool::Claude).is_err());
    assert!(settings.profile("../work", Tool::Codex).is_err());
    assert_eq!(settings.rotation_profiles(Tool::Codex), vec!["work"]);
    for directory in [root.clone(), root.join("nested"), PathBuf::from("relative"), root.join("../escape")] {
        config.plugin_settings.get_mut("provider-accounts").unwrap()["profiles"]["second"]["directory"] =
            json!(directory);
        assert!(Settings::from_config(&config).is_err());
    }
    use std::os::unix::fs::symlink;
    std::fs::create_dir_all(&root).unwrap();
    let alias = dir.path().join("alias");
    symlink(&root, &alias).unwrap();
    config.plugin_settings.get_mut("provider-accounts").unwrap()["profiles"]["second"]["directory"] = json!(alias);
    assert!(Settings::from_config(&config).is_err(), "symlink aliases cannot share a credential root");
}

#[test]
fn explicit_profile_paths_keep_claude_account_metadata_inside_the_selected_root() {
    let dir = TempDir::new("accounts-claude");
    let profile = Profile { provider: Tool::Claude, directory: dir.path().join(".claude") };
    let login = json!({"claudeAiOauth":{"refreshToken":"synthetic-refresh","expiresAt":2000},"oauthAccount":{"accountUuid":"fixture"}});
    crate::hosts::logins::import_profile(&profile, &login, 0).unwrap();
    assert!(profile.directory.join(".claude.json").is_file());
    assert!(!dir.path().join(".claude.json").exists());
    assert_eq!(crate::hosts::logins::export(&profile, LoginTool::Claude).unwrap(), login);
}
