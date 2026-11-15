use super::*;
use crate::testsupport::TempDir;
use std::os::unix::fs::symlink;

fn paths(dir: &TempDir) -> LoginPaths {
    LoginPaths { claude_dir: dir.path().join("claude"), codex_dir: dir.path().join("codex") }
}

fn policy(path: &Path) -> Value {
    let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(
        value["bindings"].as_array().unwrap().last().unwrap(),
        &json!({"context":"Chat","bindings":{"enter":"chat:sendNow","ctrl+x ctrl+s":null}})
    );
    value
}

#[test]
fn enter_policy_merges_local_default_profiles_and_mirrors_without_credentials() {
    let dir = TempDir::new("claude-enter-consumers");
    let paths = paths(&dir);
    dir.write(
        "claude/keybindings.json",
        r#"{
        // The old aliases must not win over the final policy block.
        /* A complete block comment is valid repair input. */
        "extra":{"keep":[1,2],"literal":"/* unfinished inside a string"},
        "bindings":[
            {"context":"Chat","bindings":{"return":"chat:submit","control+x control+s":"chat:stash"}},
            {"context":"Global","bindings":{"ctrl+o":"app:toggleTranscript"}},
            {"context":"Chat","note":"keep","bindings":{"ctrl+e":"chat:externalEditor"}},
        ],
    }"#,
    );
    fs::create_dir_all(dir.path().join("windows/claude")).unwrap();
    let mut config = Config::default();
    config.plugin_settings.insert(
        "provider-accounts".into(),
        json!({"profiles":{
            "work":{"provider":"claude","directory":dir.path().join("work")},
            "other":{"provider":"codex","directory":dir.path().join("other")}
        }}),
    );
    config.plugin_settings.insert(
        "login-sync".into(),
        json!({"credential_mirrors":[{
            "claude_dir":dir.path().join("windows/claude"),"codex_dir":dir.path().join("windows/codex")
        }]}),
    );
    assert!(!compliant(&paths, &config).unwrap());
    apply(&paths, &config).unwrap();
    let kept = policy(&paths.claude_dir.join("keybindings.json"));
    assert_eq!(kept["extra"], json!({"keep":[1,2],"literal":"/* unfinished inside a string"}));
    assert_eq!(kept["bindings"][0]["bindings"]["return"], "chat:submit");
    assert_eq!(kept["bindings"][1]["bindings"]["ctrl+o"], "app:toggleTranscript");
    assert_eq!(kept["bindings"][2]["note"], "keep");
    policy(&dir.path().join("work/keybindings.json"));
    policy(&dir.path().join("windows/claude/keybindings.json"));
    assert!(!dir.path().join("other").exists(), "Codex roots are not Claude consumers");
    assert!(!paths.codex_dir.exists());
    assert!(compliant(&paths, &config).unwrap());
}

