//! `cepheus agent-update [--check] [--local] [--tool AGENT] [--config FILE]`:
//! update the installed agents on this machine and the deck's hosts, each
//! through the installer that put it there (mise, npm or its own `update`).
//! Other hosts run their own `cepheus agent-update --local` over SSH.
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

const AGENTS: [&str; 4] = ["grok", "claude", "agy", "codex"];
const TIMEOUT: Duration = Duration::from_secs(240);
const REMOTE_TIMEOUT: Duration = Duration::from_secs(4 * (240 + 150) + 30);
const OUTPUT_MAX: usize = 2 * 1024 * 1024;
const USAGE: &str =
    "usage: cepheus agent-update [-h] [--local] [--check] [--tool {grok,claude,agy,codex}] [--config CONFIG]";
const UNREADABLE: &str = "Agent updater: configuration or state could not be read.";

fn package(name: &str) -> Option<&'static str> {
    match name {
        "grok" => Some("@xai-official/grok"),
        "claude" => Some("@anthropic-ai/claude-code"),
        "codex" => Some("@openai/codex"),
        _ => None,
    }
}

/// Why one agent could not be checked or updated: a message of our own, or
/// installer metadata that could not be understood.
#[derive(Debug, PartialEq)]
enum Fail {
    Update(String),
    Metadata,
}

fn fail(text: &str) -> Fail {
    Fail::Update(text.to_string())
}

impl From<std::io::Error> for Fail {
    fn from(_: std::io::Error) -> Fail {
        Fail::Metadata
    }
}

impl From<serde_json::Error> for Fail {
    fn from(_: serde_json::Error) -> Fail {
        Fail::Metadata
    }
}

/// The environment the installers run in: the user's own installations come
/// before an older root-owned copy a plain SSH session might find first.
pub struct Env {
    pub home: PathBuf,
    pub path: OsString,
}

impl Env {
    pub fn new(home: &Path, inherited: Option<OsString>) -> Env {
        let inherited = inherited.unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
        let mut dirs: Vec<PathBuf> = vec![home.join(".local/bin"), home.join(".local/share/mise/shims")];
        for dir in std::env::split_paths(&inherited) {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        let mut path = OsString::new();
        for (i, dir) in dirs.iter().enumerate() {
            if i > 0 {
                path.push(":");
            }
            path.push(dir);
        }
        Env { home: home.to_path_buf(), path }
    }

    fn which(&self, name: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.path).filter(|dir| !dir.as_os_str().is_empty()).map(|dir| dir.join(name)).find(
            |candidate| {
                let Ok(c) = std::ffi::CString::new(candidate.as_os_str().as_bytes()) else { return false };
                // SAFETY: a valid NUL-terminated path.
                candidate.exists() && !candidate.is_dir() && unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0
            },
        )
    }

    /// Every child the updater starts, installers and SSH alike: it runs in
    /// this home with this HOME and PATH, never the caller's.
    fn command(&self, argv: &[OsString]) -> Command {
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        command
    }
}

/// The path with every existing part's symbolic links resolved.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => resolved(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// Wait for `child` until `deadline`; `None` once it had to be killed.
fn wait_until(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    let pid = child.id() as libc::pid_t;
    let (done, finished) = mpsc::channel::<()>();
    let limit = deadline.saturating_duration_since(Instant::now());
    let timer = std::thread::spawn(move || {
        let expired = matches!(finished.recv_timeout(limit), Err(mpsc::RecvTimeoutError::Timeout));
        if expired {
            // SAFETY: our own child, not yet reaped by the waiting thread.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        expired
    });
    let status = child.wait().ok();
    let _ = done.send(());
    if timer.join().unwrap_or(false) {
        None
    } else {
        status
    }
}

fn exit_code(status: ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    status.code().map_or_else(|| -(status.signal().unwrap_or(0) as i64), i64::from)
}

/// Run an installer step and return its trimmed output. Its diagnostics,
/// npm configuration or tokens are never echoed.
fn run(env: &Env, argv: &[OsString], timeout: Duration) -> Result<String, Fail> {
    let mut child = env
        .command(argv)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| fail("updater could not be started"))?;
    let deadline = Instant::now() + timeout;
    let mut stdout = child.stdout.take().expect("piped");
    let mut output = Vec::new();
    let mut overflow = false;
    let mut chunk = vec![0u8; 65536];
    let read = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut poll = libc::pollfd { fd: stdout.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().min(i32::MAX as u128) as i32) };
        if ready < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if ready <= 0 || remaining.is_zero() {
            break Err(fail("updater timed out"));
        }
        match stdout.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(n) if output.len() + n <= OUTPUT_MAX => output.extend_from_slice(&chunk[..n]),
            Ok(_) => overflow = true,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break Err(Fail::Metadata),
        }
    };
    drop(stdout);
    if let Err(error) = read {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let status = wait_until(&mut child, deadline).ok_or_else(|| fail("updater timed out"))?;
    if !status.success() {
        return Err(Fail::Update(format!("updater exited with status {}", exit_code(status))));
    }
    if overflow {
        return Err(fail("updater response exceeded 2 MB"));
    }
    Ok(String::from_utf8(output).map_err(|_| Fail::Metadata)?.trim().to_string())
}

