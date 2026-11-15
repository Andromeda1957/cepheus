use std::process::{Command, Stdio};

use super::*;
use crate::testsupport::TempDir;

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

#[test]
fn dependencies_build_output_databases_and_env_files_stay_behind() {
    for path in [
        ".venv/bin/python",
        "web/node_modules/x/index.js",
        "cepheus/target/release/cepheus",
        "a/__pycache__/m.pyc",
        ".git/config",
        "db.sqlite3",
        "data/x.db-wal",
        ".env",
        "ops/.env.production",
        "android/.gradle/x",
    ] {
        assert!(excluded(path), "{path}");
    }
    for path in ["README.md", ".env.example", "core/models.py", "targets/list.txt", "docs/database.md", "my.envrc"] {
        assert!(!excluded(path), "{path}");
    }
}

#[test]
fn the_copy_is_gits_listing_less_lfs_and_excluded_plus_what_is_included() {
    let listed: Vec<String> = ["b.py", "a.py", "big.bin", "node_modules/x.js", "a.py"].map(String::from).to_vec();
    let lfs: HashSet<String> = ["big.bin".to_string()].into();
    let files = select_files(&listed, &lfs, &["node_modules/x.js".into(), "extra/data.db".into()]);
    assert_eq!(files, ["a.py", "b.py", "extra/data.db", "node_modules/x.js"]);
}

#[test]
fn lfs_paths_come_from_check_attr() {
    let out = b"a.png\0filter\0lfs\0b.py\0filter\0unspecified\0c d.bin\0filter\0lfs\0";
    let expected: HashSet<String> = ["a.png".to_string(), "c d.bin".to_string()].into();
    assert_eq!(lfs_paths(out), expected);
    assert_eq!(split_nul(b"a\0b c\0\0"), ["a", "b c"]);
}

#[test]
fn paths_on_the_command_line_stay_inside_the_worktree() {
    let tree = Worktree { root: "/w".into(), cwd: "cepheus".into() };
    assert_eq!(tree.relative("../docs/x.md").unwrap(), "docs/x.md");
    assert_eq!(tree.relative("src/../Cargo.toml").unwrap(), "cepheus/Cargo.toml");
    assert!(tree.relative("../../etc").is_err());
    assert!(tree.relative("/etc/passwd").is_err());
    assert!(tree.relative("..").is_err());
}

#[test]
fn the_dependency_digest_is_sha256sum_of_the_inputs_listing() {
    // `printf 'x\n' > a; : > sub/b; sha256sum a sub/b | sha256sum`
    let digest = deps_digest(&[("a", b"x\n".to_vec()), ("sub/b", Vec::new())]);
    assert_eq!(digest, "c9b149ce9d1d6d8de3e23f48c67c1720499cac956f60c1f94f20eb6a96ea5d0e");
}

#[test]
fn the_environment_key_changes_with_the_inputs_and_the_settings() {
    let none: &[String] = &[];
    let key = environment_key("3.12", "d", none, none);
    assert!(key.starts_with("3.12-") && key.len() == "3.12-".len() + 16, "{key}");
    assert_eq!(environment_key("3.12", "d", none, none), key);
    assert_ne!(environment_key("3.13", "d", none, none), key);
    assert_ne!(environment_key("3.12", "e", none, none), key);
    assert_ne!(environment_key("3.12", "d", &argv(&["--only-group", "dev"]), none), key);
    assert_ne!(environment_key("3.12", "d", none, &argv(&["{python}", "x.py"])), key);
    // A word moving from one list to the other makes another environment.
    assert_ne!(environment_key("3.12", "d", &argv(&["a"]), none), environment_key("3.12", "d", none, &argv(&["a"])));
}

#[test]
fn the_uv_pin_and_its_hashes_come_from_the_hashed_export() {
    let text = "--extra-index-url https://example.invalid\n\n\
uv==0.13.4 ; python_full_version == '3.12.*' and platform_machine == 'x86_64' \\\n    \
--hash=sha256:abc \\\n    --hash=sha256:def\nzzz==1 \\\n    --hash=sha256:fff\n";
    assert_eq!(parse_uv_requirement(text), Some(("0.13.4".into(), vec!["abc".into(), "def".into()])));
    assert_eq!(parse_uv_requirement("uv==0.13.4\n"), None);
    assert_eq!(parse_uv_requirement("pip==1 \\\n    --hash=sha256:abc\n"), None);
}

