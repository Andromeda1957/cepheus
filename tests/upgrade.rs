//! The service updates itself in place when its binary is replaced, keeping
//! every running session. Owner, 2026-10-26: "i have agents running to the
//! older service, hard to stop and start it, cant lose whats running" and
//! "make it so the service just auto updates in place".
//!
//! This runs the built binary as a real service in a private directory:
//! its own socket, state, config and stand-in Claude Code. Nothing
//! touches the owner's service, sessions or port 8000.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BINARY: &str = env!("CARGO_BIN_EXE_cepheus");

/// A private directory removed when the test ends, pass or fail.
struct Scratch(PathBuf);

impl Scratch {
    fn new(test: &str) -> Scratch {
        let path = std::env::temp_dir().join(format!("cepheus-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A service started by the test, stopped (and killed if need be) when the
/// test ends.
struct Service {
    child: Child,
    dir: PathBuf,
}

impl Drop for Service {
    fn drop(&mut self) {
        // The built binary, not the installed copy: the test leaves a
        // refusing script installed.
        if let Ok(state) =
            serde_json::from_slice::<serde_json::Value>(&run(Path::new(BINARY), &self.dir, &["ctl", "status"]).stdout)
        {
            for session in state["sessions"].as_array().into_iter().flatten() {
                if let Some(id) = session["id"].as_u64() {
                    let _ = run(Path::new(BINARY), &self.dir, &["ctl", "close", &id.to_string()]);
                }
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let Ok(state) = serde_json::from_slice::<serde_json::Value>(
                    &run(Path::new(BINARY), &self.dir, &["ctl", "status"]).stdout,
                ) else {
                    break;
                };
                if state["sessions"].as_array().into_iter().flatten().all(|session| session["alive"] != true) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = run(Path::new(BINARY), &self.dir, &["stop", "--yes"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn env(cmd: &mut Command, dir: &Path) {
    cmd.env("CEPHEUS_SOCKET", dir.join("run/deck.sock"))
        .env("CEPHEUS_STATE_DIR", dir.join("state"))
        .env("CEPHEUS_CONFIG", dir.join("config.json"))
        .env("CLAUDE_CONFIG_DIR", dir.join("claude"))
        .env("CODEX_HOME", dir.join("codex"))
        .env("CEPHEUS_ANTIGRAVITY_DIR", dir.join("agy"))
        .env("CEPHEUS_TEMP_DIR", dir.join("clients"))
        .env("CEPHEUS_START_DIR", dir);
    // A test run from an agent's shell still starts its own private service
    // there, not one in the owner's session (`owner_env.rs`).
    for marker in ["CEPHEUS_SESSION", "AGENT_DECK_SESSION", "CLAUDECODE", "AI_AGENT", "CODEX_THREAD_ID", "CODEX_CI"] {
        // legacy-name
        cmd.env_remove(marker);
    }
}

fn run(program: &Path, dir: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    env(&mut cmd, dir);
    cmd.output().expect("run cepheus")
}

/// A control command, from the built binary so it never races the test's
/// installs.
fn ctl(dir: &Path, args: &[&str]) -> String {
    let mut all = vec!["ctl"];
    all.extend_from_slice(args);
    let out = run(Path::new(BINARY), dir, &all);
    assert!(out.status.success(), "ctl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

/// Put a new file at the binary's path as `cepheus install` does: a
/// rename, so the running service's file is replaced, not rewritten.
fn install(dir: &Path, from: &Path) {
    let tmp = dir.join("bin/.cepheus.install.tmp");
    fs::copy(from, &tmp).unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&tmp, dir.join("bin/cepheus")).unwrap();
}

fn exe_of(pid: u32) -> String {
    fs::read_link(format!("/proc/{pid}/exe")).map(|p| p.display().to_string()).unwrap_or_default()
}

fn status(dir: &Path) -> serde_json::Value {
    serde_json::from_str(&ctl(dir, &["status"])).expect("status is JSON")
}

/// A stand-in Claude Code that takes owner messages as the deck's
/// `cepheus-owner-input` plugin has Claude take them: it runs the owner-input
/// listener, shows each message it is handed as `got:<text>`, and reports
/// that it submitted it through the program the deck names.
const CLAUDE: &str = r#"
import json, os, subprocess, sys
args = sys.argv[1:]
conversation = args[args.index("--session-id") + 1]
print("ready", flush=True)
program = os.environ["CEPHEUS_OWNER_INPUT"]
listen = [program, "hook", "owner-input", "listen", conversation]
listener = subprocess.Popen(listen, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE)
for line in listener.stdout:
    handed = json.loads(line)
    print("got:" + handed["text"], flush=True)
    receipt = json.dumps({"message": handed["message"], "outcome": "submitted"}).encode()
    subprocess.run([program, "hook", "owner-input", "receipt"], input=receipt)
"#;

/// Install the built binary in `dir` and start it as a service there, with
/// the stand-in above for Claude Code and `sh` for Codex.
fn start(dir: &Path) -> Service {
    start_script_config(dir, r#"echo ready; while read line; do echo "got:$line"; done"#, |config| {
        config["claude_command"] = serde_json::json!(["/usr/bin/python3", "-c", CLAUDE]);
    })
}
fn start_script_config(dir: &Path, script: &str, configure: impl FnOnce(&mut serde_json::Value)) -> Service {
    start_script_config_env(dir, script, configure, |_| {})
}
fn start_script_config_env(
    dir: &Path,
    script: &str,
    configure: impl FnOnce(&mut serde_json::Value),
    configure_env: impl FnOnce(&mut Command),
) -> Service {
    fs::create_dir_all(dir.join("bin")).unwrap();
    install(dir, Path::new(BINARY));
    let probe = dir.join("probe");
    fs::create_dir_all(probe.join("pressure")).unwrap();
    fs::write(probe.join("meminfo"), "MemTotal: 16777216 kB\nMemAvailable: 12582912 kB\n").unwrap();
    fs::write(probe.join("pressure/memory"), "full avg10=0\n").unwrap();
    fs::write(probe.join("pressure/cpu"), "some avg60=0\n").unwrap();
    fs::write(probe.join("loadavg"), "0 0 0 1/1 1\n").unwrap();
    let mut config = serde_json::json!({
        "claude_command": ["sh", "-c", script],
        "codex_command": ["sh", "-c", script],
        "scheduler":{"probe":{"root":probe},"pools":{"heavy":2}},
        "notifications": false,
        "claude_hooks": false,
        "codex_notify": false,
        "palette_file": dir.join("no-kitty.conf"),
        // Never the owner's runserver on port 8000.
        "dev_server_command": ["sh", "-c", "exec sleep 60"],
    });
    configure(&mut config);
    fs::write(dir.join("config.json"), config.to_string()).unwrap();

    let log = fs::File::create(dir.join("service.log")).unwrap();
    let mut cmd = Command::new(dir.join("bin/cepheus"));
    cmd.arg("service").stdin(Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log);
    env(&mut cmd, dir);
    configure_env(&mut cmd);
    let service = Service { child: cmd.spawn().expect("start the service"), dir: dir.to_path_buf() };
    wait_until("the service socket", || dir.join("run/deck.sock").exists());
    service
}

/// A new session that has answered one line; its id and its program's pid.
fn session_with_a_reply(dir: &Path) -> (String, u64) {
    let id = ctl(dir, &["new", "claude"]).trim().to_string();
    wait_until("the session's first output", || ctl(dir, &["screen", &id]).contains("ready"));
    ctl(dir, &["send", &id, "before"]);
    wait_until("the reply before the update", || ctl(dir, &["screen", &id]).contains("got:before"));
    let session_pid = status(dir)["sessions"][0]["pid"].as_u64().expect("the session's pid");
    (id, session_pid)
}

#[test]
fn a_replaced_binary_takes_over_the_running_service_and_its_sessions() {
    let scratch = Scratch::new("upgrade");
    let dir = scratch.0.as_path();
    let service = start(dir);
    let pid = service.child.id();
    let (id, session_pid) = session_with_a_reply(dir);
    let hold = serde_json::json!({"pid":session_pid,"session":id.parse::<u64>().unwrap(),"key":"upgrade-holder"});
    let held: serde_json::Value = serde_json::from_str(&ctl(dir, &["fleet", "slot-hold", &hold.to_string()])).unwrap();
    let slot = held["id"].as_u64().unwrap();
    wait_until("the slot grant", || {
        let snapshot: serde_json::Value = serde_json::from_str(&ctl(dir, &["fleet", "status"])).unwrap();
        snapshot["holders"].as_array().unwrap().iter().any(|s| s["id"] == slot)
    });
    let queued: serde_json::Value = serde_json::from_str(&ctl(
        dir,
        &["fleet", "enqueue", r#"{"provider":"codex","placement":"missing-peer","title":"upgrade-held"}"#],
    ))
    .unwrap();

    // Install a new build: the service runs it in its own process.
    install(dir, Path::new(BINARY));
    assert!(exe_of(pid).ends_with(" (deleted)"), "{}", exe_of(pid));
    wait_until("the service to run the new binary", || {
        let exe = exe_of(pid);
        !exe.is_empty() && !exe.ends_with(" (deleted)")
    });
    assert!(service_alive(pid), "the same process carries on");
    let snapshot: serde_json::Value = serde_json::from_str(&ctl(dir, &["fleet", "status"])).unwrap();
    assert!(
        snapshot["holders"].as_array().unwrap().iter().any(|s| s["id"] == slot),
        "live slot survives exec handover: {snapshot}"
    );
    assert!(
        snapshot["starts"].as_array().unwrap().iter().any(|s| s["id"] == queued["id"]),
        "queued start survives handover: {snapshot}"
    );
    ctl(dir, &["fleet", "slot-release", &serde_json::json!({"id":slot}).to_string()]);
    ctl(dir, &["fleet", "cancel", &queued.to_string()]);

    let screen = ctl(dir, &["screen", &id]);
    assert!(screen.contains("ready") && screen.contains("got:before"), "the screen came along:\n{screen}");
    let state = status(dir);
    let session = &state["sessions"][0];
    assert_eq!(session["alive"], true, "{state}");
    assert_eq!(session["pid"].as_u64(), Some(session_pid), "the same program, not a restarted one");
    ctl(dir, &["send", &id, "after"]);
    wait_until("the reply after the update", || ctl(dir, &["screen", &id]).contains("got:after"));

    // A binary that cannot take over is refused, and the service keeps going.
    let refusing = dir.join("refusing");
    fs::write(&refusing, "#!/bin/sh\necho 'cannot take over' >&2\nexit 1\n").unwrap();
    install(dir, &refusing);
    wait_until("the refused update in the error list", || {
        status(dir)["errors"].as_array().is_some_and(|e| {
            e.iter().any(|e| e["text"].as_str().is_some_and(|t| t.contains("could not update itself")))
        })
    });
    assert!(exe_of(pid).ends_with(" (deleted)"), "still the previous build");
    // The session still takes the owner's message. Claude's plugin reports
    // what Claude did with it through the installed program, which now
    // refuses, so the send is not confirmed and says so.
    let still = run(Path::new(BINARY), dir, &["ctl", "send", &id, "still"]);
    let said = String::from_utf8_lossy(&still.stderr);
    assert!(!still.status.success() && said.contains("did not confirm"), "{said}");
    assert!(ctl(dir, &["screen", &id]).contains("got:still"), "the reply after the refused update");
    assert_eq!(status(dir)["sessions"][0]["pid"].as_u64(), Some(session_pid));
    drop(service);
}

#[test]
fn a_restart_runs_the_service_again_and_every_session_carries_on() {
    // Owner, 2026-10-26: "in the desk there should be a key that restarts
    // the service , and reopens all sessions as they were before the
    // restart". The deck's key and `cepheus restart` ask for it.
    let scratch = Scratch::new("restart");
    let dir = scratch.0.as_path();
    let service = start(dir);
    let pid = service.child.id();
    let (id, session_pid) = session_with_a_reply(dir);
    assert!(!command_line(pid).contains("--handover"), "{}", command_line(pid));

    let out = run(Path::new(BINARY), dir, &["restart"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{said}{}", String::from_utf8_lossy(&out.stderr));
    assert!(said.contains("service restarted"), "{said}");
    assert!(command_line(pid).contains("--handover"), "the program started again in the same process");
    let state = status(dir);
    assert_eq!(state["sessions"][0]["alive"], true, "{state}");
    assert_eq!(state["sessions"][0]["pid"].as_u64(), Some(session_pid), "the same program, not a reopened one");
    assert!(ctl(dir, &["screen", &id]).contains("got:before"), "its screen came along");
    ctl(dir, &["send", &id, "after"]);
    wait_until("the reply after the restart", || ctl(dir, &["screen", &id]).contains("got:after"));
    drop(service);
}

#[test]
fn restart_sends_an_older_service_to_the_deck_key_and_asks_it_nothing() {
    // The owner's running service is protocol 1. It answers a newer
    // client's hello with an error notice, then its welcome and state, as a
    // fake service does here. `cepheus restart` stopped at that notice
    // and never named the deck's key that can restart such a service.
    let scratch = Scratch::new("older-restart");
    let dir = scratch.0.as_path();
    fs::create_dir_all(dir.join("run")).unwrap();
    let listener = UnixListener::bind(dir.join("run/deck.sock")).unwrap();
    let older = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let hello = read_frame(&mut stream);
        let replies = [
            r#"{"t":"notice","text":"protocol 3 differs from the service's 1; restart the service","error":true}"#,
            r#"{"t":"welcome","version":1,"pid":4242,"start_dir":"/srv"}"#,
            r#"{"t":"state","sessions":[],"outside":[],"usage":{"claude":[],"codex":[]},"start_dir":"/srv"}"#,
        ];
        for reply in replies {
            stream.write_all(&(reply.len() as u32).to_be_bytes()).unwrap();
            stream.write_all(reply.as_bytes()).unwrap();
        }
        // Everything else the command sends, until it hangs up.
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
        (hello, String::from_utf8_lossy(&rest).into_owned())
    });

    let out = run(Path::new(BINARY), dir, &["restart"]);
    let said = String::from_utf8_lossy(&out.stderr);
    let (hello, rest) = older.join().unwrap();
    assert!(hello.contains(r#""t":"hello""#), "{hello}");
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("older build (protocol 1)") && said.contains(":restart"), "{said}");
    assert!(!rest.contains("restart") && !rest.contains("shutdown"), "it asked the older service: {rest}");
}

/// One length-prefixed message from the stream, as text.
fn read_frame(stream: &mut UnixStream) -> String {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut body).unwrap();
    String::from_utf8_lossy(&body).into_owned()
}

/// The command line a process runs, its arguments separated by spaces.
fn command_line(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline")).map(|b| String::from_utf8_lossy(&b).replace('\0', " ")).unwrap_or_default()
}

fn service_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
fn a_hundred_real_hosted_agents_report_status_hooks_across_two_services() {
    let first = Scratch::new("fleet100-a");
    let second = Scratch::new("fleet100-b");
    let script = format!(
        "printf '\\033[?2004hready\\n'; \"{BINARY}\" hook codex '{{\"type\":\"agent-turn-complete\"}}'; exec sleep 600"
    );
    // Both hosts are private logical hosts on this machine. Exercise the real
    // authenticated TLS federation, not a sum of independent service states.
    let port = loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        if port != 8000 {
            break port;
        }
    };
    let fingerprint = run(Path::new(BINARY), &second.0, &["peer", "fingerprint"]);
    assert!(fingerprint.status.success());
    let fingerprint = String::from_utf8(fingerprint.stdout).unwrap().trim().to_string();
    let key = "af".repeat(32);
    fs::create_dir_all(first.0.join("state/hub")).unwrap();
    fs::write(first.0.join("state/hub/key"), &key).unwrap();
    fs::set_permissions(first.0.join("state/hub/key"), fs::Permissions::from_mode(0o600)).unwrap();
    let mut authorize = Command::new(BINARY);
    authorize.args(["peer", "authorize"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    env(&mut authorize, &second.0);
    let mut authorize = authorize.spawn().unwrap();
    authorize.stdin.take().unwrap().write_all(key.as_bytes()).unwrap();
    assert!(authorize.wait_with_output().unwrap().status.success());
    let began = Instant::now();
    let b = start_script_config(&second.0, &script, |config| {
        config["peer_listen"] = serde_json::json!(format!("127.0.0.1:{port}"));
        config["peer_allow"] = serde_json::json!(["127.0.0.1/32"]);
        config["sync_logins"] = serde_json::json!(false);
    });
    let a = start_script_config(&first.0, &script, |config| {
        config["sync_logins"] = serde_json::json!(false);
        config["hosts"] = serde_json::json!([{
            "name":"worker", "tag":1, "addresses":[format!("127.0.0.1:{port}")], "fingerprint":fingerprint
        }]);
    });
    wait_until("the authenticated peer's merged state", || status(&a.dir)["host_schedulers"]["worker"].is_object());
    // Model capacity for fifty recent starts per logical host, plus reserves.
    for dir in [&a.dir, &b.dir] {
        fs::write(dir.join("probe/meminfo"), "MemTotal: 67108864 kB\nMemAvailable: 50331648 kB\n").unwrap();
    }
    for _ in 0..50 {
        ctl(&a.dir, &["new", "codex"]);
        ctl(&a.dir, &["new", "codex", "--host", "worker"]);
    }
    wait_until("all 100 live hook statuses in the hub's federated state", || {
        let state = status(&a.dir);
        let sessions = state["sessions"].as_array().unwrap();
        sessions.len() == 100 && sessions.iter().all(|s| s["status"] == "finished" && s["alive"] == true)
    });
    let merged = status(&a.dir);
    let sessions = merged["sessions"].as_array().unwrap();
    let ids: std::collections::HashSet<_> = sessions.iter().map(|s| s["id"].as_u64().unwrap()).collect();
    assert_eq!(ids.len(), 100, "peer-local IDs must never collide with hub IDs");
    let remote: Vec<_> = sessions.iter().filter(|s| s["host"] == "worker").collect();
    assert_eq!(remote.len(), 50);
    assert!(remote.iter().all(|s| s["id"].as_u64().unwrap() >> 40 == 1));
    assert_eq!(status(&b.dir)["sessions"].as_array().unwrap().len(), 50);
    let bytes = serde_json::to_vec(&merged).unwrap().len();
    eprintln!("fleet100-real-federated: startup_ms={} merged_state_bytes={bytes}", began.elapsed().as_millis());
    let began = Instant::now();
    for _ in 0..20 {
        assert_eq!(status(&a.dir)["sessions"].as_array().unwrap().len(), 100);
    }
    eprintln!("fleet100-real-federated: twenty_merged_status_reads_ms={}", began.elapsed().as_millis());
}

/// Prints what decides a hosted program's colours and Claude's sign-in, one
/// value a line, then waits.
const ENV_REPORT: &str = r#"for v in NO_COLOR TERM COLORTERM LANG PAGER CODEX_CI CLAUDECODE; do eval "echo env:$v=\${$v-unset}"; done; if test -f "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.credentials.json"; then echo login:readable; else echo login:missing; fi; echo env:done; while read line; do :; done"#;

/// Every provider, Codex resumed and the deck's own terminal, with their
/// screens once each has reported.
fn hosted_reports(dir: &Path) -> Vec<(String, String)> {
    let mut started = Vec::new();
    for tool in ["claude", "agy", "grok", "opencode"] {
        started.push((tool.to_string(), ctl(dir, &["new", tool]).trim().to_string()));
    }
    let codex = ctl(dir, &["resume", "codex", "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"]).trim().to_string();
    started.push(("codex".into(), codex));
    started.push(("terminal".into(), ctl(dir, &["term", "--", "sh", "-c", ENV_REPORT]).trim().to_string()));
    started
        .into_iter()
        .map(|(tool, id)| {
            wait_until(&format!("{tool}'s report"), || ctl(dir, &["screen", &id]).contains("env:done"));
            (tool, ctl(dir, &["screen", &id]))
        })
        .collect()
}

fn fake_busctl(dir: &Path, body: &str) -> PathBuf {
    let bin = dir.join("session-bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("busctl"), format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(bin.join("busctl"), fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn every_provider(config: &mut serde_json::Value) {
    for key in ["claude_command", "codex_command", "antigravity_command", "grok_command", "opencode_command"] {
        config[key] = serde_json::json!(["sh", "-c", ENV_REPORT]);
    }
}

#[test]
fn a_service_started_in_an_agent_shell_hosts_every_provider_in_colour_and_signed_in() {
    // 2026-11-07: Codex started the owner's service from its shell, and every
    // hosted program inherited Codex's command environment: NO_COLOR, a dumb
    // terminal and the CLAUDE_CONFIG_DIR that keeps Codex from running Claude.
    let scratch = Scratch::new("agent-shell-start");
    let dir = scratch.0.as_path();
    let home = dir.join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(home.join(".claude/.credentials.json"), "fixture-owner-login").unwrap();
    let session = serde_json::json!({"type": "as", "data": [
        format!("HOME={}", home.display()), "PATH=/usr/bin:/bin", "LANG=en_US.UTF-8",
    ]});
    let bin = fake_busctl(dir, &format!("printf '%s\\n' '{session}'"));
    let _service = start_script_config_env(dir, ENV_REPORT, every_provider, |command| {
        command
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", dir.join("agent-home"))
            .env("CODEX_THREAD_ID", "agent-thread")
            .env("CODEX_CI", "1")
            .env("CLAUDECODE", "1")
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .env("COLORTERM", "")
            .env("LANG", "C.UTF-8")
            .env("PAGER", "cat")
            .env("CLAUDE_CONFIG_DIR", dir.join("agent-guard"));
    });
    for (tool, screen) in hosted_reports(dir) {
        for line in [
            "env:NO_COLOR=unset",
            "env:TERM=xterm-256color",
            "env:COLORTERM=truecolor",
            "env:LANG=en_US.UTF-8",
            "env:PAGER=unset",
            "env:CODEX_CI=unset",
            "env:CLAUDECODE=unset",
            "login:readable",
        ] {
            assert!(screen.contains(line), "{tool} lacks {line}:\n{screen}");
        }
    }
}

#[test]
fn a_service_started_by_the_owner_keeps_the_owners_environment() {
    let scratch = Scratch::new("owner-start");
    let dir = scratch.0.as_path();
    let custom = dir.join("custom-claude");
    fs::create_dir_all(&custom).unwrap();
    fs::write(custom.join(".credentials.json"), "fixture-owner-login").unwrap();
    // The user manager is not consulted for the owner's own start.
    let bin = fake_busctl(dir, "exit 1");
    let _service = start_script_config_env(dir, ENV_REPORT, every_provider, |command| {
        command
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", dir.join("home"))
            .env("CLAUDE_CONFIG_DIR", &custom)
            .env("TERM", "xterm-kitty")
            .env("KITTY_WINDOW_ID", "3")
            .env("NO_COLOR", "1");
    });
    for (tool, screen) in hosted_reports(dir) {
        // The owner's own choices pass through unchanged; only the terminal
        // is the deck's.
        for line in ["env:NO_COLOR=1", "env:TERM=xterm-256color", "env:COLORTERM=truecolor", "login:readable"] {
            assert!(screen.contains(line), "{tool} lacks {line}:\n{screen}");
        }
    }
}

#[test]
fn an_agent_shell_service_without_the_owners_session_refuses_to_start_or_take_over() {
    let scratch = Scratch::new("agent-shell-refused");
    let dir = scratch.0.as_path();
    fs::create_dir_all(dir.join("bin")).unwrap();
    install(dir, Path::new(BINARY));
    let bin = fake_busctl(dir, "echo 'Failed to connect to bus' >&2; exit 1");
    for args in [&["service"][..], &["service", "--check-handover", "/nonexistent/handover.json"][..]] {
        let mut cmd = Command::new(dir.join("bin/cepheus"));
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        env(&mut cmd, dir);
        cmd.env("PATH", format!("{}:/usr/bin:/bin", bin.display())).env("CODEX_THREAD_ID", "agent-thread");
        let mut child = cmd.spawn().unwrap();
        wait_until("the refusal", || child.try_wait().unwrap().is_some());
        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} ran: {err}");
        assert!(err.contains("agent session (CODEX_THREAD_ID is set)"), "{args:?}: {err}");
        assert!(err.contains("Failed to connect to bus"), "{args:?}: {err}");
        assert!(!dir.join("run/deck.sock").exists(), "{args:?} listened");
    }
}
