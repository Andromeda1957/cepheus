use super::*;
use crate::testsupport::TempDir;

struct Runtime(std::process::Child);

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn pinned_opencode_native_names_deliver_once_without_running_the_launcher() {
    let dir = TempDir::new("native-opencode-message");
    let conversation = "ses_e3e1815effffzLvhwuJIKrGC5r";
    // A native shell stands in for the installed ELF. Its API commands execute
    // this private fixture; no PATH launcher or owner runtime is invoked.
    dir.write(
        "api",
        format!(
            r#"case "$1" in
--help) echo '--data --param' ;;
session.get) echo '{{"data":{{"id":"{conversation}"}}}}' ;;
session.prompt)
    printf '%s\n' "$5" >> submitted
    echo '{{"data":{{"sessionID":"{conversation}","id":"message","type":"user"}}}}'
    ;;
*) exit 1 ;;
esac
"#
        ),
    );
    for name in ["opencode", "opencode.exe"] {
        let executable = dir.path().join(name);
        std::fs::copy("/bin/sh", &executable).unwrap();
        let runtime = Runtime(
            Command::new(&executable)
                .arg0(name)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let request =
            request(Tool::OpenCode, &Config::default(), conversation, "which model are you", dir.path(), dir.path())
                .unwrap()
                .for_owner(runtime.0.id());
        assert!(matches!(
            request.deliver(Instant::now() + Duration::from_secs(5), &AtomicBool::new(false)),
            Delivery::Accepted(crate::protocol::Delivered::Queued)
        ));
    }
    let submitted = std::fs::read_to_string(dir.path().join("submitted")).unwrap();
    let messages: Vec<serde_json::Value> = submitted.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(messages.len(), 2);
    for message in messages {
        assert_eq!(message["text"], "which model are you");
        assert_eq!(message["delivery"], "queue");
    }
}

#[test]
fn unrelated_native_executables_are_refused_before_api_execution() {
    let dir = TempDir::new("native-opencode-wrong-runtime");
    dir.write("api", "echo attempted > submitted\n");
    for name in ["codex.exe", "other.exe", "opencode", "opencode.exe"] {
        let runtime = Runtime(
            Command::new("/bin/sh")
                .arg0(name)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let request = request(
            Tool::OpenCode,
            &Config::default(),
            "ses_e3e1815effffzLvhwuJIKrGC5r",
            "which model are you",
            dir.path(),
            dir.path(),
        )
        .unwrap()
        .for_owner(runtime.0.id());
        assert!(matches!(
            request.deliver(Instant::now() + Duration::from_secs(5), &AtomicBool::new(false)),
            Delivery::Failed(_)
        ));
    }
    assert!(!dir.path().join("submitted").exists());
}

#[test]
fn missing_historical_opencode_session_is_never_recreated_or_prompted() {
    let dir = TempDir::new("native-opencode-missing-history");
    let conversation = "ses_e3e1815effffzLvhwuJIKrGC5r";
    dir.write(
        "api",
        format!(
            r#"case "$1" in
--help) echo '--data --param' ;;
session.get)
    echo '{{"_tag":"SessionNotFoundError","sessionID":"{conversation}"}}'
    exit 1 ;;
*) echo attempted > submitted; exit 1 ;;
esac
"#
        ),
    );
    let executable = dir.path().join("opencode.exe");
    std::fs::copy("/bin/sh", &executable).unwrap();
    let runtime = Runtime(
        Command::new(&executable)
            .arg0("opencode.exe")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let request = request(Tool::OpenCode, &Config::default(), conversation, "preserve history", dir.path(), dir.path())
        .unwrap()
        .for_owner(runtime.0.id());
    assert!(matches!(
        request.deliver(Instant::now() + Duration::from_secs(5), &AtomicBool::new(false)),
        Delivery::Failed(error) if error.contains("does not exist")
    ));
    assert!(!dir.path().join("submitted").exists());
}