#[test]
fn the_python_version_is_a_file_of_its_own_or_a_key_in_another() {
    assert_eq!(parse_python_version("3.12\n", None), Some("3.12".into()));
    assert_eq!(parse_python_version("# pinned\n\n  '3.11.9'\n", None), Some("3.11.9".into()));
    assert_eq!(parse_python_version("system\n", None), None);
    assert_eq!(parse_python_version("", None), None);
    let key = Some("CI_PYTHON_VERSION");
    assert_eq!(parse_python_version("env:\n  CI_PYTHON_VERSION: \"3.12.13\"\n", key), Some("3.12.13".into()));
    assert_eq!(parse_python_version("env:\n  CI_PYTHON_VERSION: ${{ inputs.v }}\n", key), None);
    assert_eq!(
        parse_python_version("CI_PYTHON_VERSION_OLD: 3.10\nCI_PYTHON_VERSION : 3.12\n", key),
        Some("3.12".into())
    );
    assert_eq!(parse_python_version("env: {}\n", key), None);
}

#[test]
fn by_default_the_environment_is_a_plain_uv_projects() {
    let root = TempDir::new("offload-lock-default");
    root.write("pyproject.toml", "[project]\n");
    root.write("uv.lock", "version = 1\n");
    root.write(".python-version", "3.12\n");
    let lock = PythonLock::read(root.path(), &OffloadConfig::default()).unwrap();
    let digest = deps_digest(&[("pyproject.toml", b"[project]\n".to_vec()), ("uv.lock", b"version = 1\n".to_vec())]);
    assert_eq!(
        lock,
        PythonLock {
            key: environment_key("3.12", &digest, &[], &[]),
            python: "3.12".into(),
            uv_version: String::new(),
            uv_hashes: Vec::new(),
            sync_args: Vec::new(),
            install: Vec::new(),
        }
    );
    std::fs::remove_file(root.path().join("uv.lock")).unwrap();
    let err = format!("{:#}", PythonLock::read(root.path(), &OffloadConfig::default()).unwrap_err());
    assert!(err.contains("uv.lock") && err.contains("offload.python_inputs"), "{err}");
}

#[test]
fn a_project_names_its_inputs_version_key_uv_pin_and_commands() {
    let root = TempDir::new("offload-lock-configured");
    root.write("pyproject.toml", "a");
    root.write("ci/workflow.yml", "env:\n  CI_PYTHON_VERSION: \"3.12.13\"\n");
    root.write("ci/uv-requirements.txt", "uv==0.13.4 \\\n    --hash=sha256:abc \\\n    --hash=sha256:def\n");
    let cfg = OffloadConfig {
        python_inputs: argv(&["pyproject.toml", "ci/uv-requirements.txt"]),
        python_version: "ci/workflow.yml:CI_PYTHON_VERSION".into(),
        uv_requirements: "ci/uv-requirements.txt".into(),
        sync_args: argv(&["--only-group", "dev"]),
        install: argv(&["{python}", "install.py", "--python", "{python}"]),
        ..OffloadConfig::default()
    };
    let lock = PythonLock::read(root.path(), &cfg).unwrap();
    let digest = deps_digest(&[
        ("pyproject.toml", b"a".to_vec()),
        ("ci/uv-requirements.txt", std::fs::read(root.path().join("ci/uv-requirements.txt")).unwrap()),
    ]);
    assert_eq!(lock.key, environment_key("3.12.13", &digest, &cfg.sync_args, &cfg.install));
    assert_eq!(lock.python, "3.12.13");
    assert_eq!((lock.uv_version.as_str(), lock.uv_hashes.clone()), ("0.13.4", argv(&["abc", "def"])));
    assert_eq!(lock.sync_args, cfg.sync_args);
    assert_eq!(lock.install, cfg.install);

    for (setting, named) in
        [("ci/workflow.yml:PYTHON", "PYTHON"), ("ci/missing.yml:CI_PYTHON_VERSION", "missing.yml"), (":X", ":X")]
    {
        let cfg = OffloadConfig { python_version: setting.into(), ..cfg.clone() };
        let err = format!("{:#}", PythonLock::read(root.path(), &cfg).unwrap_err());
        assert!(err.contains(named) && err.contains("offload.python_version"), "{setting}: {err}");
    }
    let cfg = OffloadConfig { uv_requirements: "pyproject.toml".into(), ..cfg };
    let err = format!("{:#}", PythonLock::read(root.path(), &cfg).unwrap_err());
    assert!(err.contains("no hashed uv pin") && err.contains("offload.uv_requirements"), "{err}");
}

/// `PYTHON_ENV`'s arguments: the lists after their lengths, every word as given.
#[test]
fn the_environment_scripts_arguments_carry_each_list_whole() {
    let lock = PythonLock {
        key: "3.12-k".into(),
        python: "3.12".into(),
        uv_version: "0.13.4".into(),
        uv_hashes: argv(&["abc", "def"]),
        sync_args: argv(&["--only-group", "dev"]),
        install: argv(&["{python}", "a b"]),
    };
    assert_eq!(
        python_env_args("m", &lock),
        argv(&["m", "3.12-k", "3.12", "0.13.4", "2", "2", "abc", "def", "--only-group", "dev", "{python}", "a b"])
    );
}

