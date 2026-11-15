//! Run builds and tests on available configured build hosts.
//!
//! `cepheus run` copies the worktree to the least busy host and runs one
//! command there; `cepheus shard` splits a test selection across every host
//! with free cores. Both report what ran and never record a verification
//! receipt: CI gates stay with CI. The project's Python environment and
//! shard plan come from the config's `offload` settings.

pub mod hosts;
pub mod lease;
pub mod results;
pub mod shard;
pub mod sync;

use std::process::{Command, ExitStatus, Stdio};

use anyhow::{bail, Context, Result};

use crate::config::OffloadConfig;
use hosts::BuildHost;
use sync::{PythonLock, Worktree};

/// SSH without prompts, giving up on a host that does not answer, and
/// noticing one that stops answering during a long run.
pub fn ssh_args() -> Vec<String> {
    [
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ServerAliveCountMax=4",
    ]
    .map(String::from)
    .to_vec()
}

pub fn ssh_command(host: &BuildHost) -> Command {
    let mut command = Command::new("ssh");
    command.args(ssh_args()).arg(&host.ssh);
    command
}

/// One word for the remote login shell, whichever shell it is: always single
/// quoted, so nothing in it expands.
pub fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The line SSH hands the host's login shell: `script` run by bash with
/// `args` as its positional parameters.
pub fn remote_command(script: &str, args: &[&str]) -> String {
    let mut line = format!("bash -c {} cepheus-offload", quote(script));
    for arg in args {
        line.push(' ');
        line.push_str(&quote(arg));
    }
    line
}

/// Bound offloaded work to at most half a build host's cores, leaving capacity for unrelated work. Each of the
/// `cap` slots is a lock file under `~/.cache/cepheus-offload/slots`, shared by all callers. A process holds
/// its slots while compiling or testing; callers wait in `flock` when none are free. `take_slots one` takes one
/// slot; `take_slots all` takes the host's whole offload share; `close_slots` keeps them out of a child's open
/// files.
macro_rules! slots_sh {
    () => {
        r#"cap=$(( $(nproc) / 2 )); [ "$cap" -ge 1 ] || cap=1
slot_dir="$HOME/.cache/cepheus-offload/slots"
mkdir -p "$slot_dir"
slot_fds=()
take_slots() {
  local i fd said=
  if [ "$1" = all ]; then
    for ((i = 0; i < cap; i++)); do
      exec {fd}>"$slot_dir/$i"
      if ! flock -n -x "$fd"; then
        [ -n "$said" ] || echo "[offload] $(hostname): waiting for other offloaded work to finish" >&2
        said=1
        flock -x "$fd"
      fi
      slot_fds+=("$fd")
    done
    return
  fi
  for ((i = 0; i < cap; i++)); do
    exec {fd}>"$slot_dir/$i"
    if flock -n -x "$fd"; then slot_fds+=("$fd"); return; fi
    exec {fd}>&-
  done
  echo "[offload] $(hostname): waiting for a free slot" >&2
  exec {fd}>"$slot_dir/$(( $$ % cap ))"
  flock -x "$fd"
  slot_fds+=("$fd")
}
close_slots() { local fd; for fd in "${slot_fds[@]+"${slot_fds[@]}"}"; do exec {fd}>&-; done; }
"#
    };
}
pub(crate) use slots_sh;

/// Runs a command in its container while holding its host
/// slots (`slots_sh`). Cargo is held to the slots taken. Its standard
/// input is the SSH channel's: when the caller goes away the channel closes
/// and the command's whole process group is ended, so nothing keeps running
/// for a caller that is gone. The independent lease removes its container
/// and volume on caller loss. `@STATE@` is the container's state directory.
const RUN: &str = concat!(
    r#"set -uo pipefail
container=$1; cwd=$2; python=$3; want=$4; shift 4
assignments=()
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do assignments+=("$1"); shift; done
[ "$#" -gt 0 ] && shift
exec 3<&0
"#,
    slots_sh!(),
    r#"take_slots "$want"
state=/scratch/source/.cepheus-offload/state
options=(--env "CARGO_BUILD_JOBS=${#slot_fds[@]}" --env "RUST_TEST_THREADS=${#slot_fds[@]}")
for assignment in "${assignments[@]+"${assignments[@]}"}"; do options+=(--env "${assignment//@STATE@/$state}"); done
set -- "${@//@STATE@/$state}"
( close_slots; exec setsid -w podman --remote=false exec --workdir "/scratch/source/$cwd" "${options[@]}" "$container" bash -c '
python=$1; shift
if [ "$python" = 1 ]; then
  export VIRTUAL_ENV=/scratch/source/.venv PATH="/scratch/source/.venv/bin:$PATH" PYTHONPATH="/scratch/source${PYTHONPATH:+:$PYTHONPATH}"
fi
exec "$@"' cepheus-offload "$python" "$@" ) </dev/null 3<&- &
pid=$!
( close_slots; cat <&3; kill -TERM -- -"$pid"; sleep 10; kill -KILL -- -"$pid" ) >/dev/null 2>&1 &
watcher=$!
exec 3<&-
wait "$pid"; status=$?
kill "$watcher" 2>/dev/null
exit "$status""#
);

/// How much of a host's share of cores a remote command holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Slots {
    /// One slot: a test shard, a setup step.
    One,
    /// Every slot: a command that uses all the cores it is given, a build.
    All,
}

