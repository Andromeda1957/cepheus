//! Copying a worktree into an invocation's anonymous container volume. The copy
//! holds the source Git would list (`git ls-files -co --exclude-standard`),
//! never dependencies, build output, databases, Git LFS content or `.env`
//! files unless a command names them with `--include`.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::lease::{Lease, SOURCE};
use super::{hosts::BuildHost, remote_command, slots_sh, ssh_args, ssh_command};
use crate::config::OffloadConfig;

/// Directories never sent wherever they appear: environments, dependency
/// and build output, and caches that each host makes for itself.
pub const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".venv",
    ".python_libs",
    "node_modules",
    "target",
    "__pycache__",
    ".gradle",
    ".mypy_cache",
    ".ruff_cache",
    ".pytest_cache",
    ".import_linter_cache",
];

/// Whether a worktree path stays behind unless a command includes it.
pub fn excluded(path: &str) -> bool {
    let mut components = path.split('/');
    let name = path.rsplit('/').next().unwrap_or(path);
    let secret = name == ".env" || (name.starts_with(".env.") && name != ".env.example");
    let database = [
        ".sqlite3",
        ".sqlite",
        ".db",
        ".sqlite3-wal",
        ".sqlite3-shm",
        ".sqlite3-journal",
        ".db-wal",
        ".db-shm",
        ".db-journal",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix));
    secret || database || components.any(|c| EXCLUDED_DIRS.contains(&c))
}

/// The paths to send: Git's listing less the excluded and LFS paths, plus
/// the included ones. Sorted and without repeats, as the manifest records.
pub fn select_files(listed: &[String], lfs: &HashSet<String>, included: &[String]) -> Vec<String> {
    let mut files: Vec<String> = listed.iter().filter(|p| !excluded(p) && !lfs.contains(p.as_str())).cloned().collect();
    files.extend(included.iter().cloned());
    files.sort();
    files.dedup();
    files
}

/// `git ls-files -z` style output as paths.
pub fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect()
}

/// The paths `git check-attr -z filter` marked `lfs`: its output is
/// path, attribute and value, each NUL-terminated.
pub fn lfs_paths(check_attr: &[u8]) -> HashSet<String> {
    let fields = split_nul_keep_empty(check_attr);
    fields.chunks(3).filter(|c| c.len() == 3 && c[2] == "lfs").map(|c| c[0].clone()).collect()
}

fn split_nul_keep_empty(bytes: &[u8]) -> Vec<String> {
    let mut fields: Vec<String> = bytes.split(|b| *b == 0).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
    if fields.last().is_some_and(String::is_empty) {
        fields.pop();
    }
    fields
}

/// The worktree a command runs from and where in it.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub root: PathBuf,
    /// The current directory relative to the root; empty at the root.
    pub cwd: String,
}

impl Worktree {
    pub fn current() -> Result<Worktree> {
        let here = std::env::current_dir().context("reading the current directory")?;
        let out = Command::new("git").args(["rev-parse", "--show-toplevel"]).current_dir(&here).output()?;
        if !out.status.success() {
            bail!("run this inside a Git worktree");
        }
        let root = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).canonicalize()?;
        let here = here.canonicalize()?;
        let cwd = here.strip_prefix(&root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Worktree { root, cwd })
    }

    /// A fresh container name shared by this invocation's build hosts.
    pub fn mirror(&self) -> String {
        let machine = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock after epoch").as_nanos();
        let identity = format!("{}:{}:{}:{stamp}", machine.trim(), self.root.display(), std::process::id());
        format!("cepheus-offload-{}", &hex_digest(identity.as_bytes())[..32])
    }

    /// The files to send, as `select_files` chooses them from Git's listing,
    /// skipping index entries whose file is gone and symlinks to absolute
    /// paths, which would mean something else on another host.
    pub fn files(&self, include: &[String]) -> Result<Vec<String>> {
        let listed = git_output(&self.root, &["ls-files", "-z", "-co", "--exclude-standard"], None)?;
        let listed = split_nul(&listed);
        let mut stdin = Vec::new();
        for path in &listed {
            stdin.extend_from_slice(path.as_bytes());
            stdin.push(0);
        }
        let attributes = git_output(&self.root, &["check-attr", "-z", "--stdin", "filter"], Some(&stdin))?;
        let lfs = lfs_paths(&attributes);
        let mut included = Vec::new();
        for path in include {
            let relative = self.relative(path)?;
            walk(&self.root, &relative, &mut included)?;
        }
        let files = select_files(&listed, &lfs, &included);
        Ok(files
            .into_iter()
            .filter(|path| match fs::symlink_metadata(self.root.join(path)) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    fs::read_link(self.root.join(path)).is_ok_and(|target| target.is_relative())
                }
                Ok(meta) => meta.is_file(),
                Err(_) => false,
            })
            .collect())
    }

    /// A path given on the command line, relative to the current directory,
    /// as a path relative to the root that stays inside it.
    pub fn relative(&self, path: &str) -> Result<String> {
        let joined = Path::new(&self.cwd).join(path);
        let mut parts: Vec<String> = Vec::new();
        for component in joined.components() {
            match component {
                std::path::Component::Normal(c) => parts.push(c.to_string_lossy().into_owned()),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir if !parts.is_empty() => {
                    parts.pop();
                }
                _ => bail!("{path} is outside the worktree"),
            }
        }
        if parts.is_empty() {
            bail!("{path} names the whole worktree");
        }
        Ok(parts.join("/"))
    }
}

