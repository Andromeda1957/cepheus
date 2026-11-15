use super::*;
use crate::testsupport::TempDir;
use std::os::unix::fs::{symlink, PermissionsExt};

/// The real utilities the fake installers call, linked into the home's `bin`:
/// the updater's PATH holds only the home's own directories, which keeps any
/// real agent, npm or mise out of reach.
const UTILITIES: [&str; 7] = ["basename", "cat", "chmod", "ln", "mkdir", "readlink", "rm"];

/// A home with fake installers. Agents use an interpreter whose name is not a
/// shell's, so the updater does not take them for launch wrappers.
struct Home {
    dir: TempDir,
    shebang: String,
}

impl Home {
    fn new(name: &str) -> Home {
        let dir = TempDir::new(name);
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        for utility in UTILITIES {
            let real = ["/usr/bin", "/bin"]
                .into_iter()
                .map(|system| Path::new(system).join(utility))
                .find(|path| path.is_file())
                .unwrap_or_else(|| panic!("{utility} is in neither /usr/bin nor /bin"));
            symlink(real, dir.path().join("bin").join(utility)).unwrap();
        }
        fs::create_dir_all(dir.path().join("interp")).unwrap();
        symlink("/bin/sh", dir.path().join("interp/posix")).unwrap();
        let shebang = format!("#!{}\n", dir.path().join("interp/posix").display());
        Home { dir, shebang }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn env(&self) -> Env {
        Env::new(self.path(), Some(self.path().join("bin").into_os_string()))
    }

    /// An agent or installer; it does nothing when run without arguments.
    fn script(&self, rel: &str, body: &str) -> PathBuf {
        self.dir.executable(rel, format!("{}[ $# -gt 0 ] || exit 0\n{body}", self.shebang))
    }

    /// A POSIX shell script that must never run.
    fn wrapper(&self, rel: &str, tool: &str) {
        let path = self.dir.write(
            rel,
            format!(
                "#!/bin/sh\n: > \"$HOME/wrapper-ran\"\nexit 9\nmise x \"{tool}\" -- \"{}\" \"$@\"\n",
                rel.rsplit('/').next().unwrap()
            ),
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn tool(&self, rel: &str, fail: bool) -> PathBuf {
        self.script(rel, &tool(fail))
    }

    fn local(&self, check: bool) -> (bool, String) {
        let mut out = Vec::new();
        let failed = local(&self.env(), check, None, &mut out);
        (failed, String::from_utf8(out).unwrap())
    }

    fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.path().join(rel)).ok()
    }
}

/// An agent with a native `update`: version 1.0.0 until it updates to 2.0.0.
fn tool(fail: bool) -> String {
    format!(
        r#"name=$(basename "$0")
version="$HOME/$name.version"
case "$*" in
  --version) if [ -f "$version" ]; then echo "$name $(cat "$version")"; else echo "$name 1.0.0"; fi ;;
  "update --check --json") echo '{{"currentVersion": "1.0.0", "latestVersion": "2.0.0", "updateAvailable": true}}' ;;
  update) echo "$name" >> "$HOME/updates.log"; {}printf 2.0.0 > "$version"; echo updated ;;
  *) exit 8 ;;
esac
"#,
        if fail { "exit 7; " } else { "" }
    )
}

#[test]
fn missing_agents_are_skipped_without_installing_anything() {
    let home = Home::new("agent-update-missing");
    let (failed, out) = home.local(false);
    assert!(!failed, "{out}");
    assert_eq!(out.matches("not installed").count(), 4, "{out}");
    assert_eq!(home.read("updates.log"), None);
}

#[test]
fn native_updates_continue_after_one_agent_fails() {
    let home = Home::new("agent-update-native");
    for name in AGENTS {
        home.tool(&format!("bin/{name}"), name == "claude");
    }
    let (failed, out) = home.local(false);
    assert!(failed, "{out}");
    let mut updated: Vec<String> = home.read("updates.log").unwrap().lines().map(str::to_string).collect();
    updated.sort();
    assert_eq!(updated, ["agy", "claude", "codex", "grok"]);
    assert!(out.contains("  claude: failed updater exited with status 7"), "{out}");
    assert!(out.contains("  grok: checking/updating via native (1.0.0)..."), "{out}");
    assert!(out.contains("  codex: updated 2.0.0"), "{out}");
    assert_eq!(home.read("codex.version").as_deref(), Some("2.0.0"));
}