/// What a remote command needs besides its argv.
pub struct RemoteRun<'a> {
    pub host: &'a BuildHost,
    pub mirror: &'a str,
    pub cwd: &'a str,
    /// `Some(wrap)`: `argv` runs in the project's locked Python environment,
    /// after `wrap` (`offload.wrap`, possibly empty). `None`: as given.
    pub python: Option<&'a [String]>,
    pub slots: Slots,
    pub environment: &'a [String],
    pub argv: &'a [String],
}

impl RemoteRun<'_> {
    pub fn command(&self) -> Command {
        let python = if self.python.is_some() { "1" } else { "0" };
        let slots = match self.slots {
            Slots::One => "one",
            Slots::All => "all",
        };
        let mut args: Vec<&str> = vec![self.mirror, self.cwd, python, slots];
        args.extend(self.environment.iter().map(String::as_str));
        args.push("--");
        args.extend(self.python.unwrap_or_default().iter().map(String::as_str));
        args.extend(self.argv.iter().map(String::as_str));
        let mut command = ssh_command(self.host);
        command.arg(remote_command(RUN, &args));
        command
    }

    /// Run it with output going to `stdout` and `stderr`. Standard input
    /// stays open until it ends (see `RUN`).
    pub fn status(&self, stdout: Stdio, stderr: Stdio) -> Result<ExitStatus> {
        let mut child =
            self.command().stdin(Stdio::piped()).stdout(stdout).stderr(stderr).spawn().context("starting ssh")?;
        let hold = child.stdin.take();
        let status = child.wait()?;
        drop(hold);
        Ok(status)
    }
}

/// `NAME=VALUE`, with a name a shell can export.
pub fn valid_assignment(text: &str) -> bool {
    text.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

const RUN_USAGE: &str = "\
usage: cepheus run [--host auto|NAME] [--python] [--env NAME=VALUE]... [--include PATH]...
                   [--artifact PATH]... -- COMMAND [ARG]...

Copy this worktree to the least busy build host and run COMMAND there, in the
same directory relative to the worktree, streaming its output and exiting
with its status. COMMAND gets the host's whole offload share, half its cores
(CARGO_BUILD_JOBS and RUST_TEST_THREADS say how many), and waits while other
offloaded work holds it; --host auto skips a host whose own work already
keeps half its cores busy. --python runs it in the project's locked Python
environment, as the config's offload settings describe it; --include sends a
path the copy leaves out (dependencies, build output, databases, Git LFS
files); --artifact copies a path back afterwards.";

#[derive(Debug, Default, PartialEq)]
pub struct RunArgs {
    pub host: String,
    pub python: bool,
    pub environment: Vec<String>,
    pub include: Vec<String>,
    pub artifacts: Vec<String>,
    pub command: Vec<String>,
}

/// The value after option `args[*i]`, moving past it.
pub fn option_value(args: &[String], i: &mut usize) -> Result<String> {
    let name = &args[*i];
    *i += 1;
    args.get(*i).cloned().with_context(|| format!("{name} needs a value"))
}

/// An `--env` value, checked.
pub fn assignment(args: &[String], i: &mut usize) -> Result<String> {
    let value = option_value(args, i)?;
    if !valid_assignment(&value) {
        bail!("--env takes NAME=VALUE, not {value:?}");
    }
    Ok(value)
}

pub fn parse_run(args: &[String]) -> Result<RunArgs> {
    let mut parsed = RunArgs { host: "auto".into(), ..RunArgs::default() };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                parsed.command = args[i + 1..].to_vec();
                break;
            }
            "--host" => parsed.host = option_value(args, &mut i)?,
            "--python" => parsed.python = true,
            "--env" => parsed.environment.push(assignment(args, &mut i)?),
            "--include" => parsed.include.push(option_value(args, &mut i)?),
            "--artifact" => parsed.artifacts.push(option_value(args, &mut i)?),
            other if other.starts_with('-') => bail!("unknown option {other}"),
            _ => {
                parsed.command = args[i..].to_vec();
                break;
            }
        }
        i += 1;
    }
    if parsed.command.is_empty() {
        bail!("no command given");
    }
    Ok(parsed)
}