#[test]
fn enter_policy_preserves_links_and_does_not_rewrite_compliant_files() {
    let dir = TempDir::new("claude-enter-links");
    let paths = paths(&dir);
    dir.write("shared.json", r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+e":"chat:externalEditor"}}]}"#);
    fs::create_dir_all(&paths.claude_dir).unwrap();
    symlink(dir.path().join("shared.json"), paths.claude_dir.join("keybindings.json")).unwrap();
    let config = Config::default();
    apply(&paths, &config).unwrap();
    let file = dir.path().join("shared.json");
    policy(&file);
    assert!(fs::symlink_metadata(paths.claude_dir.join("keybindings.json")).unwrap().is_symlink());
    let stamp = crate::util::file_stamp(&file);
    let before = fs::read(&file).unwrap();
    assert!(compliant(&paths, &config).unwrap());
    apply(&paths, &config).unwrap();
    assert_eq!(crate::util::file_stamp(&file), stamp);
    assert_eq!(fs::read(&file).unwrap(), before);
}

#[test]
fn enter_policy_preflights_all_roots_and_preserves_invalid_owner_files() {
    for invalid in [
        "not json",
        "[]",
        "{}",
        r#"{"bindings":{}}"#,
        r#"{"bindings":[]} /* unfinished comment"#,
        r#"{"bindings":[{"context":4,"bindings":{}}]}"#,
        r#"{"bindings":[{"context":"Chat","bindings":[]}] }"#,
        r#"{"bindings":[{"context":"Chat","bindings":{"enter":true}}]}"#,
    ] {
        let dir = TempDir::new("claude-enter-invalid");
        let paths = paths(&dir);
        let original = r#"{"bindings":[],"keep":"default"}"#;
        dir.write("claude/keybindings.json", original);
        dir.write("work/keybindings.json", invalid);
        let mut config = Config::default();
        config.plugin_settings.insert(
            "provider-accounts".into(),
            json!({"profiles":{
                "work":{"provider":"claude","directory":dir.path().join("work")}
            }}),
        );
        assert!(compliant(&paths, &config).is_err(), "{invalid}");
        assert!(apply(&paths, &config).is_err(), "{invalid}");
        assert_eq!(fs::read_to_string(paths.claude_dir.join("keybindings.json")).unwrap(), original);
        assert_eq!(fs::read_to_string(dir.path().join("work/keybindings.json")).unwrap(), invalid);
    }
    let dir = TempDir::new("claude-enter-unmounted");
    let paths = paths(&dir);
    let mut config = Config::default();
    config.plugin_settings.insert("login-sync".into(), json!({"credential_mirrors":[{
        "claude_dir":dir.path().join("unmounted/windows/claude"),"codex_dir":dir.path().join("unmounted/windows/codex")
    }]}));
    assert!(apply(&paths, &config).is_err());
    assert!(!paths.claude_dir.exists(), "preflight precedes every default/profile write");
    assert!(!dir.path().join("unmounted").exists(), "missing mirrors must not create fake mount roots");
}

#[test]
fn enter_policy_rejects_nonregular_oversized_and_dangling_targets() {
    for kind in ["directory", "fifo", "oversized", "dangling"] {
        let dir = TempDir::new("claude-enter-file-boundary");
        let paths = paths(&dir);
        fs::create_dir_all(&paths.claude_dir).unwrap();
        let file = paths.claude_dir.join("keybindings.json");
        match kind {
            "directory" => fs::create_dir(&file).unwrap(),
            "fifo" => {
                use std::os::unix::ffi::OsStrExt;
                let name = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
                // SAFETY: the fixture owns this path and the CString is NUL-terminated.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            "oversized" => fs::write(&file, vec![b' '; LIMIT as usize + 1]).unwrap(),
            "dangling" => symlink(dir.path().join("absent"), &file).unwrap(),
            _ => unreachable!(),
        }
        assert!(compliant(&paths, &Config::default()).is_err(), "{kind}");
        assert!(apply(&paths, &Config::default()).is_err(), "{kind}");
        assert!(fs::symlink_metadata(&file).is_ok(), "{kind}: preserve the original object");
    }
}

#[test]
fn send_now_is_one_plain_enter_that_the_policy_binds_to_send_now() {
    // Claude 2.1.319 parses "\r" as Return (no Ctrl) and binds chat:stash to
    // ctrl+s (0x13); its sendNow chord ctrl+x ctrl+s (0x18 0x13) is unbound.
    let keys = crate::service::SEND_NOW_KEYS;
    assert_eq!(keys, "\r");
    assert!(!keys.bytes().any(|b| b == 0x13 || b == 0x18), "no Ctrl+S or Ctrl+X byte can reach Claude");
    for spelling in ["enter", "Enter", "return", " RETURN "] {
        assert_eq!(chord(spelling), ["enter"], "{spelling}");
    }
    assert_eq!(chord("Control+X  control+S"), ["ctrl+x", "ctrl+s"]);
    assert_eq!(chord("shift+ctrl+enter"), ["ctrl+shift+enter"]);
    let merged: Value = serde_json::from_slice(&merged(None).unwrap().unwrap()).unwrap();
    let last = merged["bindings"].as_array().unwrap().last().unwrap();
    let enter: Vec<_> = last["bindings"].as_object().unwrap().iter().filter(|(k, _)| chord(k) == ["enter"]).collect();
    assert_eq!(enter, [(&"enter".to_string(), &json!("chat:sendNow"))]);
    assert_eq!(last["bindings"]["ctrl+x ctrl+s"], Value::Null);
}

#[test]
fn enter_policy_refuses_bindings_that_keep_enter_from_sending_now() {
    for block in [
        json!({"context":"Autocomplete","bindings":{"return":"chat:stash"}}),
        json!({"context":"Chat","bindings":{"Enter":"chat:stash"}}),
        json!({"context":"Global","bindings":{"ctrl+x enter":"chat:stash"}}),
        json!({"context":"Chat","bindings":{"enter x":"chat:submit"}}),
        json!({"context":"Global","bindings":{"return  ctrl+s":"app:toggleTodos"}}),
    ] {
        let dir = TempDir::new("claude-enter-stash");
        let paths = paths(&dir);
        let original = serde_json::to_string(&json!({"bindings":[block]})).unwrap();
        dir.write("claude/keybindings.json", &original);
        assert!(compliant(&paths, &Config::default()).is_err(), "{original}");
        assert!(apply(&paths, &Config::default()).is_err(), "{original}");
        assert!(compliant_target(&paths.claude_dir).is_err(), "Send now refuses: {original}");
        assert_eq!(fs::read_to_string(paths.claude_dir.join("keybindings.json")).unwrap(), original);
    }
    let dir = TempDir::new("claude-enter-unrelated");
    let paths = paths(&dir);
    dir.write(
        "claude/keybindings.json",
        r#"{"bindings":[
            {"context":"Chat","bindings":{"ctrl+s":"chat:stash","shift+enter":"chat:stash","enter x":"chat:submit"}},
            {"context":"Chat","bindings":{"return x":null,"ctrl+m":"chat:stash"}}
        ]}"#,
    );
    apply(&paths, &Config::default()).unwrap();
    let kept = policy(&paths.claude_dir.join("keybindings.json"));
    assert_eq!(kept["bindings"][0]["bindings"]["ctrl+s"], "chat:stash", "unrelated owner bindings are kept");
    assert_eq!(kept["bindings"][1]["bindings"]["ctrl+m"], "chat:stash");
    assert!(compliant_target(&paths.claude_dir).unwrap());
}
