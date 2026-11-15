use super::*;
use crate::testsupport::TempDir;
use serde_json::json;

fn rule(input: &Value) -> Value {
    json!({"id":"read-source","tool":"Read","pattern":{"kind":"native_exact","input":input}})
}

#[test]
fn read_native_exact_preserves_all_options_and_proves_current_file() {
    let dir = TempDir::new("native-read-policy");
    let file = dir.write("src/main.rs", "fn main() {}\n");
    let root = dir.path().canonicalize().unwrap();
    let input = json!({"file_path":file,"offset":1,"limit":20});
    let rules = [rule(&input)];
    assert_eq!(evaluate(&input, &root, &root, &rules), Decision::AllowOnce { rule_id: "read-source".into() });
    for changed in [
        json!({"file_path":file}),
        json!({"file_path":file,"offset":2,"limit":20}),
        json!({"file_path":file,"offset":1,"limit":21}),
    ] {
        assert_eq!(evaluate(&changed, &root, &root, &rules), Decision::Decline(Category::NoRule));
    }
    std::fs::remove_file(&file).unwrap();
    assert_eq!(evaluate(&input, &root, &root, &rules), Decision::Decline(Category::UnresolvedPath));
}

#[test]
fn native_unknown_fields_numbers_and_rule_semantics_never_coerce_to_read() {
    let dir = TempDir::new("native-read-policy");
    let file = dir.write("safe.txt", "synthetic");
    let root = dir.path().canonicalize().unwrap();
    for input in [
        json!({"file_path":file,"pages":"1"}),
        json!({"file_path":file,"limit":true}),
        json!({"file_path":file,"offset":1.0}),
        json!({"file_path":file,"offset":0}),
        json!({"file_path":file,"limit":10001}),
        json!({"file_path":file,"limit":-1}),
        json!({"file_path":"relative.txt"}),
        json!({"file_path":format!("{}/./safe.txt",root.display())}),
        json!({"file_path":format!("{}/../safe.txt",root.display())}),
    ] {
        assert!(!validate_rules(&[rule(&input)]));
        assert_eq!(evaluate(&input, &root, &root, &[]), Decision::Decline(Category::InvalidInput));
    }
    let input = json!({"file_path":file});
    let good = rule(&input);
    for bad in [
        json!({"id":"read-source","tool":"Bash","pattern":{"kind":"native_exact","input":input}}),
        json!({"id":"read-source","tool":"Read","pattern":{"kind":"exact","input":input}}),
        json!({"id":"read-source","tool":"Read","pattern":{"kind":"native_exact","input":input,"extra":true}}),
        json!({"id":"read-source","tool":"Read","pattern":{"kind":"native_exact","input":input},"extra":true}),
    ] {
        assert!(!validate_rules(&[bad]));
    }
    assert!(!validate_rules(&[good.clone(), good]));
}

#[test]
fn native_read_declines_credential_paths_outside_trees_and_nonregular_files() {
    let dir = TempDir::new("native-read-policy");
    let root = dir.path().canonicalize().unwrap();
    for name in [".env", ".git/config", "credentials.json", "keys/service.key", "api-token.txt", "safe/.hidden.txt"] {
        let file = dir.write(name, "synthetic");
        let input = json!({"file_path":file});
        assert_eq!(evaluate(&input, &root, &root, &[rule(&input)]), Decision::Decline(Category::ForbiddenPath));
    }
    let outside = TempDir::new("native-read-other");
    let file = outside.write("safe.txt", "synthetic");
    let input = json!({"file_path":file});
    assert_eq!(evaluate(&input, &root, &root, &[rule(&input)]), Decision::Decline(Category::OutsideWorktree));
    let input = json!({"file_path":root});
    assert_eq!(evaluate(&input, &root, &root, &[rule(&input)]), Decision::Decline(Category::Unsupported));
}

#[cfg(unix)]
#[test]
fn native_read_declines_symlink_and_hardlink_aliases_in_current_metadata() {
    let dir = TempDir::new("native-read-policy");
    let root = dir.path().canonicalize().unwrap();
    let file = dir.write("original.txt", "synthetic");
    let alias = root.join("alias.txt");
    std::os::unix::fs::symlink(&file, &alias).unwrap();
    let input = json!({"file_path":alias});
    assert_eq!(evaluate(&input, &root, &root, &[rule(&input)]), Decision::Decline(Category::UnresolvedPath));
    let hardlink = root.join("copy.txt");
    std::fs::hard_link(&file, &hardlink).unwrap();
    let input = json!({"file_path":hardlink});
    assert_eq!(evaluate(&input, &root, &root, &[rule(&input)]), Decision::Decline(Category::Unsupported));
}

#[test]
fn native_read_checks_credentials_within_each_approved_root_and_excludes_real_stores() {
    let dir = TempDir::new("native-read-roots");
    for ancestor in ["ordinary", ".local/share/projects/project", "token-owner/.worktrees/project"] {
        let file = dir.write(&format!("{ancestor}/src/main.rs"), "fn main() {}\n");
        let root = dir.path().join(ancestor).canonicalize().unwrap();
        let input = json!({"file_path":file});
        assert_eq!(
            evaluate(&input, &root, &root, &[rule(&input)]),
            Decision::AllowOnce { rule_id: "read-source".into() }
        );
        let file = dir.write(&format!("{ancestor}/.env"), "synthetic");
        let input = json!({"file_path":file});
        assert_eq!(evaluate(&input, &root, &root, &[rule(&input)]), Decision::Decline(Category::ForbiddenPath));
    }
    for store in [".ssh", ".gnupg", ".aws", ".claude", ".codex", ".config/gh", ".local/share/cepheus"] {
        let file = dir.write(&format!("{store}/safe.txt"), "synthetic");
        let root = dir.path().join(store).canonicalize().unwrap();
        let input = json!({"file_path":file});
        assert_eq!(evaluate(&input, &root, &root, &[rule(&input)]), Decision::Decline(Category::ForbiddenPath));
    }
}
