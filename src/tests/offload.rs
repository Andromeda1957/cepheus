use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::*;
use crate::testsupport::TempDir;

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

#[test]
fn run_takes_its_options_then_the_command() {
    let parsed = parse_run(&argv(&[
        "--host",
        "builder",
        "--python",
        "--env",
        "A=1",
        "--include",
        "node_modules",
        "--artifact",
        "out.txt",
        "--",
        "make",
        "--host",
    ]))
    .unwrap();
    assert_eq!(
        parsed,
        RunArgs {
            host: "builder".into(),
            python: true,
            environment: argv(&["A=1"]),
            include: argv(&["node_modules"]),
            artifacts: argv(&["out.txt"]),
            command: argv(&["make", "--host"]),
        }
    );
}

#[test]
fn run_defaults_to_auto_and_a_bare_command_needs_no_separator() {
    let parsed = parse_run(&argv(&["cargo", "test", "--release"])).unwrap();
    assert_eq!(parsed.host, "auto");
    assert_eq!(parsed.command, argv(&["cargo", "test", "--release"]));
}

#[test]
fn run_refuses_unknown_options_bad_assignments_and_no_command() {
    assert!(parse_run(&argv(&["--hots", "x", "--", "ls"])).is_err());
    assert!(parse_run(&argv(&["--env", "1A=b", "--", "ls"])).is_err());
    assert!(parse_run(&argv(&["--env", "NOEQUALS", "--", "ls"])).is_err());
    assert!(parse_run(&argv(&["--host"])).is_err());
    assert!(parse_run(&argv(&["--python", "--"])).is_err());
}

#[test]
fn assignments_need_an_exportable_name() {
    assert!(valid_assignment("A=1"));
    assert!(valid_assignment("_A_2="));
    assert!(valid_assignment("PATH=a=b"));
    assert!(!valid_assignment("=x"));
    assert!(!valid_assignment("2A=x"));
    assert!(!valid_assignment("A-B=x"));
    assert!(!valid_assignment("A"));
}

#[test]
fn container_lease_binds_only_a_canonical_public_android_sdk_read_only() {
    let home = TempDir::new("offload-sdk-binding");
    let sdk = home.path().join("Android/Sdk");
    for name in ["platforms", "build-tools", "licenses"] {
        std::fs::create_dir_all(sdk.join(name)).unwrap();
    }
    home.executable(
        "bin/podman",
        r#"#!/bin/bash
shift
case "$1 $2" in
  'container exists') exit 1;;
  'image exists') exit 0;;
  'run --detach') printf '%s\0' "$@" > "$HOME/run-args"; exit 125;;
  *) exit 125;;
esac
"#,
    );
    for candidate in [sdk.clone(), home.path().join("missing"), sdk.join("../Sdk")] {
        let output = Command::new("bash")
            .args([
                "-c",
                lease::START,
                "offload",
                "cepheus-offload-00000000000000000000000000000000",
                "tools",
                candidate.to_str().unwrap(),
            ])
            .env("HOME", home.path())
            .env("PATH", format!("{}:{}", home.path().join("bin").display(), std::env::var("PATH").unwrap()))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(125));
        let argv = home.path().join("run-args");
        if candidate == sdk {
            let bytes = std::fs::read(&argv).unwrap();
            let words: Vec<_> = bytes.split(|byte| *byte == 0).map(|word| String::from_utf8_lossy(word)).collect();
            let mount = format!("type=bind,src={},dst=/opt/android-sdk,ro,relabel=shared", sdk.display());
            assert!(words.iter().any(|word| word.as_ref() == mount.as_str()));
            assert!(words.iter().any(|word| word.as_ref() == "ANDROID_HOME=/opt/android-sdk"));
            std::fs::remove_file(argv).unwrap();
        } else {
            assert!(!argv.exists(), "invalid SDK must fail before starting a container");
        }
    }
}

#[test]
fn quoting_keeps_every_word_literal() {
    assert_eq!(quote("plain"), "'plain'");
    assert_eq!(quote("it's $HOME"), "'it'\\''s $HOME'");
    assert_eq!(remote_command("echo \"$1\"", &["a b", "c'd"]), "bash -c 'echo \"$1\"' cepheus-offload 'a b' 'c'\\''d'");
}