#[test]
fn check_mode_does_not_run_native_update_commands() {
    let home = Home::new("agent-update-check");
    for name in AGENTS {
        home.tool(&format!("bin/{name}"), false);
    }
    let (failed, out) = home.local(true);
    assert!(!failed, "{out}");
    assert!(out.contains("  grok: available 1.0.0 -> 2.0.0"), "{out}");
    assert!(out.contains("  claude: installed 1.0.0"), "{out}");
    assert_eq!(home.read("updates.log"), None);
}

#[test]
fn npm_agent_inside_mise_node_updates_its_prefix_without_upgrading_node() {
    let home = Home::new("agent-update-npm");
    let prefix = home.path().join("mise/installs/node/26.0.0");
    let package = prefix.join("lib/node_modules/@xai-official/grok");
    let executable = home.tool("mise/installs/node/26.0.0/lib/node_modules/@xai-official/grok/bin/grok", false);
    fs::write(package.join("package.json"), r#"{"name": "@xai-official/grok"}"#).unwrap();
    fs::write(prefix.join(".mise.backend.toml"), "short = \"node\"\n").unwrap();
    symlink(&executable, home.path().join("bin/grok")).unwrap();
    home.dir.executable("bin/mise", "#!/bin/sh\n[ $# -gt 0 ] || exit 0\necho 'must not upgrade Node' >&2\nexit 1\n");
    home.dir.executable(
        "bin/npm",
        "#!/bin/sh\n[ $# -gt 0 ] || exit 0\ncase \"$1\" in\n  view) echo '\"2.0.0\"' ;;\n  \
         install) printf '%s\\n' \"$@\" > \"$HOME/npm-args\"; printf 2.0.0 > \"$HOME/grok.version\" ;;\n  \
         *) exit 9 ;;\nesac\n",
    );
    let (failed, out) = home.local(false);
    assert!(!failed, "{out}");
    let args: Vec<String> = home.read("npm-args").unwrap().lines().map(str::to_string).collect();
    let at = args.iter().position(|arg| arg == "--prefix").unwrap();
    assert_eq!(PathBuf::from(&args[at + 1]), fs::canonicalize(&prefix).unwrap());
    assert!(args.contains(&"@xai-official/grok@latest".to_string()), "{args:?}");
    assert!(out.contains("  grok: updated 2.0.0"), "{out}");
}

#[test]
fn direct_mise_install_keeps_global_request_and_verifies_new_location() {
    let home = Home::new("agent-update-mise");
    let executable = home.tool("mise/installs/claude/1.0.0/claude", false);
    fs::write(executable.parent().unwrap().join(".mise.backend.toml"), "short = \"claude\"\n").unwrap();
    symlink(&executable, home.path().join("bin/claude")).unwrap();
    home.dir.executable(
        "bin/mise",
        r#"#!/bin/sh
[ $# -gt 0 ] || exit 0
echo "$*" >> "$HOME/mise-args"
installs="$HOME/mise/installs/claude"
case "$1" in
  ls) if [ -f "$installs/2.0.0/claude" ]; then r=2.0.0; else r=1.0.0; fi
      printf '[{"requested_version": "latest", "version": "%s", "active": true, "installed": true, "install_path": "%s"}]\n' "$r" "$installs/$r" ;;
  outdated) echo '{"claude": {"latest": "2.0.0"}}' ;;
  upgrade) [ "$*" = "upgrade --global claude --yes" ] || exit 3
    mkdir -p "$installs/2.0.0"
    printf '#!/bin/sh\necho "claude 2.0.0"\n' > "$installs/2.0.0/claude"
    chmod 755 "$installs/2.0.0/claude"
    rm "$HOME/bin/claude"
    ln -s "$installs/2.0.0/claude" "$HOME/bin/claude" ;;
  which) readlink -f "$HOME/bin/claude" ;;
  *) exit 9 ;;
esac
"#,
    );
    let (failed, out) = home.local(false);
    assert!(!failed, "{out}");
    assert!(out.contains("  claude: checking/updating via mise (1.0.0)..."), "{out}");
    assert!(out.contains("  claude: updated 2.0.0"), "{out}");
    let calls = home.read("mise-args").unwrap();
    assert!(calls.lines().any(|call| call == "which --tool claude@2.0.0 claude"), "{calls}");
    assert_eq!(home.read("updates.log"), None, "a mise-managed executable does not update itself");
}