fn os(text: impl AsRef<OsStr>) -> OsString {
    text.as_ref().to_os_string()
}

fn json(env: &Env, argv: &[OsString], timeout: Duration) -> Result<Value, Fail> {
    Ok(serde_json::from_str(&run(env, argv, timeout)?)?)
}

/// Python's truth value of a JSON member.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(members)) => !members.is_empty(),
    }
}

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The first `[v]N.N.N[-suffix]` in `text` not preceded by a digit, as the
/// Python updater's `(?<!\d)v?(\d+\.\d+\.\d+(?:[-+][\w.-]+)?)` found it: the
/// suffix starts with `-` or `+` and holds only word characters, dots and
/// hyphens, so a `+build` after a prerelease is not part of it.
pub fn version_in(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let digits = |mut i: usize| {
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        (i > start).then_some(i)
    };
    for start in 0..chars.len() {
        if start > 0 && chars[start - 1].is_ascii_digit() {
            continue;
        }
        let begin = start + usize::from(chars[start] == 'v');
        let mut end = begin;
        let mut parts = 0;
        while let Some(next) = digits(end) {
            parts += 1;
            end = next;
            if parts == 3 {
                break;
            }
            if chars.get(end) != Some(&'.') {
                break;
            }
            end += 1;
        }
        if parts < 3 {
            continue;
        }
        if matches!(chars.get(end), Some('-' | '+'))
            && chars.get(end + 1).is_some_and(|&c| word(c) || c == '.' || c == '-')
        {
            end += 1;
            while chars.get(end).is_some_and(|&c| word(c) || c == '.' || c == '-') {
                end += 1;
            }
        }
        return Some(chars[begin..end].iter().collect());
    }
    None
}

/// Whether `text` is wholly `N.N.N[-suffix]`.
fn release_version(text: &str) -> bool {
    version_in(text).is_some_and(|found| found == text) && !text.starts_with('v')
}