pub fn hex_digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Every file below `relative` (or the file itself), for `--include`.
fn walk(root: &Path, relative: &str, out: &mut Vec<String>) -> Result<()> {
    let path = root.join(relative);
    let meta = fs::symlink_metadata(&path).with_context(|| format!("--include {relative}"))?;
    if meta.is_dir() {
        for entry in fs::read_dir(&path)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            walk(root, &format!("{relative}/{name}"), out)?;
        }
    } else {
        out.push(relative.to_string());
    }
    Ok(())
}

fn git_output(root: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git")?;
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("piped");
        let input = input.to_vec();
        // Written from its own thread: git answers while it reads.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// The NUL-separated manifest the host keeps of what it was last sent.
pub fn manifest(files: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for file in files {
        out.extend_from_slice(file.as_bytes());
        out.push(0);
    }
    out
}

/// Index the disposable source copy for repository tools. No history or host
/// cache survives the invocation. Conventional target paths stay in /scratch.
const FINISH: &str = concat!(
    r#"set -euo pipefail
cd "$1"
[ -d .git ] || git init -q
mkdir -p .git/info
printf '%s\n' /.cepheus-offload /.venv target > .git/info/exclude
git add -A
while IFS= read -r -d '' path; do
  case "$path" in
    Cargo.toml) dir=. ;;
    */Cargo.toml) dir=${path%/Cargo.toml} ;;
    *) continue ;;
  esac
  [ -e "$dir/target" ] || [ -L "$dir/target" ] || ln -s "$CARGO_TARGET_DIR" "$dir/target"
done < .cepheus-offload/manifest
"#
);

/// Copy only the selected inputs into this invocation's anonymous volume.
pub fn sync(lease: &Lease, tree: &Worktree, files: &[String]) -> Result<()> {
    let host = &lease.host;
    let list = manifest(files);
    exec_script(lease, "cat > \"$1/.cepheus-offload/manifest\"", &[SOURCE], &list)?;
    let shell = format!("ssh {}", ssh_args().join(" "));
    let mut rsync = Command::new("rsync")
        .args([
            "-a",
            "--no-owner",
            "--no-group",
            "--rsync-path=podman --remote=false unshare rsync",
            "--from0",
            "--files-from=-",
            "-e",
            &shell,
        ])
        .arg(format!("{}/", tree.root.display()))
        .arg(format!("{}:{}/", host.ssh, lease.source))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("starting rsync")?;
    {
        let mut pipe = rsync.stdin.take().expect("piped");
        pipe.write_all(&list)?;
    }
    let status = rsync.wait()?;
    if !status.success() {
        bail!("rsync to {} failed ({status})", host.name);
    }
    exec_script(lease, FINISH, &[SOURCE], &[]).context("finishing the copy")
}

/// Execute setup inside the same owned container, preserving binary stdin.
pub fn exec_script(lease: &Lease, script: &str, args: &[&str], input: &[u8]) -> Result<()> {
    let mut words = vec![lease.name.as_str(), script];
    words.extend_from_slice(args);
    const EXEC: &str = concat!(slots_sh!(), "take_slots one\ncontainer=$1; script=$2; shift 2; podman --remote=false exec --interactive \"$container\" bash -c \"$script\" cepheus-offload \"$@\"");
    run_with_input(&lease.host, EXEC, &words, input)
}

/// Run a short setup script on `host`, feeding it `input`; its own output
/// shows only when it fails.
pub fn run_with_input(host: &BuildHost, script: &str, args: &[&str], input: &[u8]) -> Result<()> {
    let mut child = ssh_command(host)
        .arg(remote_command(script, args))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ssh")?;
    let mut pipe = child.stdin.take().expect("piped");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
    });
    let out = child.wait_with_output()?;
    let _ = writer.join();
    if !out.status.success() {
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        bail!("{} ({}): {}", host.name, out.status, text.trim());
    }
    Ok(())
}

