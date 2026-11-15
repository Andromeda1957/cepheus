use super::*;
use crate::testsupport::TempDir;
use serde_json::{json, Value};

#[test]
fn multiline_and_oversize_input_never_submits_a_truncated_message() {
    let mut terminal = terminal::Terminal::default();
    assert!(terminal.feed("\x1b[200~one\ntwø\x1b[201~").is_empty());
    assert_eq!(terminal.feed("\r"), vec![terminal::Input::Line("one\ntwø".into())]);
    assert!(terminal.feed(&"🦀".repeat(32001)).is_empty());
    assert!(terminal.feed("\r").is_empty());
    terminal.feed("\x15");
    assert_eq!(terminal.feed("ok\r"), vec![terminal::Input::Line("ok".into())]);
}

/// Set in a process that runs one Dot lock test by itself.
const ALONE: &str = "CEPHEUS_TEST_DOT_ALONE";

/// Runs `dot::tests::{name}` in a process of its own, and is true when this
/// call did. A process another test forks holds a copy of every open
/// descriptor until it execs, so in a parallel run a controller lock this
/// test released can stay held there for a moment.
fn alone(name: &str) -> bool {
    if std::env::var_os(ALONE).is_some() {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(format!("dot::tests::{name}"))
        .args(["--exact", "--test-threads", "1"])
        .env(ALONE, "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("1 passed"), "the child ran no test: {text}");
    true
}

#[test]
fn provider_migration_preserves_private_state_and_refuses_process_paths() {
    if alone("provider_migration_preserves_private_state_and_refuses_process_paths") {
        return;
    }
    let temp = TempDir::new("dot-registration");
    let config = state::private_dir(&temp.path().join("config")).unwrap();
    let state = state::State::new(&temp.path().join("state")).unwrap();
    state.write("settings", &json!({"url": "https://chatgpt.com/dots/existing", "native_host": "fixture"})).unwrap();
    state.write("selection", &json!({"dot_id":"existing", "room_id":"room"})).unwrap();
    state.write("pending", &Value::Null).unwrap();
    state::private_dir(&config.join("providers")).unwrap();
    let before = std::fs::read(state.path("settings")).unwrap();
    // A registration naming this deck through another path, as after a reinstall.
    let link = temp.path().join("deck");
    std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &link).unwrap();
    let mut provider = registration::provider(&link, &state.directory);
    state::write_json(&config.join("providers/dot.json"), &provider).unwrap();
    let held = registration::hold(&config, None).unwrap().unwrap();
    assert!(registration::relocate(&held, &std::env::current_exe().unwrap()).unwrap());
    assert_eq!(std::fs::read(state.path("settings")).unwrap(), before);
    drop(held);
    assert!(registration::read(&config, None, false).unwrap().unwrap().runs(&std::env::current_exe().unwrap()));
    let source = temp.path().join("source");
    temp.write("source/plugins/catalog.json", "{}");
    temp.write("source/plugins/dot.lua", "-- preserved fixture");
    state.acquire().unwrap();
    assert!(registration::source_mode(&config, &source, true).is_err());
    state.close();
    registration::source_mode(&config, &source, true).unwrap();
    assert!(!config.join("profile.json").exists());
    registration::source_mode(&config, &source, false).unwrap();
    assert_eq!(
        state::read_json(&config.join("profile.json")).unwrap().unwrap()["plugin_source_dir"],
        source.to_string_lossy().as_ref()
    );
    assert_eq!(std::fs::read(state.path("settings")).unwrap(), before);
    provider["command"][0] = "/proc/self/exe".into();
    state::write_json(&config.join("providers/dot.json"), &provider).unwrap();
    assert!(registration::hold(&config, None).is_err());
    assert_eq!(state::read_json(&config.join("providers/dot.json")).unwrap().unwrap(), provider);
}

// The Python browser route is gone: setup only registers this deck, and a
// leftover Python registration is preserved untouched, never run.
#[test]
fn setup_registers_only_the_deck_and_preserves_a_python_registration() {
    let temp = TempDir::new("dot-native-setup");
    let config = state::private_dir(&temp.path().join("config")).unwrap();
    let directory = temp.path().join("state");
    let state = state::State::new(&directory).unwrap();
    state.write("settings", &json!({"transport":"browser", "url":"https://chatgpt.com/dots/existing"})).unwrap();
    let words = |extra: &[&str]| {
        let mut words: Vec<String> = vec![
            "setup".into(),
            "--config-dir".into(),
            config.to_string_lossy().into(),
            "--state-dir".into(),
            directory.to_string_lossy().into(),
        ];
        words.extend(extra.iter().map(|word| word.to_string()));
        words
    };
    assert_ne!(cli::run(&words(&["--transport", "browser"])), 0);
    assert_ne!(cli::run(&words(&["--import-firefox"])), 0);
    let providers = config.join("providers/dot.json");
    assert!(!providers.exists());
    assert_eq!(cli::run(&words(&["--native-host", "fixture"])), 0);
    let deck = registration::provider(&std::env::current_exe().unwrap(), &directory);
    assert_eq!(state::read_json(&providers).unwrap().unwrap(), deck);
    assert_eq!(
        Value::Object(state.settings().unwrap()),
        json!({"url":"https://chatgpt.com/dots/existing", "native_host":"fixture"})
    );
    let mut python = deck.clone();
    python["command"] = json!(["/usr/bin/python3", config.join("lua/dot/cli.py"), "chat", "--state-dir", directory]);
    state::write_json(&providers, &python).unwrap();
    assert_ne!(cli::run(&words(&[])), 0);
    assert_eq!(state::read_json(&providers).unwrap().unwrap(), python);
}

#[test]
fn trusted_targets_reject_url_and_ssh_argument_injection() {
    for url in [
        "https://chatgpt.com.evil/dots/id",
        "https://chatgpt.com/dots/id?q=1",
        "https://chatgpt.com/dots/a%2Fb",
        "http://chatgpt.com/dots/id",
    ] {
        assert!(host::target_id(Some(&json!(url))).is_err());
    }
    for alias in ["-oProxyCommand=evil", "host;evil", "host user", "host\nother"] {
        assert!(bridge::host_command(alias).is_err());
    }
    for url in [
        "https://127.0.0.1/a",
        "https://evil/a",
        "https://x.oaiusercontent.com:8081/a",
        "http://x.oaiusercontent.com/a",
    ] {
        assert!(!profile::avatar_allowed(url, false));
    }
}

// The owner confirmed the deck shows Dot's blue avatar and the name
// "AndroDot". Lock what the bridge keeps for the view: the card, the stored
// picture, and how the view reads them, including after a reconnect.
#[test]
fn profile_card_keeps_the_confirmed_name_and_avatar_for_the_view() {
    let temp = TempDir::new("dot-profile");
    let state = state::State::new(&temp.path().join("state")).unwrap();
    let picture = b"\x89PNG\r\n\x1a\nfixture-avatar".to_vec();
    let image = "c98cef135dcbc7a764853e615860c53425f8deb17230065d22e25ad2ce17f2b0";
    let service = json!({
        "display_name": " AndroDot ", "avatar_type": "codex-pet",
        "avatar_manifest": {"pet_id": "codex", "snapshot": {"content_sha256": image}},
        "avatar_url": "https://files.oaiusercontent.com/avatar.png",
    });
    let (card, url) = profile::card(&service).unwrap();
    assert_eq!(url.as_deref(), Some("https://files.oaiusercontent.com/avatar.png"));
    assert!(profile::avatar_allowed(url.as_deref().unwrap(), false));
    let answer = json!({"card": card.to_json(), "avatar": {"media": "png", "data": crate::util::base64(&picture)}});
    profile::record(&state, &answer).unwrap();
    let stored = json!({"name":"AndroDot","kind":"codex-pet","pet":"codex","image":image,"file":"avatar.png"});
    assert_eq!(state.read("profile").unwrap().unwrap(), stored);
    assert_eq!(std::fs::read(state.directory.join("avatar.png")).unwrap(), picture);
    let shown = crate::pet::Profile::read(&state.directory).unwrap();
    assert_eq!(shown.name.as_deref(), Some("AndroDot"));
    assert_eq!(shown.art_key().as_deref(), Some("pet:codex"));
    // The same picture is not fetched again, and a card without one keeps it.
    assert_eq!(profile::have(&state, true).as_deref(), Some(image));
    profile::record(&state, &json!({"card": card.to_json(), "avatar": null})).unwrap();
    profile::record(&state, &json!({"card": null, "avatar": null})).unwrap();
    assert_eq!(state.read("profile").unwrap().unwrap(), stored);
    assert_eq!(std::fs::read(state.directory.join("avatar.png")).unwrap(), picture);
}

/// The app's cached profile for the fixture Dot under the fixture login.
fn native_room(temp: &TempDir) {
    let cache = json!({"electron-persisted-atom-state": {
        "cloud-aeon-profiles-v1:[\"account\",\"user\"]": {"fixture": {
            "status":"available", "profile":{"id":"fixture","messaging_room_id":"room"}
        }}
    }});
    temp.write(".codex-global-state.json", cache.to_string());
}

#[test]
fn native_cached_profile_survives_account_scoped_restart_and_waits_for_startup() {
    let temp = TempDir::new("dot-profile-reboot");
    let identity = ("account".to_string(), "user".to_string());
    native_room(&temp);
    assert_eq!(host::cached_profile(temp.path(), "fixture", &identity).unwrap(), "room");
    let atoms = json!({
        "cloud-aeon-profiles-v1:[\"account\",\"user\"]": {"fixture": {
            "status":"available", "profile":{"id":"fixture","messaging_room_id":"room"}
        }},
        "cloud-aeon-profiles-v1:[\"other-account\",\"user\"]": {"fixture": {
            "status":"available", "profile":{"id":"fixture","messaging_room_id":"foreign-room"}
        }}
    });
    let mut cache = json!({"electron-persisted-atom-state":{}, "electron-owner-atoms:owner":atoms});
    temp.write(".codex-global-state.json", cache.to_string());
    assert_eq!(host::cached_profile(temp.path(), "fixture", &identity).unwrap(), "room");
    // The app can populate its cache after the bridge starts. Waiting must
    // reconnect automatically, without selecting another account's room.
    for state in [
        json!({}),
        json!({"electron-owner-atoms:owner":{
            "cloud-aeon-profiles-v1:[\"other-account\",\"user\"]": atoms["cloud-aeon-profiles-v1:[\"other-account\",\"user\"]"]
        }}),
        json!({"electron-owner-atoms:owner":{
            "cloud-aeon-profiles-v1:[\"account\",\"user\"]":{"fixture":{"status":"loading"}}
        }}),
    ] {
        temp.write(".codex-global-state.json", state.to_string());
        assert!(matches!(
            host::cached_profile(temp.path(), "fixture", &identity),
            Err(host::HostError::Transient(_, 5))
        ));
    }
    // A migration can briefly leave both copies. Identical rooms agree;
    // conflicting rooms must still stop instead of switching conversations.
    cache["electron-persisted-atom-state"] = atoms.clone();
    temp.write(".codex-global-state.json", cache.to_string());
    assert_eq!(host::cached_profile(temp.path(), "fixture", &identity).unwrap(), "room");
    cache["electron-persisted-atom-state"]["cloud-aeon-profiles-v1:[\"account\",\"user\"]"]["fixture"]["profile"]
        ["messaging_room_id"] = json!("changed-room");
    temp.write(".codex-global-state.json", cache.to_string());
    assert!(matches!(host::cached_profile(temp.path(), "fixture", &identity), Err(host::HostError::Attention(_))));
}

/// One HTTP request from the native host: its request line and body.
fn native_request(stream: &std::net::TcpStream) -> (String, Vec<u8>) {
    use std::io::{BufRead, BufReader, Read};
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let request = line.clone();
    let mut length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse::<usize>().unwrap();
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    (request, body)
}

fn native_room_metadata() -> Value {
    json!({"id":"room","aeon_id":"fixture","type":"DM","app_source":"chatgpt:messaging", "members":[
        {"account_user_id":"human"}, {"account_user_id":"bot","aeon_id":"fixture"}
    ]})
}

/// A signed-in app-server answering the account RPC for the fixture login.
fn native_account() -> std::sync::Arc<dyn Fn() -> Result<host::AccountClient, host::HostError> + Send + Sync> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    std::sync::Arc::new(|| {
        let (client, server) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            let claims = json!({"exp":4102444800u64, "https://api.openai.com/auth":{"chatgpt_account_id":"account","chatgpt_user_id":"user"}});
            let encoded = crate::util::base64(claims.to_string().as_bytes()).replace('+', "-").replace('/', "_");
            let token = format!("fixture.{}.signature", encoded.trim_end_matches('='));
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let Some(id) = request.get("id") else { continue };
                let result = if request["method"] == "initialize" {
                    json!({})
                } else {
                    json!({"authMethod":"chatgpt", "authToken":token})
                };
                writeln!(writer, "{}", json!({"id":id,"result":result})).unwrap();
            }
        });
        host::AccountClient::over(Box::new(client.try_clone().unwrap()), Box::new(client), "fixture".into(), None)
    })
}

