use super::*;

use serde_json::json;

use crate::testsupport::TempDir;

fn signed_in(account: &str, renewed: i64) -> LoginState {
    LoginState { account: Some(account.into()), renewed: Some(renewed), ..Default::default() }
}

#[test]
fn a_host_without_a_sign_in_gets_the_hubs() {
    assert_eq!(decide(&signed_in("a", 10), &LoginState::default()), Move::ToHost);
}

#[test]
fn the_hub_is_written_only_with_a_newer_login_for_its_own_account() {
    assert_eq!(decide(&LoginState::default(), &signed_in("a", 10)), Move::Stay, "nothing here: left alone");
    assert_eq!(decide(&signed_in("a", 10), &signed_in("a", 20)), Move::ToHub, "renewed on the host: comes here");
    assert_eq!(decide(&signed_in("a", 20), &signed_in("a", 10)), Move::ToHost, "renewed here: goes there");
    assert_eq!(decide(&signed_in("a", 10), &signed_in("a", 10)), Move::Stay);
    assert_eq!(decide(&signed_in("a", 10), &signed_in("b", 20)), Move::Report, "another account is never overwritten");
}

#[test]
fn a_rejected_login_is_replaced_and_never_spread() {
    let rejected = |account: &str, renewed: i64| LoginState { rejected: true, ..signed_in(account, renewed) };
    assert_eq!(
        decide(&signed_in("a", 10), &rejected("b", 20)),
        Move::ToHost,
        "a host whose login cannot renew gets the hub's"
    );
    assert_eq!(decide(&signed_in("a", 10), &rejected("a", 20)), Move::ToHost);
    assert_eq!(decide(&rejected("a", 10), &signed_in("a", 5)), Move::Stay, "a rejected hub login is not copied out");
    assert_eq!(decide(&rejected("a", 10), &signed_in("a", 20)), Move::ToHub);
    assert_eq!(
        decide(&rejected("a", 10), &LoginState::default()),
        Move::Stay,
        "a rejected hub cannot seed an empty host"
    );
}

fn write(path: &Path, value: Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_string(&value).unwrap()).unwrap();
}

fn machine(dir: &TempDir) -> LoginPaths {
    LoginPaths { claude_dir: dir.path().join(".claude"), codex_dir: dir.path().join(".codex") }
}