/// What a host needs to build the project's locked Python environment, as
/// the `offload` settings describe it.
#[derive(Debug, Clone, PartialEq)]
pub struct PythonLock {
    /// `<python version>-<digest>` (`environment_key`).
    pub key: String,
    pub python: String,
    /// The pinned `uv` wheel: its version and hashes from
    /// `offload.uv_requirements`. Empty: the host's own `uv`.
    pub uv_version: String,
    pub uv_hashes: Vec<String>,
    /// Arguments after `uv sync --locked` (`offload.sync_args`).
    pub sync_args: Vec<String>,
    /// Run from the mirror after syncing, `{python}` being the
    /// environment's interpreter (`offload.install`); empty: nothing.
    pub install: Vec<String>,
}

/// The project's digest: `sha256sum` of each input, then of that listing.
pub fn deps_digest(inputs: &[(&str, Vec<u8>)]) -> String {
    let listing: String = inputs.iter().map(|(path, bytes)| format!("{}  {path}\n", hex_digest(bytes))).collect();
    hex_digest(listing.as_bytes())
}

/// The environment's key: its Python version and a digest of the inputs'
/// digest, the `uv sync` arguments and the install command, so changing
/// the files or the settings builds a new environment.
pub fn environment_key(python: &str, inputs_digest: &str, sync_args: &[String], install: &[String]) -> String {
    let settings = serde_json::to_string(&(sync_args, install)).unwrap_or_default();
    let digest = hex_digest(format!("{inputs_digest}\n{settings}").as_bytes());
    format!("{python}-{}", &digest[..16])
}

/// The `uv` pin and its hashes from the hashed `uv` export.
pub fn parse_uv_requirement(text: &str) -> Option<(String, Vec<String>)> {
    let mut version = None;
    let mut hashes = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("uv==") {
            if version.is_some() {
                break;
            }
            version = Some(rest.split(|c: char| c.is_whitespace() || c == ';' || c == '\\').next()?.to_string());
            continue;
        }
        if version.is_some() {
            match line.strip_prefix("--hash=sha256:") {
                Some(hash) => hashes.push(hash.trim_end_matches('\\').trim().to_string()),
                None if line.is_empty() || line.starts_with('#') => {}
                None => break,
            }
        }
    }
    let version = version.filter(|v| !v.is_empty())?;
    (!hashes.is_empty()).then_some((version, hashes))
}

/// The Python version pinned in `text`: with no `key`, the file's first line
/// that is not blank or a comment; with one, the first `KEY: value` line
/// that names a version. Quotes around it are trimmed, and only digits and
/// dots count as a version.
pub fn parse_python_version(text: &str, key: Option<&str>) -> Option<String> {
    let version = |value: &str| {
        let value = value.trim().trim_matches('"').trim_matches('\'');
        (!value.is_empty() && value.chars().all(|c| c.is_ascii_digit() || c == '.')).then(|| value.to_string())
    };
    match key {
        None => version(text.lines().map(str::trim).find(|line| !line.is_empty() && !line.starts_with('#'))?),
        Some(key) => {
            text.lines().find_map(|line| version(line.trim().strip_prefix(key)?.trim_start().strip_prefix(':')?))
        }
    }
}

/// The version `setting` (`offload.python_version`) names: a file holding
/// only the version, or `FILE:KEY` for a `KEY: value` line in that file.
fn read_python_version(root: &Path, setting: &str) -> Result<String> {
    let (file, key) = match setting.rsplit_once(':') {
        Some((file, key)) => (file, Some(key.trim())),
        None => (setting, None),
    };
    if file.trim().is_empty() || key.is_some_and(str::is_empty) {
        bail!("offload.python_version is {setting:?}; it takes a file, or FILE:KEY");
    }
    let text =
        fs::read_to_string(root.join(file)).with_context(|| format!("reading {file} (offload.python_version)"))?;
    parse_python_version(&text, key).with_context(|| match key {
        Some(key) => format!("{file} pins no Python version as {key} (offload.python_version)"),
        None => format!("{file} holds no Python version (offload.python_version)"),
    })
}

impl PythonLock {
    pub fn read(root: &Path, cfg: &OffloadConfig) -> Result<PythonLock> {
        let mut inputs = Vec::new();
        for path in &cfg.python_inputs {
            let bytes = fs::read(root.join(path)).with_context(|| format!("reading {path} (offload.python_inputs)"))?;
            inputs.push((path.as_str(), bytes));
        }
        let python = read_python_version(root, &cfg.python_version)?;
        let (uv_version, uv_hashes) = if cfg.uv_requirements.is_empty() {
            (String::new(), Vec::new())
        } else {
            let path = &cfg.uv_requirements;
            let text = fs::read_to_string(root.join(path))
                .with_context(|| format!("reading {path} (offload.uv_requirements)"))?;
            parse_uv_requirement(&text)
                .with_context(|| format!("{path} has no hashed uv pin (offload.uv_requirements)"))?
        };
        Ok(PythonLock {
            key: environment_key(&python, &deps_digest(&inputs), &cfg.sync_args, &cfg.install),
            python,
            uv_version,
            uv_hashes,
            sync_args: cfg.sync_args.clone(),
            install: cfg.install.clone(),
        })
    }
}