pub fn run_cli(args: &[String]) -> i32 {
    if args.iter().take_while(|a| *a != "--").any(|a| a == "-h" || a == "--help") {
        println!("{RUN_USAGE}");
        return 0;
    }
    match parse_run(args).and_then(|parsed| run(&parsed)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("cepheus run: {err:#}");
            1
        }
    }
}

/// The config's `offload` settings. A config that does not load fails the
/// command rather than building the generic environment in its place.
pub fn offload_config() -> Result<OffloadConfig> {
    let (cfg, err) = crate::config::Config::load();
    if let Some(err) = err {
        bail!("config: {err}");
    }
    Ok(cfg.offload)
}

fn run(args: &RunArgs) -> Result<i32> {
    let tree = Worktree::current()?;
    let cfg = offload_config()?;
    let python = if args.python { Some(PythonLock::read(&tree.root, &cfg)?) } else { None };
    let chosen = hosts::choose(&args.host, &hosts::configured())?;
    let (host, load) = hosts::pick(hosts::ranked(&chosen, "run")?, args.host != "auto")?;
    eprintln!("[run] {}", hosts::describe(&host, &load));
    let files = tree.files(&args.include)?;
    let mirror = tree.mirror();
    let lease = lease::Lease::start(&host, &mirror, &cfg.container)?;
    sync::sync(&lease, &tree, &files)?;
    if let Some(python) = &python {
        sync::python_env(&lease, python)?;
    }
    let remote = RemoteRun {
        host: &host,
        mirror: &mirror,
        cwd: &tree.cwd,
        python: if args.python { Some(cfg.wrap.as_slice()) } else { None },
        slots: Slots::All,
        environment: &args.environment,
        argv: &args.command,
    };
    let status = remote.status(Stdio::inherit(), Stdio::inherit())?;
    let mut code = status.code().unwrap_or(1);
    for artifact in &args.artifacts {
        if let Err(err) = fetch(&lease, &tree, artifact) {
            eprintln!("cepheus run: {err:#}");
            if code == 0 {
                code = 1;
            }
        }
    }
    lease.finish()?;
    Ok(code)
}

/// Copy `artifact` (relative to the command's directory) back to the same
/// place here.
fn fetch(lease: &lease::Lease, tree: &Worktree, artifact: &str) -> Result<()> {
    let host = &lease.host;
    let relative = tree.relative(artifact)?;
    let parent = std::path::Path::new(&relative).parent().context("artifact parent")?;
    let mut destination = tree.root.clone();
    for component in parent.components() {
        destination.push(component.as_os_str());
        if let Err(err) = std::fs::create_dir(&destination) {
            if err.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(err).context("creating artifact destination");
            }
        }
        destination = destination.canonicalize().context("resolving artifact destination")?;
        anyhow::ensure!(
            destination.starts_with(&tree.root) && destination.is_dir(),
            "{artifact} has a destination outside the worktree or not a directory"
        );
    }
    // Podman resolves absolute source symlinks in the container's filesystem.
    // Stream its archive without requiring rsync inside the tools image or
    // retaining an intermediate artifact on the build host.
    let source = format!("{}:{}/{relative}", lease.name, lease::SOURCE);
    let mut copy = ssh_command(host)
        .arg(remote_command("exec podman --remote=false cp \"$1\" -", &[&source]))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("starting container artifact copy")?;
    let extracted = Command::new("tar")
        .args(["--extract", "--file=-", "--no-same-owner", "--directory"])
        .arg(&destination)
        .stdin(copy.stdout.take().expect("piped artifact archive"))
        .status();
    let extracted = match extracted {
        Ok(status) => status,
        Err(err) => {
            let _ = copy.kill();
            let _ = copy.wait();
            return Err(err).context("starting artifact extraction");
        }
    };
    let copied = copy.wait().context("waiting for container artifact copy")?;
    if !copied.success() || !extracted.success() {
        bail!("copying {artifact} back from {} failed (copy: {copied}, extract: {extracted})", host.name);
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/offload.rs"]
mod tests;
