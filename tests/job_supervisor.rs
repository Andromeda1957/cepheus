//! Real executor guard checks; no models, services or interactive terminals.
use std::{
    fs,
    io::Write,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};
const HOSTED_ENV: [&str; 8] = [
    "CEPHEUS_SESSION",
    "AGENT_DECK_SESSION", // legacy-name
    "CEPHEUS_ACCOUNT",
    "AGENT_DECK_ACCOUNT", // legacy-name
    "CEPHEUS_ACCOUNT_DIRECTORY",
    "AGENT_DECK_ACCOUNT_DIRECTORY", // legacy-name
    "CEPHEUS_ACCOUNT_ID",
    "AGENT_DECK_ACCOUNT_ID", // legacy-name
];
struct Private(PathBuf);
impl Private {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cepheus-job-executor-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Private {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn spec(dir: &Private, provider: serde_json::Value) -> serde_json::Value {
    let program = "import json,os,sys; print(json.dumps({'args':sys.argv[1:],'stdin':sys.stdin.read(),'private':os.getenv('PRIVATE_SECRET'),'hosted':{p+s:os.getenv(p+s) for p in ('CEPHEUS_','AGENT_DECK_') for s in ('SESSION','ACCOUNT','ACCOUNT_DIRECTORY','ACCOUNT_ID')},'only':os.getenv('ONLY')}))"; // legacy-name
    serde_json::json!({"argv":["/usr/bin/python3","-c",program,"literal $HOME * ; spaces"],"cwd":dir.0,"provider":provider,"stdin":"do <work> & done","env_clear":true,"env_allowlist":[],"env":{"ONLY":"visible"}})
}
fn executor(dir: &Private, open: bool) -> std::process::Output {
    let (read, mut write) = UnixStream::pair().unwrap();
    if open {
        write.write_all(&[1]).unwrap();
    }
    drop(write);
    let fd = read.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cepheus"));
    command
        .args(["__job-exec", dir.0.to_str().unwrap(), &fd.to_string()])
        .env("PRIVATE_SECRET", "never forward")
        .env("CEPHEUS_TEMP_DIR", dir.0.join("clients"))
        .stdin(Stdio::null());
    for name in HOSTED_ENV {
        command.env(name, "foreign-parent");
    }
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.output().unwrap()
}
#[test]
fn closed_parent_gate_prevents_any_workload_input_output_or_execution() {
    let dir = Private::new();
    fs::write(dir.0.join("execution.json"), serde_json::to_vec(&spec(&dir, serde_json::Value::Null)).unwrap()).unwrap();
    fs::set_permissions(dir.0.join("execution.json"), fs::Permissions::from_mode(0o600)).unwrap();
    let result = executor(&dir, false);
    assert!(!result.status.success());
    assert!(!dir.0.join("input").exists());
    assert!(!dir.0.join("output.log").exists());
    assert!(String::from_utf8_lossy(&result.stderr).contains("gate closed"));
}
#[test]
fn argv_and_finite_input_are_literal_and_environment_is_explicit() {
    for (provider, clear, allowlist, explicit) in
        [serde_json::Value::Null, serde_json::json!("codex")].into_iter().flat_map(|provider| {
            [(true, false, false), (true, true, false), (false, false, false), (false, false, true)]
                .into_iter()
                .map(move |(clear, allowlist, explicit)| (provider.clone(), clear, allowlist, explicit))
        })
    {
        let dir = Private::new();
        let mut request = spec(&dir, provider.clone());
        request["env_clear"] = serde_json::json!(clear);
        if allowlist {
            request["env_allowlist"] = serde_json::json!(HOSTED_ENV);
        }
        if explicit {
            for name in HOSTED_ENV {
                request["env"][name] = serde_json::json!("explicit-job");
            }
        }
        fs::write(dir.0.join("execution.json"), serde_json::to_vec(&request).unwrap()).unwrap();
        fs::set_permissions(dir.0.join("execution.json"), fs::Permissions::from_mode(0o600)).unwrap();
        let result = executor(&dir, true);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        assert!(result.stdout.is_empty(), "output stays private");
        let log = dir.0.join("output.log");
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&log).unwrap()).unwrap();
        assert_eq!(value["args"], serde_json::json!(["literal $HOME * ; spaces"]));
        assert_eq!(value["private"], if clear { serde_json::Value::Null } else { serde_json::json!("never forward") });
        for name in HOSTED_ENV {
            let expected = if explicit { serde_json::json!("explicit-job") } else { serde_json::Value::Null };
            assert_eq!(value["hosted"][name], expected, "{name}: clear={clear}, allowlist={allowlist}");
        }
        assert_eq!(value["only"], "visible");
        if provider.is_null() {
            assert_eq!(value["stdin"], "do <work> & done");
        } else {
            let text = value["stdin"].as_str().unwrap();
            assert!(text.contains("<cross-session-message source=\"cepheus\""));
            assert!(text.contains("do &lt;work&gt; &amp; done"));
        }
        for path in [log, dir.0.join("input")] {
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
        }
    }
}