fn bash(home: &TempDir, script: &str, args: &[&str], input: &[u8]) -> std::process::Output {
    let mut child = Command::new("bash")
        .args(["-c", script, "cepheus-offload"])
        .args(args)
        .env("HOME", home.path())
        .env("CARGO_TARGET_DIR", home.path().join("target"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), input).unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

#[test]
fn a_finished_copy_is_indexed_without_the_tools_own_files() {
    let home = TempDir::new("offload-finish");
    for path in ["m/README.md", "m/cepheus/Cargo.toml", "m/cepheus/src/main.rs"] {
        home.write(path, "x");
    }
    home.write(
        "m/.cepheus-offload/manifest",
        manifest(&["README.md".into(), "cepheus/Cargo.toml".into(), "cepheus/src/main.rs".into()]),
    );
    std::os::unix::fs::symlink("/nowhere", home.path().join("m/.venv")).unwrap();
    let mirror = home.path().join("m").display().to_string();
    let out = bash(&home, FINISH, &[&mirror], b"");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let m = home.path().join("m");
    assert!(m.join(".cepheus-offload/manifest").exists());
    let target = std::fs::read_link(m.join("cepheus/target")).unwrap();
    assert_eq!(target, home.path().join("target"));
    let listed = Command::new("git").args(["ls-files", "-z"]).current_dir(&m).output().unwrap();
    assert_eq!(split_nul(&listed.stdout), ["README.md", "cepheus/Cargo.toml", "cepheus/src/main.rs"]);
}

/// A stand-in `uv` that records its arguments and makes an environment whose
/// `python` records where it ran and its own.
const FAKE_UV: &str = r#"#!/bin/sh
[ "$#" -gt 0 ] || exit 0
printf '%s\n' "$@" > "$HOME/uv-args"
mkdir -p "$UV_PROJECT_ENVIRONMENT/bin"
cat > "$UV_PROJECT_ENVIRONMENT/bin/python" <<'EOF'
#!/bin/sh
{ pwd -P; printf '%s\n' "$0" "$@"; } > "$HOME/install-args"
EOF
chmod +x "$UV_PROJECT_ENVIRONMENT/bin/python"
"#;

/// `PYTHON_ENV` for `lock` in mirror `m`, with `home/bin` first on `PATH`.
fn python_env_on(home: &TempDir, lock: &PythonLock) -> std::process::Output {
    let path = format!("{}:{}", home.path().join("bin").display(), std::env::var("PATH").unwrap_or_default());
    Command::new("bash")
        .args(["-c", PYTHON_ENV, "cepheus-offload"])
        .args(python_env_args(&home.path().join("m").display().to_string(), lock))
        .env("HOME", home.path())
        .env("PATH", path)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn with_no_pinned_uv_the_hosts_own_builds_the_environment_once() {
    let home = TempDir::new("offload-python-env");
    home.write("m/pyproject.toml", "");
    home.executable("bin/uv", FAKE_UV);
    let lock = PythonLock {
        key: "3.12-0123456789abcdef".into(),
        python: "3.12".into(),
        uv_version: String::new(),
        uv_hashes: Vec::new(),
        sync_args: argv(&["--only-group", "dev"]),
        install: argv(&["{python}", "install.py", "--python", "{python}", "--interpreter={python}", "a b;$(false) *"]),
    };
    let out = python_env_on(&home, &lock);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let read = |name: &str| std::fs::read_to_string(home.path().join(name)).unwrap();
    assert_eq!(read("uv-args"), "sync\n--locked\n--only-group\ndev\n--python\n3.12\n--quiet\n");
    let venv = home.path().join("venv/3.12-0123456789abcdef");
    let python = venv.join("bin/python").display().to_string();
    let mirror = home.path().join("m").canonicalize().unwrap().display().to_string();
    assert_eq!(
        read("install-args"),
        format!("{mirror}\n{python}\ninstall.py\n--python\n{python}\n--interpreter={python}\na b;$(false) *\n")
    );
    assert_eq!(std::fs::read_to_string(venv.join(".cepheus-deps-hash")).unwrap(), "3.12-0123456789abcdef\n");
    assert_eq!(std::fs::read_link(home.path().join("m/.venv")).unwrap(), venv);

    // The marker matches: the next copy reuses the environment.
    std::fs::remove_file(home.path().join("uv-args")).unwrap();
    let out = python_env_on(&home, &lock);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!home.path().join("uv-args").exists());
}
