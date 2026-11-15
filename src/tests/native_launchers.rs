use super::*;
use crate::testsupport::TempDir;
use std::os::unix::fs::{symlink, PermissionsExt};

#[test]
fn installed_mise_shims_and_official_js_resolve_without_running_upgrade_wrappers() {
    let stop = AtomicBool::new(false);
    let dir = TempDir::new("native-launcher-mise");
    let binary = dir.path().join("installed/codex");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::copy("/bin/sh", &binary).unwrap();
    std::fs::copy("/bin/sh", dir.path().join("mise")).unwrap();
    symlink("mise", dir.path().join("codex")).unwrap();
    dir.write("which", format!("echo {}\n", crate::util::shell_quote(&binary.to_string_lossy())));
    let mut config = Config::default();
    config.codex_notify = false;
    config.codex_command = vec![dir.path().join("codex").to_string_lossy().into_owned()];
    let native_request =
        request(Tool::Codex, &config, "00000000-0000-4000-8000-000000000000", "", dir.path(), dir.path()).unwrap();
    let found = locate_native(&native_request, Instant::now() + Duration::from_secs(5), &stop).unwrap();
    assert_eq!(found.program, binary.canonicalize().unwrap());

    let dir = TempDir::new("native-launcher-npm");
    let root = dir.path().join("npm");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("bin/codex.js"), "throw new Error('the wrapper must not run')").unwrap();
    symlink(root.join("bin/codex.js"), dir.path().join("codex")).unwrap();
    let alias = root.join("node_modules/@openai/codex-linux-x64");
    let binary = alias.join("vendor/x86_64-unknown-linux-musl/bin/codex");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::copy("/bin/sh", &binary).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o775)).unwrap();
    let metadata = json!({"name":"@openai/codex","version":"0.167.0","optionalDependencies":{"@openai/codex-linux-x64":"npm:@openai/codex@0.167.0-linux-x64"}});
    std::fs::write(root.join("package.json"), metadata.to_string()).unwrap();
    std::fs::write(
        alias.join("package.json"),
        json!({"name":"@openai/codex","version":"0.167.0-linux-x64"}).to_string(),
    )
    .unwrap();
    config.codex_command = vec![dir.path().join("codex").to_string_lossy().into_owned()];
    let native_request =
        request(Tool::Codex, &config, "00000000-0000-4000-8000-000000000000", "", dir.path(), dir.path()).unwrap();
    let found = locate_native(&native_request, Instant::now() + Duration::from_secs(5), &stop).unwrap();
    assert_eq!(found.program, binary.canonicalize().unwrap());
    assert_eq!(found.version.as_deref(), Some("0.167.0"));
    assert!(found.environment.contains(&("CODEX_MANAGED_BY_NPM".into(), "1".into())));
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(locate_native(&native_request, Instant::now() + Duration::from_secs(5), &stop).is_err());
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o775)).unwrap();
    std::fs::write(root.join("package.json"), json!({"name":"private","version":"0.167.0"}).to_string()).unwrap();
    assert!(locate_native(&native_request, Instant::now() + Duration::from_secs(5), &stop).is_err());
}