#[test]
fn a_claude_sign_in_moves_without_the_machines_own_settings() {
    let hub_dir = TempDir::new("logins-hub");
    let host_dir = TempDir::new("logins-host");
    let (hub, host) = (machine(&hub_dir), machine(&host_dir));
    write(
        &hub.claude_dir.join(".credentials.json"),
        json!({"claudeAiOauth": {"refreshToken": "r2", "expiresAt": 2000}, "mcpOAuth": {"hub": 1}}),
    );
    write(
        &hub_dir.path().join(".claude.json"),
        json!({"oauthAccount": {"accountUuid": "acct"}, "projects": {"hub": 1}}),
    );
    write(
        &host.claude_dir.join(".credentials.json"),
        json!({"claudeAiOauth": {"refreshToken": "r1", "expiresAt": 1000}, "mcpOAuth": {"host": 1}}),
    );
    write(
        &host_dir.path().join(".claude.json"),
        json!({"oauthAccount": {"accountUuid": "acct"}, "projects": {"host": 1}}),
    );

    let now = 0;
    assert_eq!(state(&hub, LoginTool::Claude, now), signed_in("acct", 2000));
    assert_eq!(decide(&state(&hub, LoginTool::Claude, now), &state(&host, LoginTool::Claude, now)), Move::ToHost);
    import(&host, LoginTool::Claude, &export(&hub, LoginTool::Claude).unwrap()).unwrap();

    assert_eq!(state(&host, LoginTool::Claude, now), signed_in("acct", 2000));
    let creds: Value =
        serde_json::from_str(&fs::read_to_string(host.claude_dir.join(".credentials.json")).unwrap()).unwrap();
    assert_eq!(creds["mcpOAuth"], json!({"host": 1}), "the host's own MCP sign-ins stay");
    let json: Value = serde_json::from_str(&fs::read_to_string(host_dir.path().join(".claude.json")).unwrap()).unwrap();
    assert_eq!(json["projects"], json!({"host": 1}));
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(host.claude_dir.join(".credentials.json")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_claude_login_past_its_refresh_expiry_is_rejected_and_one_without_a_token_is_absent() {
    let dir = TempDir::new("logins-expired");
    let paths = machine(&dir);
    write(
        &paths.claude_dir.join(".credentials.json"),
        json!({"claudeAiOauth": {"refreshToken": "r", "expiresAt": 10, "refreshTokenExpiresAt": 50}}),
    );
    write(&dir.path().join(".claude.json"), json!({"oauthAccount": {"accountUuid": "acct"}}));
    assert!(!state(&paths, LoginTool::Claude, 49).rejected);
    assert!(state(&paths, LoginTool::Claude, 50).rejected);
    write(&paths.claude_dir.join(".credentials.json"), json!({"claudeAiOauth": {"refreshToken": "", "expiresAt": 10}}));
    assert_eq!(state(&paths, LoginTool::Claude, 0).account, None, "signed out");
}

#[test]
fn a_codex_sign_in_moves_its_tokens_and_is_dated_by_its_last_refresh() {
    let hub_dir = TempDir::new("logins-codex-hub");
    let host_dir = TempDir::new("logins-codex-host");
    let (hub, host) = (machine(&hub_dir), machine(&host_dir));
    let auth = json!({"OPENAI_API_KEY": null, "auth_mode": "chatgpt", "last_refresh": "2026-10-31T12:00:00.5Z", "tokens": {"account_id": "acct", "refresh_token": "r"}});
    write(&hub.codex_dir.join("auth.json"), auth.clone());
    assert_eq!(state(&host, LoginTool::Codex, 0), LoginState::default());
    let hub_state = state(&hub, LoginTool::Codex, 0);
    assert_eq!(hub_state.account.as_deref(), Some("acct"));
    assert_eq!(hub_state.renewed, rfc3339_ms("2026-10-31T12:00:00.500Z"));
    let read = |paths: &LoginPaths| -> Value {
        serde_json::from_str(&fs::read_to_string(paths.codex_dir.join("auth.json")).unwrap()).unwrap()
    };
    import(&host, LoginTool::Codex, &export(&hub, LoginTool::Codex).unwrap()).unwrap();
    assert_eq!(read(&host), auth, "a machine without a sign-in gets the whole of it");
    assert!(import(&host, LoginTool::Codex, &json!({"auth_mode": "x"})).is_err(), "an incomplete sign-in is refused");
    assert!(export(&host_dir_paths_without_login(), LoginTool::Codex).is_err());

    // A newer login from another machine replaces only the login: it cannot
    // give this machine an API key or another sign-in mode.
    let renewed = json!({"OPENAI_API_KEY": "planted", "auth_mode": "apikey", "last_refresh": "2026-10-31T13:00:00Z", "tokens": {"account_id": "acct", "refresh_token": "r2"}});
    import(&hub, LoginTool::Codex, &renewed).unwrap();
    let kept = read(&hub);
    assert_eq!(kept["tokens"], renewed["tokens"]);
    assert_eq!(kept["last_refresh"], renewed["last_refresh"]);
    assert_eq!(kept["OPENAI_API_KEY"], Value::Null, "the machine's own key setting stays");
    assert_eq!(kept["auth_mode"], "chatgpt", "the machine's own sign-in mode stays");
}

fn host_dir_paths_without_login() -> LoginPaths {
    LoginPaths { claude_dir: PathBuf::from("/nonexistent/.claude"), codex_dir: PathBuf::from("/nonexistent/.codex") }
}

#[test]
fn claudes_account_file_sits_beside_the_default_directory_and_inside_a_moved_one() {
    let default =
        LoginPaths { claude_dir: PathBuf::from("/home/u/.claude"), codex_dir: PathBuf::from("/home/u/.codex") };
    assert_eq!(default.claude_json(), PathBuf::from("/home/u/.claude.json"));
    let moved = LoginPaths { claude_dir: PathBuf::from("/srv/claude-config"), ..default };
    assert_eq!(moved.claude_json(), PathBuf::from("/srv/claude-config/.claude.json"));
}

#[test]
fn rfc3339_times_order_correctly_whatever_their_precision_or_offset() {
    assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(rfc3339_ms("2026-10-31T22:41:55Z"), Some(1_793_486_515_000));
    assert_eq!(rfc3339_ms("2026-10-31T22:41:55.1Z"), Some(1_793_486_515_100));
    assert_eq!(rfc3339_ms("2026-10-31T22:41:55.123456789Z"), Some(1_793_486_515_123));
    assert_eq!(rfc3339_ms("2026-10-31T17:41:55-05:00"), rfc3339_ms("2026-10-31T22:41:55Z"));
    assert!(rfc3339_ms("2026-10-31T22:41:55.9Z") > rfc3339_ms("2026-10-31T22:41:55.10Z"));
    assert_eq!(rfc3339_ms("2024-02-29T00:00:00Z").map(|ms| ms % 86_400_000), Some(0));
    for bad in [
        "",
        "2026-10-31",
        "2026-13-01T00:00:00Z",
        "2026-10-31T22:41:55",
        "2026-10-31T22:41:55.Z",
        "yyyy-10-31T22:41:55Z",
        // A host's file can hold anything: a character split by a fixed
        // byte offset must be refused, not panic the hub's sync.
        "2026-10-3\u{e9}12:00:00Z",
        "2026-10-31T12:00:00+00\u{e9}0",
    ] {
        assert_eq!(rfc3339_ms(bad), None, "{bad:?}");
    }
}

#[test]
fn native_credential_consumers_share_only_newer_same_account_logins_and_keep_settings() {
    let hub_dir = TempDir::new("login-canonical");
    let native_dir = TempDir::new("login-native");
    let (hub, native) = (machine(&hub_dir), machine(&native_dir));
    let settings = Settings { credential_mirrors: vec![native.clone()], ..Default::default() };
    let old = json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"last_refresh":"2026-10-30T12:00:00Z","tokens":{"account_id":"same","refresh_token":"old"},"local":"canonical"});
    let newer = json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"last_refresh":"2026-10-31T12:00:00Z","tokens":{"account_id":"same","refresh_token":"renewed"},"local":"native"});
    write(&hub.codex_auth(), old.clone());
    write(&native.codex_auth(), newer.clone());
    mirrors::reconcile_at(&hub, LoginTool::Codex, &settings, now_ms()).unwrap();
    assert_eq!(export(&hub, LoginTool::Codex).unwrap()["tokens"], newer["tokens"]);
    assert_eq!(export(&hub, LoginTool::Codex).unwrap()["local"], "canonical");
    assert_eq!(export(&native, LoginTool::Codex).unwrap()["local"], "native");
    import_with_settings(&hub, LoginTool::Codex, &old, &settings).unwrap();
    assert_eq!(
        export(&native, LoginTool::Codex).unwrap()["tokens"],
        newer["tokens"],
        "late replies cannot undo renewal"
    );
    let other = json!({"last_refresh":"2026-11-01T12:00:00Z","tokens":{"account_id":"other","refresh_token":"other"}});
    write(&native.codex_auth(), other.clone());
    assert!(mirrors::reconcile_at(&hub, LoginTool::Codex, &settings, now_ms()).is_err());
    assert!(import_with_settings(&hub, LoginTool::Codex, &other, &settings).is_err());
    assert_eq!(export(&hub, LoginTool::Codex).unwrap()["tokens"], newer["tokens"]);
    assert_eq!(export(&native, LoginTool::Codex).unwrap(), other);

    // Claude also has machine-specific MCP credentials and a separate account file.
    for (paths, home, expires) in [(&hub, &hub_dir, 100), (&native, &native_dir, 200)] {
        write(
            &paths.claude_credentials(),
            json!({"claudeAiOauth":{"refreshToken":"r","expiresAt":expires},"mcpOAuth":{"keep":expires}}),
        );
        write(
            &home.path().join(".claude.json"),
            json!({"oauthAccount":{"accountUuid":"same"},"projects":{"keep":expires}}),
        );
    }
    mirrors::reconcile_at(&hub, LoginTool::Claude, &settings, now_ms()).unwrap();
    assert_eq!(state(&hub, LoginTool::Claude, 0).renewed, Some(200));
    assert_eq!(read_optional(&hub.claude_credentials()).unwrap().unwrap()["mcpOAuth"]["keep"], 100);
    assert_eq!(read_optional(&hub.claude_json()).unwrap().unwrap()["projects"]["keep"], 100);
    // Refuse unreadable JSON rather than erasing a consumer's other settings.
    fs::write(native.codex_auth(), b"{broken").unwrap();
    assert!(mirrors::reconcile_at(&hub, LoginTool::Codex, &settings, now_ms()).is_err());
    assert_eq!(fs::read(native.codex_auth()).unwrap(), b"{broken");
}