#[test]
fn container_lease_preserves_existing_names_and_reports_engine_errors() {
    for failure in ["existing", "engine", "inspect"] {
        let home = TempDir::new("offload-lease-errors");
        home.executable(
            "bin/podman",
            r#"#!/bin/bash
shift
case "$1 $2" in
  'container exists')
    [ "$FAILURE" = existing ] && exit 0
    [ "$FAILURE" = engine ] && exit 125
    [ -f "$HOME/created" ] && exit 0
    exit 1;;
  'image exists') exit 0;;
  'run --detach') touch "$HOME/created"; printf '%064d\n' 0; exit 0;;
  'exec '*) exit 1;;
  'container inspect') exit 125;;
  *) touch "$HOME/unexpected-removal"; exit 126;;
esac
"#,
        );
        let output = Command::new("bash")
            .args(["-c", lease::START, "offload", "cepheus-offload-00000000000000000000000000000000", "tools", ""])
            .env("HOME", home.path())
            .env("FAILURE", failure)
            .env("PATH", format!("{}:{}", home.path().join("bin").display(), std::env::var("PATH").unwrap()))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(125), "{failure}: {output:?}");
        assert!(!home.path().join("unexpected-removal").exists(), "{failure}");
    }
}

#[test]
fn a_remote_command_names_its_mirror_slots_environment_wrap_and_argv() {
    let host = hosts::BuildHost { name: "h".into(), ssh: "user@h".into() };
    let environment = argv(&["A=1"]);
    let wrap = argv(&["wrap", "--registry", "@STATE@/scratch", "--"]);
    let command = argv(&["python", "-m", "unittest", "x y"]);
    let run = RemoteRun {
        host: &host,
        mirror: ".cache/m",
        cwd: "sub",
        python: Some(&wrap),
        slots: Slots::One,
        environment: &environment,
        argv: &command,
    };
    let ssh = run.command();
    let args: Vec<String> = ssh.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(ssh.get_program(), "ssh");
    assert_eq!(args[args.len() - 2], "user@h");
    let line = args.last().unwrap();
    assert!(
        line.ends_with(
            "cepheus-offload '.cache/m' 'sub' '1' 'one' 'A=1' '--' 'wrap' '--registry' '@STATE@/scratch' '--' \
             'python' '-m' 'unittest' 'x y'"
        ),
        "{line}"
    );
}

/// `RUN` as SSH would start it on a host whose home is `home`.
fn start_run(home: &TempDir, slots: &str, command: &[&str]) -> Child {
    home.write("m/.cepheus-offload/keep", "");
    home.executable("bin/podman", FAKE_EXEC);
    let path = format!("{}:{}", home.path().join("bin").display(), std::env::var("PATH").unwrap());
    Command::new("bash")
        .args(["-c", RUN, "cepheus-offload", "m", "", "0", slots, "GREETING=hi @STATE@", "--"])
        .args(command)
        .env("HOME", home.path())
        .env("PATH", path)
        .env_remove("TMPDIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

// The shell-level checks exercise host slot admission, argv and cancellation;
// actual namespaces and storage retirement are verified with real Podman.
pub(crate) const FAKE_EXEC: &str = r#"#!/bin/bash
[ "$1" = --remote=false ] && shift
[ "$1" = exec ] || exit 125
shift
while [ "$#" -gt 0 ]; do
  case "$1" in
    --workdir) shift 2;;
    --env) export "$2"; shift 2;;
    --interactive) shift;;
    *) shift; break;;
  esac
done
cd "$HOME/m"
exec "$@"
"#;

/// A FIFO a command blocks reading until `release` closes its writing end.
fn gate(home: &TempDir) -> PathBuf {
    let path = home.path().join("gate");
    let name = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    path
}

fn release(gate: &Path) {
    drop(std::fs::OpenOptions::new().write(true).open(gate).unwrap());
}

fn first_line(child: &mut Child) -> (String, BufReader<std::process::ChildStdout>) {
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    (line.trim().to_string(), reader)
}