/// A `#!` line naming a POSIX shell.
pub fn shell_script(head: &[u8]) -> bool {
    let Some(rest) = head.strip_prefix(b"#!") else { return false };
    let line = rest.split(|&b| b == b'\n').next().unwrap_or_default();
    line.split(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
        .any(|token| matches!(token, b"bash" | b"dash" | b"zsh" | b"sh"))
}

/// The tool a launch wrapper's `mise x TOOL --` or `mise exec TOOL --` names.
pub fn wrapper_tool(script: &str) -> Option<String> {
    let chars: Vec<char> = script.chars().collect();
    let space = |mut i: usize| {
        let start = i;
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        (i > start).then_some(i)
    };
    let starts = |i: usize, text: &str| text.chars().enumerate().all(|(k, c)| chars.get(i + k) == Some(&c));
    let quote = |c: Option<&char>| matches!(c, Some('"' | '\''));
    (0..chars.len()).find_map(|at| {
        if !starts(at, "mise") || (at > 0 && word(chars[at - 1])) {
            return None;
        }
        let mut i = space(at + 4)?;
        i = if chars.get(i) == Some(&'x') && space(i + 1).is_some() {
            i + 1
        } else if starts(i, "exec") {
            i + 4
        } else {
            return None;
        };
        i = space(i)?;
        if quote(chars.get(i)) {
            i += 1;
        }
        let begin = i;
        while chars.get(i).is_some_and(|&c| !c.is_whitespace() && c != '"' && c != '\'') {
            i += 1;
        }
        if i == begin {
            return None;
        }
        let tool: String = chars[begin..i].iter().collect();
        if quote(chars.get(i)) {
            i += 1;
        }
        let i = space(i)?;
        starts(i, "--").then_some(tool)
    })
}

/// The `short = "…"` identity a mise backend's metadata file names.
pub fn backend_identity(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let skip_space = |mut i: usize| {
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        i
    };
    let quote = |c: Option<&char>| matches!(c, Some('"' | '\''));
    (0..chars.len()).filter(|&i| i == 0 || chars[i - 1] == '\n').find_map(|line| {
        if !"short".chars().enumerate().all(|(k, c)| chars.get(line + k) == Some(&c)) {
            return None;
        }
        let mut i = skip_space(line + 5);
        if chars.get(i) != Some(&'=') {
            return None;
        }
        i = skip_space(i + 1);
        if !quote(chars.get(i)) {
            return None;
        }
        let begin = i + 1;
        let mut end = begin;
        while chars.get(end).is_some_and(|&c| c != '"' && c != '\'') {
            end += 1;
        }
        (end > begin && quote(chars.get(end))).then(|| chars[begin..end].iter().collect())
    })
}

fn installed_version(env: &Env, executable: &Path) -> Result<String, Fail> {
    let text = run(env, &[os(executable), os("--version")], Duration::from_secs(20))?;
    version_in(&text).ok_or_else(|| fail("installed version could not be verified"))
}

const NOT_GLOBAL: &str = "mise installation is not configured globally; existing version request preserved";

/// The executable of the globally configured installation of `identity`.
fn mise_executable(env: &Env, mise: &Path, identity: &str, name: &str) -> Result<PathBuf, Fail> {
    let records =
        json(env, &[os(mise), os("ls"), os("--global"), os("--json"), os(identity)], Duration::from_secs(45))?;
    let Value::Array(records) = records else { return Err(fail("invalid mise installation metadata")) };
    let installed: Vec<&Value> = records
        .iter()
        .filter(|item| item.is_object() && truthy(item.get("active")) && truthy(item.get("installed")))
        .collect();
    if installed.len() != 1 || !truthy(installed[0].get("requested_version")) {
        return Err(fail(NOT_GLOBAL));
    }
    let item = installed[0];
    let release = match item.get("version") {
        None => "",
        Some(Value::String(text)) => text.as_str(),
        Some(_) => return Err(fail("invalid mise installed version")),
    };
    if release.is_empty() || !release.chars().all(|c| word(c) || matches!(c, '.' | '+' | '-')) {
        return Err(fail("invalid mise installed version"));
    }
    // Without @version, mise can pick a Node-global executable instead of the
    // separately configured npm backend with the same executable name.
    let tool = format!("{identity}@{release}");
    let candidate =
        PathBuf::from(run(env, &[os(mise), os("which"), os("--tool"), os(&tool), os(name)], Duration::from_secs(30))?);
    let root = match item.get("install_path") {
        None => PathBuf::new(),
        Some(Value::String(text)) => PathBuf::from(text),
        Some(_) => return Err(Fail::Metadata),
    };
    if !candidate.is_absolute()
        || !candidate.is_file()
        || !root.is_absolute()
        || !resolved(&candidate).starts_with(resolved(&root))
    {
        return Err(fail("mise did not identify the configured installed executable"));
    }
    Ok(candidate)
}

#[derive(Debug)]
struct Install {
    executable: PathBuf,
    manager: &'static str,
    command: Vec<OsString>,
    mise: Option<(PathBuf, String)>,
    npm: Option<Vec<OsString>>,
}

fn inspect(env: &Env, name: &str) -> Result<Option<Install>, Fail> {
    let Some(mut executable) = env.which(name) else { return Ok(None) };
    let mise = env.which("mise");
    let mut real = resolved(&executable);
    let identities: Vec<String> =
        std::iter::once(name.to_string()).chain(package(name).map(|p| format!("npm:{p}"))).collect();
    // A launch wrapper may upgrade itself even for --version: recognize a
    // literal, known mise identity without running the script.
    let mut head = Vec::new();
    File::open(&executable)?.take(16385).read_to_end(&mut head)?;
    let mut wrapped = None;
    if shell_script(&head) {
        let tool = wrapper_tool(&String::from_utf8_lossy(&head));
        match (tool, &mise) {
            (Some(tool), Some(_)) if identities.contains(&tool) => wrapped = Some(tool),
            _ => return Err(fail("mise launch wrapper could not be resolved safely")),
        }
    }
    let shim = mise.as_ref().is_some_and(|mise| {
        real == resolved(mise) || executable.parent().and_then(Path::file_name) == Some(OsStr::new("shims"))
    });
    if let (Some(mise), true) = (&mise, wrapped.is_some() || shim) {
        executable = match &wrapped {
            Some(tool) => mise_executable(env, mise, tool, name)?,
            None => {
                let candidate = PathBuf::from(run(env, &[os(mise), os("which"), os(name)], Duration::from_secs(30))?);
                if candidate.as_os_str().is_empty() || !candidate.is_absolute() || !candidate.is_file() {
                    return Err(fail("mise did not identify an installed executable"));
                }
                candidate
            }
        };
        real = resolved(&executable);
    }
    // The agent's own mise backend, including direct npm backends. A mise
    // Node prefix has short = "node" and is not an agent's.
    if let Some(mise) = &mise {
        for parent in real.ancestors().skip(1).take(16) {
            let metadata = parent.join(".mise.backend.toml");
            if !fs::metadata(&metadata).is_ok_and(|meta| meta.is_file() && meta.len() < 16384) {
                continue;
            }
            let text = String::from_utf8(fs::read(&metadata)?).map_err(|_| Fail::Metadata)?;
            let Some(identity) = backend_identity(&text).filter(|short| identities.contains(short)) else {
                continue;
            };
            if wrapped.as_ref().is_some_and(|tool| *tool != identity) {
                return Err(fail("mise resolved a different installation than the launch wrapper"));
            }
            return Ok(Some(Install {
                executable,
                manager: "mise",
                command: vec![os(mise), os("upgrade"), os("--global"), os(&identity), os("--yes")],
                mise: Some((mise.clone(), identity)),
                npm: None,
            }));
        }
    }
    // npm inside a mise Node prefix installs the agent with npm; Node itself
    // is never upgraded.
    for parent in real.ancestors().skip(1).take(16) {
        let manifest = parent.join("package.json");
        if !fs::metadata(&manifest).is_ok_and(|meta| meta.is_file() && meta.len() <= 1024 * 1024) {
            continue;
        }
        let Ok(Value::Object(fields)) =
            fs::read(&manifest).map_err(drop).and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(drop))
        else {
            continue;
        };
        let Some(wanted) = package(name).filter(|wanted| fields.get("name") == Some(&Value::from(*wanted))) else {
            continue;
        };
        let modules = parent.ancestors().skip(1).find(|path| path.file_name() == Some(OsStr::new("node_modules")));
        let Some(lib) = modules.and_then(Path::parent).filter(|lib| lib.file_name() == Some(OsStr::new("lib"))) else {
            return Err(fail("unrecognized global npm installation layout"));
        };
        let prefix = lib.parent().unwrap_or(Path::new("/"));
        let node = prefix.join("bin/node");
        let npm_cli = prefix.join("lib/node_modules/npm/bin/npm-cli.js");
        let npm = if node.is_file() && npm_cli.is_file() {
            vec![os(&node), os(&npm_cli)]
        } else {
            vec![os(env.which("npm").ok_or_else(|| fail("npm is not available for this installation"))?)]
        };
        let mut command = npm.clone();
        command.extend(["install", "--global", "--prefix"].map(os).into_iter().chain([
            os(prefix),
            os(format!("{wanted}@latest")),
            os("--no-audit"),
            os("--no-fund"),
        ]));
        return Ok(Some(Install { executable, manager: "npm", command, mise: None, npm: Some(npm) }));
    }
    // The official user installers of all four tools have `update`.
    let command = vec![os(&executable), os("update")];
    Ok(Some(Install { executable, manager: "native", command, mise: None, npm: None }))
}