#[test]
fn profile_import_never_replaces_another_account_or_newer_credentials() {
    use crate::{protocol::Tool, providers::accounts::Profile};
    let dir = TempDir::new("profile-logins");
    let profile = Profile { provider: Tool::Codex, directory: dir.path().join("one") };
    let other = Profile { provider: Tool::Codex, directory: dir.path().join("two") };
    let login = |account: &str, hour: u8| json!({"tokens":{"account_id":account,"refresh_token":"synthetic"},"last_refresh":format!("2026-11-05T{hour:02}:00:00Z")});
    import_profile(&profile, &login("first", 10), 0).unwrap();
    import_profile(&other, &login("second", 9), 0).unwrap();
    let saved = fs::read(profile.directory.join("auth.json")).unwrap();
    for incoming in
        [login("first", 9), login("first", 10), login("second", 11), json!({"tokens":{"refresh_token":"synthetic"}})]
    {
        assert!(import_profile(&profile, &incoming, 0).is_err());
        assert_eq!(fs::read(profile.directory.join("auth.json")).unwrap(), saved);
    }
    let mut signed_out = login("first", 10);
    signed_out["tokens"]["refresh_token"] = json!("");
    write(&profile.directory.join("auth.json"), signed_out.clone());
    assert!(import_profile(&profile, &login("second", 11), 0).is_err());
    assert_eq!(
        export(&profile, LoginTool::Codex).unwrap(),
        signed_out,
        "missing refresh token does not release the account binding"
    );
    import_profile(&profile, &login("first", 11), 0).unwrap();
    assert_eq!(state(&profile, LoginTool::Codex, 0).renewed, rfc3339_ms("2026-11-05T11:00:00Z"));
    assert_eq!(state(&other, LoginTool::Codex, 0).account.as_deref(), Some("second"));
    let mut unidentified = login("first", 11);
    unidentified["tokens"].as_object_mut().unwrap().remove("account_id");
    write(&profile.directory.join("auth.json"), unidentified.clone());
    assert!(import_profile(&profile, &login("first", 12), 0).is_err());
    assert_eq!(export(&profile, LoginTool::Codex).unwrap(), unidentified);
    fs::write(profile.directory.join("auth.json"), b"{incomplete").unwrap();
    assert!(import_profile(&profile, &login("first", 12), 0).is_err());
    assert_eq!(
        fs::read(profile.directory.join("auth.json")).unwrap(),
        b"{incomplete",
        "uncertain local identity is preserved"
    );
    fs::remove_file(profile.directory.join("auth.json")).unwrap();
    std::os::unix::fs::symlink(other.directory.join("auth.json"), profile.directory.join("auth.json")).unwrap();
    assert!(export_profile(&profile).is_err(), "a profile cannot export another directory's credential symlink");
    assert!(import_profile(&profile, &login("second", 12), 0).is_err());
    assert_eq!(state(&other, LoginTool::Codex, 0).renewed, rfc3339_ms("2026-11-05T09:00:00Z"));
}

#[test]
fn old_peers_receive_no_profile_request_or_default_account_substitution() {
    use std::io::Read;
    let (mut hub, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_nonblocking(true).unwrap();
    let host = crate::config::HostConfig {
        role: Default::default(),
        name: "fixture".into(),
        label: None,
        tag: 1,
        addresses: vec![],
        ssh: None,
        fingerprint: String::new(),
    };
    let notes = sync_profiles(&host, &mut hub, &Settings::default(), PROFILE_PROTOCOL - 1).unwrap();
    assert!(notes[0].1.as_deref().unwrap().contains("update needed"));
    assert_eq!(peer.read(&mut [0; 1]).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}