/// Slots no process holds now, counted the way the probe counts them.
fn free_slots(home: &TempDir) -> (usize, usize) {
    let script = concat!(
        slots_sh!(),
        r#"free=0
for ((i = 0; i < cap; i++)); do flock -n -x "$slot_dir/$i" true && free=$((free + 1)); done
echo "$cap $free""#
    );
    let out = Command::new("bash").args(["-c", script]).env("HOME", home.path()).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let mut words = text.split_whitespace().map(|w| w.parse::<usize>().unwrap());
    (words.next().unwrap(), words.next().unwrap())
}

#[test]
fn a_build_holds_the_hosts_whole_share_while_it_runs_and_gives_it_back() {
    let home = TempDir::new("offload-run-slots");
    let (cap, free) = free_slots(&home);
    assert_eq!(cap, (std::thread::available_parallelism().unwrap().get() / 2).max(1));
    assert_eq!(free, cap);
    let gate = gate(&home);
    let gate_arg = gate.to_string_lossy().into_owned();
    let mut child = start_run(
        &home,
        "all",
        &["sh", "-c", "echo \"$CARGO_BUILD_JOBS $GREETING $1\"; cat \"$0\" >/dev/null", &gate_arg, "@STATE@/x"],
    );
    let stdin = child.stdin.take();
    let (line, _) = first_line(&mut child);
    let state = lease::STATE;
    assert_eq!(line, format!("{cap} hi {state} {state}/x"));
    assert_eq!(free_slots(&home), (cap, 0));
    release(&gate);
    assert!(child.wait().unwrap().success());
    drop(stdin);
    assert_eq!(free_slots(&home), (cap, cap));
}

#[test]
fn a_shard_holds_one_slot_and_its_exit_status_comes_back() {
    let home = TempDir::new("offload-run-status");
    let (cap, _) = free_slots(&home);
    let gate = gate(&home);
    let gate_arg = gate.to_string_lossy().into_owned();
    let mut child =
        start_run(&home, "one", &["sh", "-c", "echo \"$CARGO_BUILD_JOBS\"; cat \"$0\" >/dev/null; exit 3", &gate_arg]);
    let stdin = child.stdin.take();
    let (line, _) = first_line(&mut child);
    assert_eq!(line, "1");
    assert_eq!(free_slots(&home), (cap, cap - 1));
    release(&gate);
    assert_eq!(child.wait().unwrap().code(), Some(3));
    drop(stdin);
    assert_eq!(free_slots(&home), (cap, cap));
}

#[test]
fn a_caller_that_goes_away_ends_the_command_and_frees_its_slots() {
    let home = TempDir::new("offload-run-gone");
    let (cap, _) = free_slots(&home);
    let mut child = start_run(&home, "all", &["sh", "-c", "echo started; sleep 60"]);
    let (line, _) = first_line(&mut child);
    assert_eq!(line, "started");
    let began = Instant::now();
    drop(child.stdin.take());
    assert!(!child.wait().unwrap().success());
    assert!(began.elapsed() < Duration::from_secs(9), "{:?}", began.elapsed());
    assert_eq!(free_slots(&home), (cap, cap));
}

#[test]
fn a_command_waits_while_other_offloaded_work_holds_the_slots() {
    let home = TempDir::new("offload-run-wait");
    let (cap, _) = free_slots(&home);
    let mut holder = start_run(&home, "all", &["sh", "-c", "echo held; sleep 60"]);
    let holder_stdin = holder.stdin.take();
    assert_eq!(first_line(&mut holder).0, "held");

    let mut waiter = start_run(&home, "one", &["echo", "ran"]);
    let waiter_stdin = waiter.stdin.take();
    let mut said = [0u8; 9];
    waiter.stderr.take().unwrap().read_exact(&mut said).unwrap();
    assert_eq!(&said, b"[offload]");
    assert!(waiter.try_wait().unwrap().is_none());

    // The holder's caller goes away; the waiter then runs.
    drop(holder_stdin);
    holder.wait().unwrap();
    assert_eq!(first_line(&mut waiter).0, "ran");
    assert!(waiter.wait().unwrap().success());
    drop(waiter_stdin);
    assert_eq!(free_slots(&home), (cap, cap));
}