#[test]
fn native_host_roundtrip_uses_scoped_account_rpc_and_one_idempotent_post() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::Arc;
    let temp = TempDir::new("dot-native-host");
    native_room(&temp);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let http = std::thread::spawn(move || {
        for step in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let (request, body) = native_request(&stream);
            let value = match step {
                0 => {
                    assert!(request.starts_with("GET /messaging/rooms/room "));
                    native_room_metadata()
                }
                1 => json!({"items":[{"id":"old","account_user_id":"bot","content":{"text":"hello"},"created_at":"2026-11-07T06:00:00Z"}]}),
                2 => {
                    assert!(request.starts_with("POST /messaging/rooms/room/files "));
                    assert!(body.windows(b"name=\"file\"; filename=\"pasted.png\"".len()).any(|w| w == b"name=\"file\"; filename=\"pasted.png\""));
                    assert!(body.windows(b"image/png".len()).any(|w| w == b"image/png"));
                    assert!(body.windows(b"\x89PNG\r\n\x1a\nfixture".len()).any(|w| w == b"\x89PNG\r\n\x1a\nfixture"));
                    json!({"id":"image-file"})
                }
                _ => {
                    assert!(request.starts_with("POST /messaging/rooms/room/messages "));
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(body["request_id"], "11111111-1111-4111-8111-111111111111");
                    assert_eq!(body["idempotency_token"], body["request_id"]);
                    assert_eq!(body["content"]["text"], "one message");
                    assert_eq!(body["content"]["attachments"],json!([{"type":"file","file_id":"image-file"}]));
                    json!({"id":"new"})
                }
            }.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", value.len(), value).unwrap();
        }
    });
    let env = Arc::new(host::HostEnv {
        api: format!("http://{address}"),
        codex_home: temp.path().into(),
        account: native_account(),
        live: None,
        limits: host::Limits::production(),
        loopback: true,
    });
    let mut client = host::Client::new(env, None).unwrap();
    let snapshot = client.open(Some(&json!("https://chatgpt.com/dots/fixture"))).unwrap();
    assert_eq!(snapshot["messages"][0]["role"], "assistant");
    assert_eq!(snapshot["activity_known"], false);
    let image = api::Image::new("pasted.png", b"\x89PNG\r\n\x1a\nfixture").unwrap();
    let uploaded = client.upload(Some(&serde_json::to_value(image).unwrap())).unwrap();
    let sent = client
        .send_with_files(
            Some(&json!("one message")),
            Some(&json!("11111111-1111-4111-8111-111111111111")),
            Some(&json!([uploaded["id"]])),
        )
        .unwrap();
    assert_eq!(sent["id"], "new");
    client.close();
    http.join().unwrap();
}