/// One agent's outcome, as its line reports it.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub status: &'static str,
    pub version: Option<String>,
    pub latest: Option<Value>,
    pub message: Option<String>,
}

fn update_one(env: &Env, name: &str, check: bool, out: &mut dyn Write) -> Outcome {
    match try_update(env, name, check, out) {
        Ok(outcome) => outcome,
        Err(error) => Outcome {
            status: "failed",
            message: Some(match error {
                Fail::Update(text) => text,
                Fail::Metadata => "invalid installer metadata".into(),
            }),
            ..Outcome::default()
        },
    }
}

fn try_update(env: &Env, name: &str, check: bool, out: &mut dyn Write) -> Result<Outcome, Fail> {
    let Some(install) = inspect(env, name)? else {
        return Ok(Outcome { status: "skipped", message: Some("not installed".into()), ..Outcome::default() });
    };
    let old = installed_version(env, &install.executable)?;
    let current = |version: &str| -> Result<Outcome, Fail> {
        Ok(Outcome { status: "current", version: Some(version.into()), ..Outcome::default() })
    };
    let mut latest = None;
    if let Some((mise, tool)) = &install.mise {
        let configured =
            json(env, &[os(mise), os("ls"), os("--global"), os("--json"), os(tool)], Duration::from_secs(45))?;
        let global = configured.as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item.is_object()
                    && truthy(item.get("active"))
                    && truthy(item.get("installed"))
                    && truthy(item.get("requested_version"))
            })
        });
        if !global {
            return Err(fail(NOT_GLOBAL));
        }
        let outdated =
            json(env, &[os(mise), os("outdated"), os("--global"), os(tool), os("--json")], Duration::from_secs(90))?;
        if !truthy(Some(&outdated)) {
            return current(&old);
        }
        let Value::Object(outdated) = outdated else { return Err(Fail::Metadata) };
        latest = outdated.get(tool).and_then(|item| item.as_object()).and_then(|item| item.get("latest")).cloned();
    } else if let Some(npm) = &install.npm {
        let mut argv = npm.clone();
        argv.extend([os("view"), os(package(name).unwrap_or_default()), os("version"), os("--json")]);
        let release = json(env, &argv, Duration::from_secs(90))?;
        let Some(release) = release.as_str().filter(|text| release_version(text)).map(str::to_string) else {
            return Err(fail("npm returned an invalid release version"));
        };
        if old == release {
            return current(&old);
        }
        latest = Some(Value::String(release));
    } else if name == "grok" {
        let info =
            json(env, &[os(&install.executable), os("update"), os("--check"), os("--json")], Duration::from_secs(90))?;
        let Value::Object(info) = info else { return Err(Fail::Metadata) };
        if truthy(info.get("error")) {
            return Err(fail("native release check failed"));
        }
        if info.get("updateAvailable") == Some(&Value::Bool(false)) {
            return current(&old);
        }
        latest = info.get("latestVersion").cloned();
    }
    if check {
        let available = truthy(latest.as_ref());
        return Ok(Outcome {
            status: if available { "available" } else { "installed" },
            version: Some(old),
            latest,
            message: None,
        });
    }
    let _ = writeln!(out, "  {name}: checking/updating via {} ({old})...", install.manager);
    let _ = out.flush();
    run(env, &install.command, TIMEOUT)?;
    // A mise upgrade may move `latest` to a new folder: verify the new one.
    let executable = match &install.mise {
        Some((mise, tool)) => mise_executable(env, mise, tool, name)?,
        None => install.executable,
    };
    let new = installed_version(env, &executable)?;
    Ok(Outcome {
        status: if new != old { "updated" } else { "current" },
        version: Some(new),
        latest: None,
        message: None,
    })
}