#[test]
fn user_codex_wins_over_an_older_system_path_copy() {
    let home = Home::new("agent-update-user");
    home.tool("bin/codex", true);
    home.script(
        ".local/bin/codex",
        "case \"$*\" in\n  --version) echo 'codex 2.0.0' ;;\n  update) : > \"$HOME/user-codex-updated\" ;;\n  *) exit 9 ;;\nesac\n",
    );
    let (failed, out) = home.local(false);
    assert!(!failed, "{out}");
    assert!(home.path().join("user-codex-updated").exists());
    assert_eq!(home.read("updates.log"), None);
}

#[test]
fn read_only_check_bypasses_a_launch_wrapper_that_auto_updates() {
    let home = Home::new("agent-update-wrapper");
    home.wrapper(".local/bin/claude", "claude");
    let release = home.path().join("mise/installs/claude/1.0.0");
    home.dir.executable("mise/installs/claude/1.0.0/claude", "#!/bin/sh\necho 'claude 1.0.0'\n");
    fs::write(release.join(".mise.backend.toml"), "short = \"claude\"\n").unwrap();
    home.dir.executable(
        "bin/mise",
        &format!(
            r#"#!/bin/sh
[ $# -gt 0 ] || exit 0
case "$1" in
  which) echo "{release}/claude" ;;
  ls) echo '[{{"requested_version": "latest", "version": "1.0.0", "active": true, "installed": true, "install_path": "{release}"}}]' ;;
  outdated) echo '{{}}' ;;
  *) exit 8 ;;
esac
"#,
            release = release.display()
        ),
    );
    let (failed, out) = home.local(true);
    assert!(!failed, "{out}");
    assert!(out.contains("  claude: current 1.0.0"), "{out}");
    assert!(!home.path().join("wrapper-ran").exists());
}

#[test]
fn a_second_updater_cannot_start_while_one_holds_the_host_lock() {
    let home = Home::new("agent-update-lock");
    home.tool("bin/codex", false);
    let state = home.path().join("state/cepheus");
    fs::create_dir_all(&state).unwrap();
    let lock = File::create(state.join("agent-update.lock")).unwrap();
    // SAFETY: a valid open descriptor.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let options = Options { check: false, local: true, tool: None, config: home.path().join("config.json") };
    let mut out = Vec::new();
    assert_eq!(update(&options, &home.env(), &state, &mut out), Ok(1));
    assert!(String::from_utf8(out).unwrap().contains("already running"));
    assert_eq!(home.read("updates.log"), None);
}

#[test]
fn an_unresolved_mise_wrapper_fails_without_executing_it() {
    let home = Home::new("agent-update-unresolved");
    home.wrapper(".local/bin/claude", "claude");
    home.dir.executable("bin/mise", "#!/bin/sh\n[ $# -gt 0 ] || exit 0\nexit 8\n");
    let (failed, out) = home.local(true);
    assert!(failed, "{out}");
    assert!(out.contains("  claude: failed updater exited with status 8"), "{out}");
    assert!(!home.path().join("wrapper-ran").exists());
}

#[test]
fn mise_npm_wrapper_uses_its_exact_backend_instead_of_a_duplicate_node_global() {
    let home = Home::new("agent-update-npm-backend");
    home.tool("bin/grok", true);
    let release = home.path().join("mise/installs/npm-xai-official-grok/1.0.0");
    home.tool("mise/installs/npm-xai-official-grok/1.0.0/node_modules/@xai-official/grok/bin/grok", true);
    fs::write(release.parent().unwrap().join(".mise.backend.toml"), "short = \"npm:@xai-official/grok\"\n").unwrap();
    home.wrapper(".local/bin/grok", "npm:@xai-official/grok");
    home.dir.executable(
        "bin/mise",
        &format!(
            r#"#!/bin/sh
[ $# -gt 0 ] || exit 0
id='npm:@xai-official/grok'
case "$*" in
  "ls --global --json $id") echo '[{{"requested_version": "latest", "version": "1.0.0", "active": true, "installed": true, "install_path": "{release}"}}]' ;;
  "which --tool $id@1.0.0 grok") echo "{release}/node_modules/@xai-official/grok/bin/grok" ;;
  "outdated --global $id --json") echo '{{}}' ;;
  *) exit 8 ;;
esac
"#,
            release = release.display()
        ),
    );
    let (failed, out) = home.local(true);
    assert!(!failed, "{out}");
    assert!(out.contains("  grok: current 1.0.0"), "{out}");
    assert!(!home.path().join("wrapper-ran").exists());
    assert_eq!(home.read("updates.log"), None);
}

