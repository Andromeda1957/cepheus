use super::*;
use crate::protocol::{read_msg, AlertKind, ClientMsg, ClientRole, ServerMsg, PROTOCOL_VERSION};
use crate::testsupport::TempDir;

fn messages(receiver: &mpsc::Receiver<Out>) -> Vec<ServerMsg> {
    receiver
        .try_iter()
        .filter_map(|out| match out {
            Out::Msg(bytes) => read_msg::<_, ServerMsg>(&mut bytes.as_slice()).unwrap(),
            _ => None,
        })
        .collect()
}

#[test]
fn renewal_warnings_reach_the_deck_and_phone_once_and_rearm_after_a_renewal() {
    let dir = TempDir::new("login-warning-service");
    let opts = crate::service::tests::options(&dir, "true");
    let credentials = opts.claude_dir.join(".credentials.json");
    let account = opts.claude_dir.join(".claude.json");
    std::fs::create_dir_all(&opts.claude_dir).unwrap();
    let now = logins::now_ms();
    let save = |expires| {
        std::fs::write(
            &credentials,
            serde_json::to_vec(&serde_json::json!({"claudeAiOauth":{
            "refreshToken":"fixture-only","expiresAt":now-1000,"refreshTokenExpiresAt":expires}}))
            .unwrap(),
        )
        .unwrap();
    };
    std::fs::write(account, br#"{"oauthAccount":{"accountUuid":"fixture"}}"#).unwrap();
    save(now + 71 * 3_600_000);
    let (tx, _events) = mpsc::channel();
    let mut service = Service::new(opts, tx);
    let (out, deck) = mpsc::channel();
    service.handle(Ev::ClientNew { id: 1, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    service.handle_client(1, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Control });
    let (out, phone) = mpsc::channel();
    service.handle(Ev::ClientNew { id: 2, out, frame_busy: Arc::new(AtomicBool::new(false)) });
    service.handle_client(2, ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Remote });
    messages(&deck);
    messages(&phone);
    let start = Instant::now();
    tick(&mut service, start);
    assert!(messages(&deck).iter().any(
        |message| matches!(message, ServerMsg::Notice { text, error:true } if text.contains("Claude login renewal"))
    ));
    assert!(messages(&phone).iter().any(
        |message| matches!(message, ServerMsg::Alert(alert) if alert.kind==AlertKind::Error && alert.id.is_none())
    ));
    service.handle_client(
        1,
        ClientMsg::Plugin {
            request: 10,
            plugin: "login-sync".into(),
            command: "status".into(),
            args: serde_json::json!({}),
        },
    );
    let health = messages(&deck)
        .into_iter()
        .find_map(|message| match message {
            ServerMsg::PluginResult { request: 10, data, error: None } => Some(data),
            _ => None,
        })
        .expect("the actual status command keeps the active warning available");
    assert_eq!(health["tools"][0]["refresh_expires_ms"], now + 71 * 3_600_000);
    assert!(
        health["tools"][0]["warning"].as_str().unwrap().contains("renewal"),
        "late clients can inspect the active warning"
    );
    assert!(!health.to_string().contains("fixture-only"));
    tick(&mut service, start + Duration::from_secs(31));
    assert!(messages(&deck).is_empty(), "polling does not repeat an unchanged warning");
    let listed = |service: &Service| service.errors.borrow().iter().any(|e| e.text.contains("login renewal"));
    assert!(listed(&service));
    save(now + 10 * 86_400_000);
    tick(&mut service, start + Duration::from_secs(62));
    assert!(status(&service).unwrap()["tools"][0]["warning"].is_null());
    // The renewed login takes its warning back from every deck's list
    // (2026-11-12: "windows still reports login issue" after the renewal
    // reached every host), and so does a fresh watcher after a handover.
    assert!(!listed(&service), "the renewal withdraws the warning it answered");
    service.log_error("Claude login renewal expires in 5 hours. Refresh the login before then.");
    service.native.logins = Default::default();
    tick(&mut service, start + Duration::from_secs(63));
    assert!(!listed(&service), "a warning listed before a handover is withdrawn too");
    save(now + 70 * 3_600_000);
    tick(&mut service, start + Duration::from_secs(93));
    assert!(messages(&deck).iter().any(|message| matches!(message, ServerMsg::Notice { error: true, .. })));
    service.opts.config.plugin_settings.insert("login-sync".into(), serde_json::json!({"warn_before_seconds":1}));
    tick(&mut service, start + Duration::from_secs(124));
    assert!(messages(&deck)
        .iter()
        .any(|message| matches!(message,ServerMsg::Notice{text,error:true} if text.contains("configuration"))));
    tick(&mut service, start + Duration::from_secs(125));
    assert!(messages(&deck).is_empty(), "bad settings must be visible without flooding every main-loop tick");
    service.opts.config.plugin_settings.remove("login-sync");
    tick(&mut service, start + Duration::from_secs(156));
    assert!(status(&service).is_ok());
}

#[test]
fn access_expiry_is_not_reported_as_full_login_expiry() {
    let dir = TempDir::new("login-expiry-kinds");
    let paths = LoginPaths { claude_dir: dir.path().join("claude"), codex_dir: dir.path().join("codex") };
    std::fs::create_dir_all(&paths.codex_dir).unwrap();
    let now = logins::now_ms();
    let claims = serde_json::json!({"exp":(now+71*3_600_000)/1000});
    let payload = crate::util::base64(serde_json::to_string(&claims).unwrap().as_bytes())
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_");
    let auth = serde_json::json!({"last_refresh":"2026-10-31T12:00:00Z","tokens":{"account_id":"fixture","refresh_token":"opaque","access_token":format!("e30.{payload}.signature")}});
    std::fs::write(paths.codex_dir.join("auth.json"), serde_json::to_vec(&auth).unwrap()).unwrap();
    let status = logins::health(&paths, &Default::default(), now).unwrap();
    let codex = &status["tools"][1];
    assert_eq!(codex["full_login_expiry_known"], false);
    assert!(codex["refresh_expires_ms"].is_null());
    assert!(codex["warning"].as_str().unwrap().contains("access token"));
    assert!(codex["warning"].as_str().unwrap().contains("not exposed"));
    let malformed = serde_json::json!({"tokens":{"access_token":"bad.!.signature","refresh_token":"opaque"}});
    assert!(logins::expiry_of(logins::LoginTool::Codex, &malformed).access_expires_ms.is_none());
    // A near access deadline must not obscure a renewable Claude session.
    let claude = serde_json::json!({"claudeAiOauth":{"expiresAt":now+1000,"refreshTokenExpiresAt":now+10*86_400_000}});
    assert_eq!(logins::expiry_of(logins::LoginTool::Claude, &claude).refresh_expires_ms, Some(now + 10 * 86_400_000));
}