/// Make the invocation's private environment for `lock`. `uv` is the pinned wheel,
/// checked against the lock's hashes, or with no pin the host's own; the
/// environment is `uv sync --locked` with the lock's arguments, then the
/// lock's install command, if any. Building it holds a slot (`slots_sh`).
/// The lock's lists arrive as positional parameters after their lengths, so
/// no word of them is ever read as shell.
const PYTHON_ENV: &str = concat!(
    r#"set -euo pipefail
mirror=$1; key=$2; python=$3; uv_version=$4; hash_count=$5; sync_count=$6; shift 6
uv_hashes=("${@:1:$hash_count}"); shift "$hash_count"
sync_args=("${@:1:$sync_count}"); shift "$sync_count"
"#,
    r#"
base=$(dirname "$mirror")
mkdir -p "$base/venv"
if [ -z "$uv_version" ]; then
  uv=$(PATH="$PATH:$HOME/.local/bin:$HOME/.cargo/bin"; command -v uv) ||
    { echo "uv is not installed; install it, or pin a wheel with offload.uv_requirements" >&2; exit 1; }
else
  uv="$base/uv/$uv_version/uv"
  if [ ! -x "$uv" ]; then
    mkdir -p "$base/uv"
    exec 7>"$base/uv/$uv_version.lock"; flock 7
    if [ ! -x "$uv" ]; then
      work=$(mktemp -d "$base/uv/.fetch.XXXXXX")
      trap 'rm -rf "$work"' EXIT
      index=$(curl -fsSL https://pypi.org/simple/uv/)
      url=; hash=
      for candidate in "${uv_hashes[@]+"${uv_hashes[@]}"}"; do
        url=$(printf '%s\n' "$index" | grep -o "https://[^\"]*/uv-$uv_version-[^\"]*manylinux[^\"]*$(uname -m)[^\"]*\.whl#sha256=$candidate" | head -n1 || true)
        if [ -n "$url" ]; then hash=$candidate; break; fi
      done
      [ -n "$url" ] || { echo "no pinned uv $uv_version wheel for $(uname -m)" >&2; exit 1; }
      curl -fsSL -o "$work/uv.whl" "${url%%#*}"
      echo "$hash  $work/uv.whl" | sha256sum -c --quiet -
      python3 -m zipfile -e "$work/uv.whl" "$work/wheel"
      mkdir -p "$base/uv/$uv_version"
      install -m 755 "$work/wheel/uv-$uv_version.data/scripts/uv" "$uv"
    fi
    exec 7>&-
  fi
fi
venv="$base/venv/$key"
exec 8>"$base/venv/$key.lock"; flock 8
if [ "$(cat "$venv/.cepheus-deps-hash" 2>/dev/null)" != "$key" ]; then
  rm -rf "$venv"
  cd "$mirror"
  UV_PROJECT_ENVIRONMENT="$venv" "$uv" sync --locked "${sync_args[@]+"${sync_args[@]}"}" --python "$python" --quiet
  if [ "$#" -gt 0 ]; then
    placeholder='{python}'; interpreter="$venv/bin/python"; install_argv=()
    for word in "$@"; do install_argv+=("${word//"$placeholder"/"$interpreter"}"); done
    "${install_argv[@]}"
  fi
  printf '%s\n' "$key" > "$venv/.cepheus-deps-hash"
fi
exec 8>&-
ln -sfn "$venv" "$mirror/.venv""#
);

/// `PYTHON_ENV`'s positional parameters for `lock` in `mirror`.
fn python_env_args(mirror: &str, lock: &PythonLock) -> Vec<String> {
    let mut args = vec![
        mirror.to_string(),
        lock.key.clone(),
        lock.python.clone(),
        lock.uv_version.clone(),
        lock.uv_hashes.len().to_string(),
        lock.sync_args.len().to_string(),
    ];
    args.extend(lock.uv_hashes.iter().cloned());
    args.extend(lock.sync_args.iter().cloned());
    args.extend(lock.install.iter().cloned());
    args
}

pub fn python_env(lease: &Lease, lock: &PythonLock) -> Result<()> {
    let args = python_env_args(SOURCE, lock);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    exec_script(lease, PYTHON_ENV, &args, &[]).context("preparing the locked Python environment")
}

#[cfg(test)]
#[path = "../tests/offload_sync.rs"]
mod tests;