/// Check or update every agent here, or only `tool`; true when any failed.
pub fn local(env: &Env, check: bool, tool: Option<&str>, out: &mut dyn Write) -> bool {
    let mut failed = false;
    for name in AGENTS.into_iter().filter(|name| tool.is_none_or(|tool| tool == *name)) {
        let outcome = update_one(env, name, check, out);
        let mut detail = outcome.message.clone().or(outcome.version.clone()).unwrap_or_default();
        if truthy(outcome.latest.as_ref()) {
            let latest = outcome.latest.as_ref().unwrap();
            detail += &format!(" -> {}", latest.as_str().map_or_else(|| latest.to_string(), str::to_string));
        }
        let _ = writeln!(out, "  {name}: {} {detail}", outcome.status);
        let _ = out.flush();
        failed |= outcome.status == "failed";
    }
    failed
}

/// A host's own updater over SSH, its output shown as it runs; true when it failed.
fn remote(env: &Env, ssh: &str, check: bool, tool: Option<&str>, out: &mut dyn Write) -> bool {
    let command = format!(
        "\"$HOME/.local/bin/cepheus\" agent-update --local{}{}",
        if check { " --check" } else { "" },
        tool.map(|tool| format!(" --tool {tool}")).unwrap_or_default()
    );
    let argv: Vec<OsString> = [
        "ssh",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=2",
        "--",
        ssh,
        &command,
    ]
    .map(os)
    .to_vec();
    let status =
        env.command(&argv).spawn().ok().and_then(|mut child| wait_until(&mut child, Instant::now() + REMOTE_TIMEOUT));
    match status {
        Some(status) => !status.success(),
        None => {
            let _ = writeln!(out, "  host unavailable or update timed out");
            let _ = out.flush();
            true
        }
    }
}