#[test]
fn all_hosts_are_checked_even_when_the_first_is_offline() {
    let home = Home::new("agent-update-hosts");
    let config = home.dir.write(
        "config.json",
        r#"{"hosts": [{"name": "Fedora", "ssh": "offline"}, {"name": "Ubuntu", "ssh": "ubuntu"}, {"name": "Local"}]}"#,
    );
    home.dir.executable(
        "bin/ssh",
        "#!/bin/sh\n[ $# -gt 0 ] || exit 0\nfor last; do :; done\neval \"target=\\${$(($# - 1))}\"\n\
         printf '%s\\n' \"$target\" >> \"$HOME/ssh-calls\"\nprintf '%s\\n' \"$last\" >> \"$HOME/ssh-commands\"\n\
         [ \"$target\" = offline ] && exit 255\nexit 0\n",
    );
    let options = Options { check: true, local: false, tool: None, config };
    let mut out = Vec::new();
    assert_eq!(update(&options, &home.env(), &home.path().join("state"), &mut out), Ok(1));
    let out = String::from_utf8(out).unwrap();
    assert_eq!(home.read("ssh-calls").as_deref(), Some("offline\nubuntu\n"));
    assert_eq!(
        home.read("ssh-commands").unwrap().lines().collect::<Vec<_>>(),
        ["\"$HOME/.local/bin/cepheus\" agent-update --local --check"; 2]
    );
    assert!(out.starts_with("This machine — checking\n"), "{out}");
    assert!(out.contains("\nFedora — checking\n") && out.contains("\nUbuntu — checking\n"), "{out}");
    assert_eq!(out.matches("not installed").count(), 4, "{out}");
    assert!(out.ends_with("\nFinished with failures; other agents were still processed.\n"), "{out}");
}

#[test]
fn installer_output_is_parsed_as_the_patterns_describe() {
    assert_eq!(version_in("codex-cli 0.98.0").as_deref(), Some("0.98.0"));
    assert_eq!(version_in("v2.1.3 (Claude Code)").as_deref(), Some("2.1.3"));
    // As in the Python updater, a build part after a prerelease ends the version.
    assert_eq!(version_in("x1.2.3-beta.1+b rest").as_deref(), Some("1.2.3-beta.1"));
    assert_eq!(version_in("build 1.2.3.4").as_deref(), Some("1.2.3"));
    assert_eq!(version_in("1.2 and 3"), None);
    assert!(release_version("2.0.0-rc.1") && !release_version("v2.0.0") && !release_version("2.0"));
    assert!(!release_version("2.0.0 "));
    assert!(shell_script(b"#!/bin/sh\n") && shell_script(b"#!/usr/bin/env bash\nx"));
    assert!(!shell_script(b"#!/usr/bin/python3\n# sh\n") && !shell_script(b"#!/bin/shell\n") && !shell_script(b"sh"));
    assert_eq!(wrapper_tool("exec mise x \"claude\" -- \"claude\" \"$@\"").as_deref(), Some("claude"));
    assert_eq!(wrapper_tool("mise exec 'npm:@x/y' -- y").as_deref(), Some("npm:@x/y"));
    assert_eq!(wrapper_tool("mise  x\n claude\t--").as_deref(), Some("claude"));
    assert_eq!(wrapper_tool("promise x claude -- claude"), None);
    assert_eq!(wrapper_tool("mise x claude claude"), None);
    assert_eq!(backend_identity("short = \"claude\"\n").as_deref(), Some("claude"));
    assert_eq!(backend_identity("full = \"x\"\nshort='npm:@a/b'\n").as_deref(), Some("npm:@a/b"));
    assert_eq!(backend_identity(" short = \"x\""), None);
    assert_eq!(backend_identity("short = \"\""), None);
}

#[test]
fn the_users_installations_come_first_on_the_path() {
    let env = Env::new(Path::new("/home/x"), Some("/usr/bin:/home/x/.local/bin::/bin".into()));
    assert_eq!(env.path, OsString::from("/home/x/.local/bin:/home/x/.local/share/mise/shims:/usr/bin::/bin"));
    let env = Env::new(Path::new("/home/x"), None);
    assert_eq!(
        env.path,
        OsString::from("/home/x/.local/bin:/home/x/.local/share/mise/shims:/usr/local/bin:/usr/bin:/bin")
    );
}