// "Native response timed out." was permanent: the chat stopped reconnecting
// and refused every later text or image. A response that stalls after its
// head is temporary, and the host keeps answering requests afterwards.
#[test]
fn stalled_native_response_is_temporary_and_the_host_keeps_serving() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::time::Duration;
    let temp = TempDir::new("dot-native-stall");
    native_room(&temp);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let http = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (request, _) = native_request(&stream);
        assert!(request.starts_with("GET /messaging/rooms/room "));
        let value = native_room_metadata().to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", value.len(), value)
            .unwrap();
        drop(stream);
        let (mut stream, _) = listener.accept().unwrap();
        let (request, _) = native_request(&stream);
        assert!(request.starts_with("GET /messaging/rooms/room/messages"));
        // The head and part of the body arrive; the rest never does.
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\n{{\"items\":[").unwrap();
        stream.flush().unwrap();
        let _ = released.recv_timeout(Duration::from_secs(20));
    });
    let env = Arc::new(host::HostEnv {
        api: format!("http://{address}"),
        codex_home: temp.path().into(),
        account: native_account(),
        live: None,
        limits: host::Limits {
            connect: Duration::from_secs(5),
            stall: Duration::from_millis(300),
            response: Duration::from_secs(5),
            total: Duration::from_secs(10),
        },
        loopback: true,
    });
    let (chat, native) = UnixStream::pair().unwrap();
    let served = std::thread::spawn(move || host::serve(env, native.try_clone().unwrap(), native));
    let mut lines = BufReader::new(chat.try_clone().unwrap());
    let mut answer = |request: Value| -> Value {
        writeln!(&chat, "{request}").unwrap();
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let hello = answer(json!({"id":1,"action":"hello"}));
    assert_eq!(hello["result"]["protocol"], host::PROTOCOL);
    let started = std::time::Instant::now();
    let opened = answer(json!({"id":2,"action":"open","url":"https://chatgpt.com/dots/fixture"}));
    assert_eq!(opened["id"], 2);
    assert_eq!(opened["ok"], false);
    assert_eq!(opened["retry"], true, "a stalled response must be retried, not need attention: {opened}");
    assert!(started.elapsed() < Duration::from_secs(5), "a stalled read fails after its stall limit");
    // The failed client is closed; the host still answers the next request.
    assert_eq!(answer(json!({"id":3,"action":"hello"}))["ok"], true);
    release.send(()).unwrap();
    chat.shutdown(std::net::Shutdown::Write).unwrap();
    served.join().unwrap();
    http.join().unwrap();
}

// Noninteractive SSH shells on enrolled hosts need not include ~/.local/bin.
// Exercise the real remote command, preserving stdin and literal argument words.
#[test]
fn ssh_host_connects_without_interactive_local_bin_path() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new("dot-ssh-path");
    let ssh = temp.write(
        "bin/ssh",
        r#"#!/bin/sh
while [ "$1" != "--" ]; do shift; done
shift
shift
PATH=/usr/bin:/bin
export PATH
exec /bin/sh -c "$*"
"#,
    );
    let native = temp.write(
        ".local/bin/cepheus",
        r#"#!/bin/sh
[ "$1" = dot ] && [ "$2" = host ] || exit 2
IFS= read -r request
printf '%s\n' "$request"
"#,
    );
    for file in [&ssh, &native] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut child = bridge::host_command("fixture")
        .unwrap()
        .env("PATH", ssh.parent().unwrap())
        .env("HOME", temp.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    let request = r#"{"id":1,"action":"hello"}"#;
    writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "noninteractive SSH could not run the installed native host: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), request);
}