pub struct Options {
    pub check: bool,
    pub local: bool,
    /// Only this agent (one of `AGENTS`), as when the service updates the
    /// one whose session said a newer version is out.
    pub tool: Option<&'static str>,
    pub config: PathBuf,
}

/// The whole run; `Err` when the configuration or this host's state could
/// not be read.
pub fn update(options: &Options, env: &Env, state_dir: &Path, out: &mut dyn Write) -> Result<i32, ()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(state_dir).map_err(drop)?;
    let lock = OpenOptions::new().create(true).append(true).open(state_dir.join("agent-update.lock")).map_err(drop)?;
    // SAFETY: a valid open descriptor; never blocks.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let _ = writeln!(out, "An agent update is already running on this machine.");
        return Ok(1);
    }
    let doing = if options.check { "checking" } else { "checking and updating" };
    let _ = writeln!(out, "This machine — {doing}");
    let _ = out.flush();
    let mut failed = local(env, options.check, options.tool, out);
    if !options.local {
        let config: Value = if options.config.is_file() {
            serde_json::from_slice(&fs::read(&options.config).map_err(drop)?).map_err(drop)?
        } else {
            Value::Object(Default::default())
        };
        let Value::Object(config) = config else { return Err(()) };
        for host in config.get("hosts").and_then(Value::as_array).into_iter().flatten() {
            let Some(ssh) = host.get("ssh").and_then(Value::as_str).filter(|ssh| !ssh.is_empty()) else { continue };
            let label = [host.get("label"), host.get("name")]
                .into_iter()
                .flatten()
                .find(|value| truthy(Some(value)))
                .map_or_else(
                    || "Remote host".to_string(),
                    |value| value.as_str().map_or_else(|| value.to_string(), str::to_string),
                )
                .replace('\n', " ")
                .chars()
                .take(80)
                .collect::<String>();
            let _ = writeln!(out, "\n{label} — {doing}");
            let _ = out.flush();
            failed |= remote(env, ssh, options.check, options.tool, out);
        }
    }
    let _ = writeln!(
        out,
        "\nFinished{}",
        if failed { " with failures; other agents were still processed." } else { ". Existing sessions keep running." }
    );
    let _ = out.flush();
    Ok(i32::from(failed))
}

pub fn run_cli(args: &[String]) -> i32 {
    let mut options = Options { check: false, local: false, tool: None, config: crate::paths::config_path() };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}\n\nUpdate installed agents on the deck's hosts, using their actual installers.");
                return 0;
            }
            "--local" => options.local = true,
            "--check" => options.check = true,
            "--tool" => match args.next().and_then(|tool| AGENTS.into_iter().find(|name| *name == tool.as_str())) {
                Some(tool) => options.tool = Some(tool),
                None => return usage("argument --tool: expected one of grok, claude, agy, codex"),
            },
            "--config" => match args.next() {
                Some(path) => options.config = PathBuf::from(path),
                None => return usage("argument --config: expected one argument"),
            },
            other => match other.strip_prefix("--config=") {
                Some(path) => options.config = PathBuf::from(path),
                None => return usage(&format!("unrecognized arguments: {other}")),
            },
        }
    }
    let env = Env::new(&crate::paths::home(), std::env::var_os("PATH"));
    let mut stdout = std::io::stdout();
    match update(&options, &env, &crate::paths::state_dir(), &mut stdout) {
        Ok(code) => code,
        Err(()) => {
            eprintln!("{UNREADABLE}");
            1
        }
    }
}

fn usage(error: &str) -> i32 {
    eprintln!("{USAGE}\ncepheus agent-update: error: {error}");
    2
}

#[cfg(test)]
#[path = "../../src/tests/agent_update.rs"]
mod tests;
