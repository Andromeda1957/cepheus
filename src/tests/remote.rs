use super::*;
use crate::protocol::{
    write_msg, Alert, AlertKind, ClientMsg, ClientRole, DeckState, ServerMsg, Status, Tool, PROTOCOL_VERSION,
};
use crate::remote::auth::Limiter;
use crate::remote::hub::{Hub, Options};
use crate::service::ServiceOptions;
use crate::testsupport::TempDir;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use serde_json::{json, Value};
use tungstenite::Message;

const PHONE: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

#[test]
fn a_phone_leaving_is_not_logged_as_a_fault() {
    use anyhow::Context as _;
    let eof = std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "peer closed connection without sending TLS close_notify",
    );
    let left: anyhow::Result<()> = Err(tungstenite::Error::Io(eof)).context("reading");
    assert!(server::dropped(&left.unwrap_err()));
    let reset = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset)).context("sending");
    assert!(server::dropped(&reset));
    let closed: anyhow::Result<()> = Err(tungstenite::Error::ConnectionClosed).context("reading");
    assert!(server::dropped(&closed.unwrap_err()));
    // Faults still are.
    let refused = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied)).context("writing");
    assert!(!server::dropped(&refused));
    assert!(!server::dropped(&anyhow::anyhow!("bad request: missing field `t`")));
}

#[test]
fn only_allowed_addresses_and_unlocked_ones_get_in() {
    let phone: IpAddr = "198.51.100.101".parse().unwrap();
    let peer: IpAddr = "198.51.100.55".parse().unwrap();
    let allow = hub::parse_allow(&["198.51.100.101".into(), "not an address".into()]);
    assert_eq!(allow, vec![phone]);
    assert!(auth::allowed(&allow, phone));
    assert!(auth::allowed(&allow, "::ffff:198.51.100.101".parse().unwrap()), "an IPv4-mapped peer is the same phone");
    assert!(!auth::allowed(&allow, peer), "another VPN peer");
    assert!(!auth::allowed(&hub::parse_allow(&[]), phone), "an empty list allows nothing");

    let mut limiter = Limiter::default();
    let t0 = Instant::now();
    for i in 0..4 {
        limiter.fail(phone, t0 + Duration::from_secs(i));
        assert!(limiter.open(phone, t0 + Duration::from_secs(i)));
    }
    limiter.fail(phone, t0 + Duration::from_secs(5));
    assert!(!limiter.open(phone, t0 + Duration::from_secs(6)), "five failures lock the address out");
    assert!(limiter.open(peer, t0 + Duration::from_secs(6)), "only that address");
    assert!(!limiter.open(phone, t0 + Duration::from_secs(5 + 899)));
    assert!(limiter.open(phone, t0 + Duration::from_secs(5 + 900)), "for fifteen minutes");
    // Failures spread over more than ten minutes never add up to a lockout.
    let mut slow = Limiter::default();
    for i in 0..10 {
        slow.fail(phone, t0 + Duration::from_secs(i * 200));
        assert!(slow.open(phone, t0 + Duration::from_secs(i * 200)));
    }
    // A success forgets the failures before it.
    let mut ok = Limiter::default();
    for i in 0..4 {
        ok.fail(phone, t0 + Duration::from_secs(i));
    }
    ok.succeed(phone);
    ok.fail(phone, t0 + Duration::from_secs(5));
    assert!(ok.open(phone, t0 + Duration::from_secs(5)));
}

#[test]
fn a_device_is_known_by_its_id_and_the_hash_of_its_key() {
    let (a, key_a) = auth::new_device("Pixel\u{7} 9\n", 100).unwrap();
    let (b, key_b) = auth::new_device("", 100).unwrap();
    assert_eq!(key_a.len(), 64);
    assert_ne!(key_a, key_b);
    assert_eq!(a.name, "Pixel 9", "control characters are dropped");
    assert_eq!(b.name, "Phone");
    assert_eq!(a.key_sha256, auth::sha256_hex(&auth::decode_hex(&key_a).unwrap()));
    assert!(!a.key_sha256.contains(&key_a));
    let devices = vec![a.clone(), b.clone()];
    assert_eq!(auth::find_device(&devices, &a.id, &key_a), Some(0));
    assert_eq!(auth::find_device(&devices, &b.id, &key_b), Some(1));
    assert_eq!(auth::find_device(&devices, &a.id, &key_b), None, "another phone's key");
    assert_eq!(auth::find_device(&devices, &a.id, &key_a.to_uppercase()), Some(0), "hex in either case");
    assert_eq!(auth::find_device(&devices, &a.id, &key_a[..62]), None);
    assert_eq!(auth::find_device(&devices, &a.id, "zz"), None);
    assert_eq!(auth::find_device(&[b], &a.id, &key_a), None, "a removed phone");
}

#[test]
fn a_pairing_code_works_once_before_it_expires() {
    let dir = TempDir::new("remote-pairing");
    let code = store::new_code().unwrap();
    assert_eq!(code.len(), store::CODE_LEN);
    assert!(code.chars().all(|c| "ABCDEFGHJKMNPQRSTUVWXYZ23456789".contains(c)), "{code}");
    assert_eq!(store::display_code("abcd2345"), "ABCD-2345");
    assert_eq!(store::normalize_code(" abcd-2345 "), "ABCD2345");

    assert!(auth::redeem_code(dir.path(), "ABCD2345", 1_000).is_err(), "no code is open");
    store::write_pairing(dir.path(), "ABCD2345", 1_000).unwrap();
    let file = fs::read_to_string(dir.path().join("pairing.json")).unwrap();
    assert!(!file.contains("ABCD"), "only the code's hash is kept");
    assert_eq!(fs::metadata(dir.path().join("pairing.json")).unwrap().permissions().mode() & 0o777, 0o600);
    auth::redeem_code(dir.path(), "abcd-2345", 1_000 + store::PAIRING_SECS - 1).unwrap();
    let again = auth::redeem_code(dir.path(), "ABCD2345", 1_001).unwrap_err();
    assert!(format!("{again}").contains("no pairing code"), "{again}");

    store::write_pairing(dir.path(), "ABCD2345", 1_000).unwrap();
    let late = auth::redeem_code(dir.path(), "ABCD2345", 1_000 + store::PAIRING_SECS).unwrap_err();
    assert!(format!("{late}").contains("expired"), "{late}");

    // A wrong guess uses the code up, so it cannot be followed by another.
    store::write_pairing(dir.path(), "ABCD2345", 1_000).unwrap();
    assert!(format!("{}", auth::redeem_code(dir.path(), "ABCD2346", 1_001).unwrap_err()).contains("wrong"));
    assert!(auth::redeem_code(dir.path(), "ABCD2345", 1_002).is_err());
}

#[test]
fn the_identity_and_device_list_persist_privately() {
    let dir = TempDir::new("remote-store");
    let remote = dir.path().join("remote");
    assert_eq!(store::fingerprint(&remote), None);
    let first = store::load_or_create_identity(&remote, "198.51.100.102").unwrap();
    let again = store::load_or_create_identity(&remote, "198.51.100.102").unwrap();
    assert_eq!(first.fingerprint, again.fingerprint, "made once, then loaded");
    assert_eq!(store::fingerprint(&remote).as_deref(), Some(first.fingerprint.as_str()));
    assert_eq!(first.fingerprint.len(), 64);
    for name in ["cert.der", "key.der"] {
        assert_eq!(fs::metadata(remote.join(name)).unwrap().permissions().mode() & 0o777, 0o600, "{name}");
    }
    assert_eq!(fs::metadata(&remote).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(server::tls_config(&first.cert_der, &first.key_der).is_ok());

    assert!(store::load_devices(&remote).is_empty());
    let (device, _) = auth::new_device("Pixel", 5).unwrap();
    store::save_devices(&remote, std::slice::from_ref(&device)).unwrap();
    assert_eq!(store::load_devices(&remote), vec![device]);
    assert_eq!(fs::metadata(remote.join("devices.json")).unwrap().permissions().mode() & 0o777, 0o600);
}

pub(super) fn hub_options(dir: &TempDir, allow: Vec<IpAddr>) -> Options {
    Options {
        socket: dir.path().join("run/deck.sock"),
        dir: dir.path().join("state/remote"),
        listen: "127.0.0.1:0".into(),
        apk_listen: String::new(),
        allow,
        claude_dir: dir.path().join("claude"),
        codex_dir: dir.path().join("codex"),
        opencode_db: dir.path().join("opencode.db"),
        antigravity_dir: dir.path().join("antigravity"),
        sqlite3: vec!["sqlite3".into()],
        order_file: dir.path().join("state/order.json"),
        uploads_dir: dir.path().join("state/uploads"),
        ping_every: Duration::from_secs(25),
        config_file: None,
        watch_exe: false,
        // Never exit the test process when its service stops.
        service_grace: Duration::from_secs(86_400),
        here: "ThinkPad".into(),
        hosts: Vec::new(),
        hub_key: None,
    }
}

fn alert(reason: &str) -> ServerMsg {
    ServerMsg::Alert(Alert {
        kind: AlertKind::Finished,
        id: Some(1),
        title: "Fix the build".into(),
        reason: reason.into(),
        desk_focused: false,
        message: None,
    })
}

/// The deck's state with session 1 of `status` since `since` (unix seconds).
fn one_session(status: Status, since: u64) -> ServerMsg {
    let mut session = crate::tui::tests::session(1, Tool::Codex, status, true);
    session.status_since = since;
    ServerMsg::State(DeckState { processes: Vec::new(), sessions: vec![session], ..Default::default() })
}

/// The alerts a phone that connects now is sent, by number.
fn replayed(hub: &Hub, last_seq: Option<u64>, epoch: Option<&str>) -> Vec<u64> {
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("d-1", tx, last_seq, epoch);
    hub.remove_conn(conn);
    rx.try_iter()
        .map(|t| serde_json::from_str::<Value>(&t).unwrap())
        .filter(|v| v["t"] == "alert")
        .map(|v| v["seq"].as_u64().unwrap())
        .collect()
}

#[test]
fn a_phone_that_was_away_is_sent_the_alerts_it_missed() {
    let dir = TempDir::new("remote-alerts");
    let hub = Hub::new(hub_options(&dir, vec![PHONE]), "epoch-a".into());
    hub.on_service(one_session(Status::Finished, util::now_secs()));
    for reason in ["one", "two", "three"] {
        hub.on_service(alert(reason));
    }
    assert_eq!(replayed(&hub, Some(1), Some("epoch-a")), [2, 3]);
    assert_eq!(replayed(&hub, Some(3), Some("epoch-a")), Vec::<u64>::new());
    assert_eq!(replayed(&hub, Some(3), Some("an older gateway")), [1, 2, 3], "numbers from another run mean nothing");
    assert_eq!(replayed(&hub, None, None), [1, 2, 3]);
}

#[test]
fn a_reconnecting_phone_is_not_told_again_of_a_turn_the_session_moved_on_from() {
    // Owner, 2026-11-07: "keep getting android notifications that agents are
    // finished when no agents are finished".
    let dir = TempDir::new("remote-stale-alerts");
    let hub = Hub::new(hub_options(&dir, vec![PHONE]), "epoch-a".into());
    let finished_at = util::now_secs();
    hub.on_service(one_session(Status::Finished, finished_at));
    hub.on_service(alert("done"));
    assert_eq!(replayed(&hub, None, None), [1], "still at that finished turn");
    // The owner sent it on: the turn the alert told of is over.
    hub.on_service(one_session(Status::Working, finished_at));
    assert_eq!(replayed(&hub, None, None), Vec::<u64>::new());
    // Finished again later is another turn, which has its own alert.
    hub.on_service(one_session(Status::Finished, finished_at + 60));
    assert_eq!(replayed(&hub, None, None), Vec::<u64>::new());
    // A question is replayed only while the session still asks it. The hub
    // stamps an alert when it arrives, so the question is asked now: a status
    // dated after its alert would be a later question than the one it told of.
    let asked_at = util::now_secs();
    hub.on_service(ServerMsg::Alert(Alert {
        kind: AlertKind::NeedsYou,
        id: Some(1),
        title: "Fix the build".into(),
        reason: "permission".into(),
        desk_focused: false,
        message: None,
    }));
    hub.on_service(one_session(Status::NeedsYou, asked_at));
    assert_eq!(replayed(&hub, None, None), [2]);
    hub.on_service(one_session(Status::Working, finished_at + 60));
    assert_eq!(replayed(&hub, None, None), Vec::<u64>::new());
    // A session gone, or a state not yet known, replays nothing of it.
    hub.on_service(ServerMsg::State(DeckState { processes: Vec::new(), ..Default::default() }));
    assert_eq!(replayed(&hub, None, None), Vec::<u64>::new());
}

#[test]
fn what_a_phone_missed_is_marked_replay_and_what_it_sees_live_is_not() {
    let dir = TempDir::new("remote-alert-replay");
    let hub = Hub::new(hub_options(&dir, vec![PHONE]), "epoch-a".into());
    hub.on_service(one_session(Status::Finished, util::now_secs()));
    hub.on_service(alert("before"));
    let (tx, rx) = mpsc::channel();
    let conn = hub.add_conn("d-1", tx, None, None);
    hub.on_service(alert("after"));
    hub.remove_conn(conn);
    let alerts: Vec<Value> =
        rx.try_iter().map(|t| serde_json::from_str::<Value>(&t).unwrap()).filter(|v| v["t"] == "alert").collect();
    assert_eq!(alerts.len(), 2);
    assert_eq!((&alerts[0]["reason"], &alerts[0]["replay"]), (&json!("before"), &json!(true)));
    assert_eq!(alerts[1]["reason"], "after");
    assert!(alerts[1].get("replay").is_none(), "a live alert is news: {}", alerts[1]);
}

// ---- end to end: a service, the gateway, and a phone over TLS ----

/// The phone's check: the certificate is the one whose SHA-256 the pairing
/// screen showed, whatever its name or issuer.
#[derive(Debug)]
struct Pin {
    sha256: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if auth::sha256_hex(end_entity.as_ref()) == self.sha256 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("not the pinned certificate".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

type PhoneWs = tungstenite::WebSocket<StreamOwned<ClientConnection, TcpStream>>;

fn phone(addr: SocketAddr, pin: &str, path: &str) -> Result<PhoneWs, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pin { sha256: pin.to_string(), provider }))
        .with_no_client_auth();
    let tcp = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let conn = ClientConnection::new(Arc::new(config), ServerName::try_from("cepheus").unwrap()).unwrap();
    let (ws, _) =
        tungstenite::client(format!("wss://cepheus{path}"), StreamOwned::new(conn, tcp)).map_err(|e| format!("{e}"))?;
    Ok(ws)
}

fn send(ws: &mut PhoneWs, value: Value) {
    ws.send(Message::text(value.to_string())).unwrap();
}

/// The next message `want` accepts, skipping others.
fn expect(ws: &mut PhoneWs, what: &str, want: impl Fn(&Value) -> bool) -> Value {
    expect_for(ws, what, Duration::from_secs(20), want)
}

fn expect_for(ws: &mut PhoneWs, what: &str, wait: Duration, want: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + wait;
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        match ws.read() {
            Ok(Message::Text(text)) => {
                let v: Value = serde_json::from_str(text.as_str()).unwrap();
                if want(&v) {
                    return v;
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("connection ended waiting for {what}: {e}"),
        }
    }
}

fn reply(ws: &mut PhoneWs, rid: u64) -> Value {
    expect(ws, &format!("the reply to {rid}"), |v| v["rid"] == rid)
}

/// Whether the gateway closes the connection, after its last message.
fn closed(ws: &mut PhoneWs) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match ws.read() {
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => return true,
        }
    }
    false
}

fn service_options(dir: &TempDir, script: &str) -> ServiceOptions {
    let mut config = Config {
        claude_command: vec!["sh".into(), "-c".into(), script.into()],
        codex_command: vec!["sh".into(), "-c".into(), script.into()],
        notifications: false,
        claude_hooks: false,
        codex_notify: false,
        palette_file: Some(dir.path().join("no-kitty.conf")),
        // Never the owner's runserver on port 8000, which is the owner's alone.
        dev_server_command: vec!["sh".into(), "-c".into(), "echo serving; exec sleep 30".into()],
        ..Config::default()
    };
    config.scheduler.probe = dir.calm_probe();
    ServiceOptions {
        socket: dir.path().join("run/deck.sock"),
        state_dir: dir.path().join("state"),
        config,
        claude_dir: dir.path().join("claude"),
        codex_dir: dir.path().join("codex"),
        antigravity_dir: dir.path().join("agy"),
        grok_home: dir.path().join("grok"),
        opencode_db: dir.path().join("opencode.db"),
        usage_file: dir.path().join("state/claude-usage.json"),
        exe: PathBuf::from("/opt/agent deck/cepheus"),
        start_dir: dir.path().to_path_buf(),
        monitor: false,
        signals: false,
        update: false,
        handover: None,
        config_file: None,
    }
}

/// A gateway on a loopback port in front of `hub`.
fn listen(hub: &Arc<Hub>, tls: &Arc<rustls::ServerConfig>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (hub, tls) = (Arc::clone(hub), Arc::clone(tls));
    thread::spawn(move || server::accept_all(&hub, &tls, listener));
    addr
}

fn transcript_line(kind: &str, text: &str) -> String {
    let record = match kind {
        "user" => json!({"type": "user", "message": {"role": "user", "content": text}}),
        _ => {
            json!({"type": "assistant", "message": {"model": "claude-opus-5-5", "content": [{"type": "text", "text": text}]}})
        }
    };
    format!("{record}\n")
}

#[test]
fn a_paired_phone_drives_the_deck_over_tls() {
    let dir = TempDir::new("remote-e2e");
    // After its first message the program asks a several-choice question,
    // drawn as Claude Code 2.1.314 draws one, once the next message comes.
    let script = r#"printf '\033[?2004h'; echo ready; read x; echo "got $x"; read q; printf 'Pick toppings?\n\n❯ 1. [ ] Cheese\n  2. [ ] Olives\n  3. [ ] Type something\n     Submit\n──────────\n  4. Chat about this\n\nEnter to select\n'; stty -echo; read y; read z; echo "picked $y|$z" | cat -v; read w"#;
    let opts = service_options(&dir, script);
    let socket = opts.socket.clone();
    let service = thread::spawn(move || crate::service::run(opts));

    let hub_opts = hub_options(&dir, vec![PHONE]);
    let remote_dir = hub_opts.dir.clone();
    let identity = store::load_or_create_identity(&remote_dir, "127.0.0.1").unwrap();
    let pin = identity.fingerprint.clone();
    let tls = server::tls_config(&identity.cert_der, &identity.key_der).unwrap();
    let hub = Arc::new(Hub::new(hub_opts, "epoch-e2e".into()));
    start_threads(&hub);
    let addr = listen(&hub, &tls);

    // Pairing: refused with no code open, then the code the deck shows,
    // which works once.
    let mut ws = phone(addr, &pin, "/v1").unwrap();
    send(&mut ws, json!({"t": "pair", "code": "ABCD2345", "name": "Pixel"}));
    let denied = expect(&mut ws, "denied", |v| v["t"] == "denied");
    assert!(denied["reason"].as_str().unwrap().contains("no pairing code"), "{denied}");
    assert!(closed(&mut ws));
    store::write_pairing(&remote_dir, "ABCD2345", util::now_secs()).unwrap();
    let mut ws = phone(addr, &pin, "/v1").unwrap();
    send(&mut ws, json!({"t": "pair", "code": "abcd-2345", "name": "Pixel"}));
    let paired = expect(&mut ws, "paired", |v| v["t"] == "paired");
    let (device, key) = (paired["device"].as_str().unwrap().to_string(), paired["key"].as_str().unwrap().to_string());
    let hello = expect(&mut ws, "hello", |v| v["t"] == "hello");
    assert_eq!(hello["epoch"], "epoch-e2e");
    let devices = fs::read_to_string(remote_dir.join("devices.json")).unwrap();
    assert!(devices.contains(&device) && !devices.contains(&key), "the key itself is never kept");
    drop(ws);
    let mut again = phone(addr, &pin, "/v1").unwrap();
    send(&mut again, json!({"t": "pair", "code": "ABCD2345", "name": "Other"}));
    expect(&mut again, "denied", |v| v["t"] == "denied");
    drop(again);

    // A paired phone signs in with its key and drives a session.
    let mut ws = phone(addr, &pin, "/v1").unwrap();
    send(&mut ws, json!({"t": "auth", "device": device, "key": key}));
    expect(&mut ws, "hello", |v| v["t"] == "hello");
    send(&mut ws, json!({"t": "ping", "rid": 1}));
    assert_eq!(reply(&mut ws, 1)["t"], "pong");
    send(&mut ws, json!({"t": "new", "rid": 2, "tool": "claude"}));
    let created = reply(&mut ws, 2);
    assert_eq!(created["t"], "ok", "{created}");
    let id = created["id"].as_u64().unwrap();
    let state = expect(&mut ws, "the session listed", |v| {
        v["t"] == "state"
            && v["sessions"].as_array().is_some_and(|s| s.iter().any(|s| s["id"] == id && s["alive"] == true))
    });
    assert_eq!(state["sessions"][0]["tool"], "claude");

    // Typed and submitted once the program is ready for it.
    let mut rid = 10;
    loop {
        rid += 1;
        send(&mut ws, json!({"t": "send", "rid": rid, "id": id, "text": "hello deck"}));
        let r = reply(&mut ws, rid);
        if r["t"] == "ok" {
            break;
        }
        assert!(r["error"].as_str().unwrap().contains("still starting") && rid < 60, "{r}");
        thread::sleep(Duration::from_millis(300));
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while !hub.screen(id).unwrap().0.iter().any(|l| l.contains("got hello deck")) {
        assert!(Instant::now() < deadline, "the message never arrived");
        thread::sleep(Duration::from_millis(50));
    }

    // The conversation reads as chat from the transcript, and grows.
    let conversation = hub.session(id).unwrap().conversation.unwrap();
    let transcript = dir.path().join(format!("claude/projects/-elsewhere/{conversation}.jsonl"));
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, transcript_line("user", "hello deck") + &transcript_line("assistant", "Hi there.")).unwrap();
    send(&mut ws, json!({"t": "open", "rid": 70, "id": id}));
    let page = reply(&mut ws, 70);
    assert_eq!(
        (page["t"].as_str(), page["source"].as_str(), page["total"].as_u64()),
        (Some("chat"), Some("log"), Some(2)),
        "{page}"
    );
    assert_eq!(page["items"][0], json!({"k": "user", "text": "hello deck"}));
    assert_eq!(page["model"], "claude-opus-5-5");
    let mut file = fs::OpenOptions::new().append(true).open(&transcript).unwrap();
    std::io::Write::write_all(&mut file, transcript_line("assistant", "Second.").as_bytes()).unwrap();
    let more = expect(&mut ws, "new items", |v| v["t"] == "items" && v["id"] == id);
    assert_eq!(
        (more["start"].as_u64(), more["total"].as_u64(), more["reset"].as_bool()),
        (Some(2), Some(3), Some(false))
    );
    assert_eq!(more["items"], json!([{"k": "assistant", "text": "Second."}]));

    // An answer goes through the service's check: no question waits here.
    send(&mut ws, json!({"t": "answer", "rid": 71, "id": id, "inputs": 0, "fp": "x", "choice": 0}));
    let refused = reply(&mut ws, 71);
    assert_eq!(refused["t"], "err", "{refused}");
    send(&mut ws, json!({"t": "send", "rid": 72, "id": 999, "text": "hi"}));
    assert_eq!(reply(&mut ws, 72)["error"], "no such session");

    // A several-choice question (owner, 2026-10-30: "the app doesnt let me
    // answer questions and if i try sending a message it says session
    // skipped"): its card carries the ticks and the Submit row, a message
    // meanwhile is refused in the phone's words, and the ticked options go
    // back as one answer.
    send(&mut ws, json!({"t": "send", "rid": 73, "id": id, "text": "ask me"}));
    assert_eq!(reply(&mut ws, 73)["t"], "ok");
    let pushed = expect(&mut ws, "the question", |v| v["t"] == "prompt" && v["id"] == id && v["prompt"].is_object());
    let card = &pushed["prompt"];
    assert_eq!((card["multi"].as_bool(), card["submit"].as_str()), (Some(true), Some("Submit")), "{card}");
    assert_eq!(card["options"][1], json!({"label": "Olives", "checked": false}));
    assert_eq!(card["options"][3], json!({"label": "Chat about this"}));
    send(&mut ws, json!({"t": "send", "rid": 74, "id": id, "text": "hello?"}));
    let refused = reply(&mut ws, 74);
    assert_eq!(refused["error"], "Claude is waiting on the question above: answer it first.", "{refused}");
    send(
        &mut ws,
        json!({"t": "answer", "rid": 75, "id": id, "inputs": pushed["inputs"], "fp": pushed["fp"], "choice": 0, "choices": [1]}),
    );
    let answered = reply(&mut ws, 75);
    assert_eq!(answered["t"], "ok", "{answered}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !screen_shows(&hub, id, "picked ^[[B|^[[B^[[B") {
        assert!(Instant::now() < deadline, "the answer never arrived: {:?}", hub.screen(id).unwrap().0);
        thread::sleep(Duration::from_millis(50));
    }
    ws.send(Message::text("{\"t\": \"format_disk\"}")).unwrap();
    let bad = expect(&mut ws, "bad request", |v| v["t"] == "err" && v["rid"].is_null());
    assert!(bad["error"].as_str().unwrap().starts_with("bad request"));

    // A continuously connected phone learns when old history leaves the
    // gateway buffer, without a reset or losing absolute indexes.
    for batch in 0..8 {
        let text: String =
            (0..500).map(|i| transcript_line("assistant", &format!("history {}", batch * 500 + i))).collect();
        std::io::Write::write_all(&mut file, text.as_bytes()).unwrap();
        let total: usize = 3 + (batch + 1) * 500;
        let delta =
            expect(&mut ws, "retained history frontier", |v| v["t"] == "items" && v["id"] == id && v["total"] == total);
        assert_eq!(delta["retained_from"], total.saturating_sub(3000));
        assert_eq!(delta["reset"], false);
    }
    send(&mut ws, json!({"t":"chat", "rid":76, "id":id, "before":1003, "limit":60}));
    let edge = reply(&mut ws, 76);
    assert_eq!(edge["retained_from"], 1003);
    assert_eq!(edge["start"], 1003);
    assert_eq!(edge["items"], json!([]));

    // Removing the phone in the deck ends its connection.
    store::save_devices(&remote_dir, &[]).unwrap();
    let gone = expect(&mut ws, "removed", |v| v["t"] == "denied");
    assert!(gone["reason"].as_str().unwrap().contains("removed"), "{gone}");
    assert!(closed(&mut ws));
    let mut removed = phone(addr, &pin, "/v1").unwrap();
    send(&mut removed, json!({"t": "auth", "device": device, "key": key}));
    expect(&mut removed, "denied", |v| v["t"] == "denied");
    drop(removed);

    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

/// The phone drops a session's question card on any state that does not
/// show the session waiting (`Deck.onState`), but the gateway sent a card
/// again only when it changed, and it scans for questions every 700 ms while
/// states go out at once. A status that left "needs you" and came back
/// between two scans left the phone with no card while the gateway refused
/// its messages for the question (owner, 2026-11-02: "when you have a
/// question. it gets lost after more output and doesnt let me send messages
/// after").
#[test]
fn a_question_card_the_phone_dropped_is_sent_again() {
    use crate::protocol::{DeckState, Status};
    let dir = TempDir::new("remote-card-again");
    let opts = service_options(
        &dir,
        r#"printf '\033[?2004h'; echo ready; read x; printf 'Remove it?\n\n❯ 1. Yes\n  2. No\n\nEnter to select\n'; read y"#,
    );
    let socket = opts.socket.clone();
    let service = thread::spawn(move || crate::service::run(opts));
    let (hub, mut ws) = paired_phone(hub_options(&dir, vec![PHONE]));
    expect(&mut ws, "the connected service", |v| v["t"] == "state");
    send(&mut ws, json!({"t": "new", "rid": 1, "tool": "claude"}));
    let id = reply(&mut ws, 1)["id"].as_u64().unwrap();
    wait_for_claude_session_state(&hub, id);
    let mut rid = 10;
    loop {
        rid += 1;
        send(&mut ws, json!({"t": "send", "rid": rid, "id": id, "text": "go"}));
        let r = reply(&mut ws, rid);
        if r["t"] == "ok" {
            break;
        }
        assert!(r["error"].as_str().unwrap().contains("still starting") && rid < 60, "{r}");
        thread::sleep(Duration::from_millis(300));
    }
    expect(&mut ws, "the question", |v| v["t"] == "prompt" && v["id"] == id && v["prompt"].is_object());

    let asking = hub.session(id).unwrap();
    assert_eq!(asking.status, Status::NeedsYou);
    let mut working = asking.clone();
    working.status = Status::Working;
    hub.on_service(ServerMsg::State(DeckState {
        processes: Vec::new(),
        sessions: vec![working],
        ..Default::default()
    }));
    hub.on_service(ServerMsg::State(DeckState { processes: Vec::new(), sessions: vec![asking], ..Default::default() }));
    let again = expect(&mut ws, "the question again", |v| v["t"] == "prompt" && v["id"] == id);
    assert!(again["prompt"].is_object(), "{again}");

    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

/// A gateway for `hub_opts` with a phone paired through it, signed in.
fn paired_phone(hub_opts: Options) -> (Arc<Hub>, PhoneWs) {
    let remote_dir = hub_opts.dir.clone();
    let identity = store::load_or_create_identity(&remote_dir, "127.0.0.1").unwrap();
    let tls = server::tls_config(&identity.cert_der, &identity.key_der).unwrap();
    let hub = Arc::new(Hub::new(hub_opts, "epoch-order".into()));
    start_threads(&hub);
    let addr = listen(&hub, &tls);
    store::write_pairing(&remote_dir, "ABCD2345", util::now_secs()).unwrap();
    let mut ws = phone(addr, &identity.fingerprint, "/v1").unwrap();
    send(&mut ws, json!({"t": "pair", "code": "ABCD2345", "name": "Pixel"}));
    expect(&mut ws, "paired", |v| v["t"] == "paired");
    expect(&mut ws, "hello", |v| v["t"] == "hello");
    (hub, ws)
}

// Created owns an allocation reply; the independent gateway bridge publishes
// the authoritative session. reply() may already have consumed its state frame.
fn wait_for_claude_session_state(hub: &Hub, id: u64) {
    let deadline = Instant::now() + crate::testsupport::scaled_timeout(Duration::from_secs(20));
    loop {
        if let Some(session) = hub.session(id) {
            assert_eq!(session.tool, crate::protocol::Tool::Claude);
            assert!(session.alive, "new Claude session {id} ended before use");
            return;
        }
        assert!(Instant::now() < deadline, "new session {id} never appeared in authoritative state");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_unlinked_codex_phone_follows_live_output_then_its_linked_chat() {
    let dir = TempDir::new("remote-unlinked-codex");
    let opts = service_options(
        &dir,
        "printf '\\033[?2004h'; echo 'working on this session'; read x; echo \"progress: $x\"; read y",
    );
    let socket = opts.socket.clone();
    let service = thread::spawn(move || crate::service::run(opts));
    let (hub, mut ws) = paired_phone(hub_options(&dir, vec![PHONE]));
    send(&mut ws, json!({"t": "new", "rid": 1, "tool": "codex"}));
    let id = reply(&mut ws, 1)["id"].as_u64().unwrap();
    expect(&mut ws, "the unlinked session", |v| {
        v["t"] == "state" && v["sessions"].as_array().is_some_and(|s| s.iter().any(|s| s["id"] == id))
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !screen_shows(&hub, id, "working on this session") {
        assert!(Instant::now() < deadline, "the program did not start");
        thread::sleep(Duration::from_millis(30));
    }
    assert!(hub.session(id).unwrap().conversation.is_none());
    send(&mut ws, json!({"t": "open", "rid": 2, "id": id}));
    let page = reply(&mut ws, 2);
    assert_eq!(page["source"], "screen", "{page}");
    assert!(page["items"][0]["text"].as_str().unwrap().contains("working on this session"));
    send(&mut ws, json!({"t": "send", "rid": 3, "id": id, "text": "one more step"}));
    assert_eq!(reply(&mut ws, 3)["t"], "ok");
    let live = expect(&mut ws, "live output after opening", |v| {
        v["t"] == "items" && v["items"][0]["text"].as_str().is_some_and(|s| s.contains("progress: one more step"))
    });
    assert_eq!(live["reset"], true);

    // A late exact association replaces the screen with native chat. The
    // gateway never guesses one of the other sessions' conversations.
    let conversation = "11111111-1111-4111-8111-111111111111";
    let path = dir.path().join(format!("codex/sessions/2026/10/30/rollout-x-{conversation}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let meta = json!({"type": "session_meta", "payload": {"id": conversation, "cwd": dir.path(), "timestamp": "2026-10-30T23:00:00Z"}});
    fs::write(&path, format!("{meta}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"agent_message\",\"message\":\"Linked answer.\"}}}}\n")).unwrap();
    let mut control = UnixStream::connect(&socket).unwrap();
    write_msg(&mut control, &ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Hook }).unwrap();
    write_msg(
        &mut control,
        &ClientMsg::Hook {
            session: id,
            tool: crate::protocol::Tool::Codex,
            payload: json!({"type": "agent-turn-complete", "thread-id": conversation}),
        },
    )
    .unwrap();
    let linked = expect(&mut ws, "linked native chat", |v| v["t"] == "items" && v["items"][0]["k"] == "assistant");
    assert_eq!(linked["reset"], true);
    assert_eq!(linked["start"], 0);
    assert_eq!(linked["items"], json!([{"k":"assistant","text":"Linked answer."}]));

    // Losing the log returns to the same screen. It must be sent even
    // though its contents are identical to the last fallback snapshot.
    fs::remove_file(path).unwrap();
    let fallback = expect(&mut ws, "fallback after the rollout disappears", |v| {
        v["t"] == "items" && v["items"][0]["k"] == "screen"
    });
    assert!(fallback["items"][0]["text"].as_str().unwrap().contains("progress: one more step"));
    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

fn listed_first(ws: &mut PhoneWs, what: &str, id: u64) -> Value {
    expect(ws, what, |v| v["t"] == "state" && v["sessions"][0]["id"] == id && v["sessions"][1]["id"].is_u64())
}

/// Owner, 2026-10-27: "the sessions grouping should reflect the deck, they
/// dont, also allowing me to hold drag and reorder them", and "a stats
/// button on the side panel for agent usage, and the cpu/ram/ssd stats".
#[test]
fn the_phone_shares_the_decks_order_and_reads_the_stats() {
    let dir = TempDir::new("remote-order");
    let opts = service_options(&dir, "echo ready; read x");
    let socket = opts.socket.clone();
    let service = thread::spawn(move || crate::service::run(opts));
    let hub_opts = hub_options(&dir, vec![PHONE]);
    let order_file = hub_opts.order_file.clone();
    let (_hub, mut ws) = paired_phone(hub_opts);

    let mut ids = Vec::new();
    for rid in [2, 3] {
        send(&mut ws, json!({"t": "new", "rid": rid, "tool": "claude"}));
        let created = reply(&mut ws, rid);
        assert_eq!(created["t"], "ok", "{created}");
        ids.push(created["id"].as_u64().unwrap());
    }
    let (a, b) = (ids[0], ids[1]);
    let state = listed_first(&mut ws, "both sessions listed", a);
    assert!(state["outside"].is_array(), "the deck's Outside group comes along: {state}");

    // The deck moves b up (Super+↑): the phone follows without any other change.
    util::write_atomic(&order_file, &serde_json::to_vec(&[b, a]).unwrap()).unwrap();
    listed_first(&mut ws, "the deck's order", b);

    // The phone drags a back up: the deck's order file says so too.
    send(&mut ws, json!({"t": "reorder", "rid": 5, "ids": [a, b]}));
    assert_eq!(reply(&mut ws, 5)["t"], "ok");
    let saved: Vec<u64> = serde_json::from_slice(&fs::read(&order_file).unwrap()).unwrap();
    assert_eq!(saved, vec![a, b]);
    listed_first(&mut ws, "the phone's order", a);
    send(&mut ws, json!({"t": "reorder", "rid": 6, "ids": [a, 999]}));
    assert_eq!(reply(&mut ws, 6)["error"], "no such session or session order is too large");

    // Stats: the agents' usage from the service and this machine's own use.
    send(&mut ws, json!({"t": "stats", "rid": 7}));
    let stats = reply(&mut ws, 7);
    assert_eq!(stats["t"], "stats", "{stats}");
    assert!(stats["usage"]["claude"].is_array() && stats["usage"]["codex"].is_array(), "{stats}");
    let memory = stats["memory"].as_f64().expect("memory in use");
    assert!((0.0..=100.0).contains(&memory), "{stats}");
    assert!(stats["disk"]["used"].as_u64().is_some_and(|u| u > 0), "{stats}");
    assert!(stats["disk"]["available"].is_u64(), "{stats}");
    assert!(stats.get("cpu").is_some(), "cpu, or null before a second reading: {stats}");

    send(&mut ws, json!({"t": "commands", "rid": 10, "id": a}));
    let catalog = reply(&mut ws, 10);
    assert_eq!(catalog["t"], "commands", "{catalog}");
    assert_eq!(catalog["id"], a);
    assert!(catalog["entries"].as_array().unwrap().iter().any(|e| e["insert"] == "/model"));
    send(&mut ws, json!({"t": "commands", "rid": 11, "id": 999}));
    assert_eq!(reply(&mut ws, 11)["error"], "no such session");

    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

#[test]
fn the_phone_lists_earlier_conversations_and_resumes_one() {
    let dir = TempDir::new("remote-resume");
    let opts = service_options(&dir, "echo ready; read x");
    let start = opts.start_dir.to_string_lossy().into_owned();
    let conversation = "c0ffee00-0000-4000-8000-000000000001";
    dir.write(
        &format!("claude/projects/{}/{conversation}.jsonl", crate::claude::project_slug(&start)),
        transcript_line("user", "Tidy the parser") + &transcript_line("assistant", "Done."),
    );
    let socket = opts.socket.clone();
    let service = thread::spawn(move || crate::service::run(opts));
    let (_hub, mut ws) = paired_phone(hub_options(&dir, vec![PHONE]));

    send(&mut ws, json!({"t": "conversations", "rid": 2}));
    let listed = reply(&mut ws, 2);
    assert_eq!(listed["t"], "conversations", "{listed}");
    let item = &listed["items"][0];
    assert_eq!(
        (item["tool"].as_str(), item["id"].as_str(), item["title"].as_str()),
        (Some("claude"), Some(conversation), Some("Tidy the parser")),
        "{listed}"
    );
    assert!(item["open_in"].is_null() && item["updated_at"].is_u64(), "{item}");

    send(&mut ws, json!({"t": "resume", "rid": 3, "tool": "claude", "conversation": conversation}));
    let resumed = reply(&mut ws, 3);
    assert_eq!(resumed["t"], "ok", "{resumed}");
    let id = resumed["id"].as_u64().unwrap();
    expect(&mut ws, "the resumed session listed", |v| {
        v["t"] == "state"
            && v["sessions"].as_array().is_some_and(|s| s.iter().any(|s| s["id"] == id && s["alive"] == true))
    });
    // Now the list marks it open, and resuming it again shows that session.
    send(&mut ws, json!({"t": "conversations", "rid": 4}));
    assert_eq!(reply(&mut ws, 4)["items"][0]["open_in"], id);
    send(&mut ws, json!({"t": "resume", "rid": 5, "tool": "claude", "conversation": conversation}));
    assert_eq!(reply(&mut ws, 5)["id"], id);

    // The id becomes part of the program's command line: never a flag.
    send(&mut ws, json!({"t": "resume", "rid": 6, "tool": "claude", "conversation": "--dangerously-skip-permissions"}));
    assert_eq!(reply(&mut ws, 6)["error"], "invalid conversation id");
    send(&mut ws, json!({"t": "resume", "rid": 7, "tool": "claude", "conversation": "a b"}));
    assert_eq!(reply(&mut ws, 7)["error"], "invalid conversation id");

    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

/// The owner's gateway log, 2026-10-27, over and over: "connection from
/// 198.51.100.101: no word from the phone for 90 seconds". A sleeping phone's own
/// timers stop, so its pings stop; the gateway's pings wake its network stack,
/// which answers on its own.
#[test]
fn the_gateway_pings_so_a_sleeping_phone_stays_connected() {
    let dir = TempDir::new("remote-ping");
    let mut hub_opts = hub_options(&dir, vec![PHONE]);
    hub_opts.ping_every = Duration::from_millis(200);
    let (hub, mut ws) = paired_phone(hub_opts);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pings = 0;
    while pings < 2 {
        assert!(Instant::now() < deadline, "the gateway never pinged");
        match ws.read() {
            Ok(Message::Ping(_)) => pings += 1,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("the connection ended: {e}"),
        }
    }
    assert_eq!(hub.connections(), 1);
}

/// A piece of an upload as the phone frames it: the header's length, the
/// header, then the bytes.
fn chunk(ws: &mut PhoneWs, rid: u64, upload: &str, offset: usize, data: &[u8]) {
    let header = json!({"t": "chunk", "rid": rid, "upload": upload, "offset": offset}).to_string();
    let mut frame = (header.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(data);
    ws.send(Message::binary(frame)).unwrap();
}

fn upload(ws: &mut PhoneWs, rid: u64, id: u64, name: &str, data: &[u8]) -> Value {
    let sha = crate::remote::auth::sha256_hex(data);
    send(ws, json!({"t": "upload", "rid": rid, "id": id, "name": name, "size": data.len(), "sha256": sha}));
    reply(ws, rid)
}

fn screen_shows(hub: &Hub, id: u64, text: &str) -> bool {
    hub.screen(id).unwrap().0.concat().contains(text)
}

/// Owner, 2026-10-27: "in the apk, i need a way to upload files like i can in
/// claude etc".
#[test]
fn a_phone_attaches_files_to_a_message() {
    let dir = TempDir::new("remote-upload");
    let opts = service_options(&dir, r#"printf '\033[?2004h'; echo ready; exec cat"#);
    let socket = opts.socket.clone();
    let service = thread::spawn(move || crate::service::run(opts));
    let hub_opts = hub_options(&dir, vec![PHONE]);
    let uploads = hub_opts.uploads_dir.clone();
    let (hub, mut ws) = paired_phone(hub_opts);
    // Pairing can finish before the gateway's asynchronous service
    // connection. A state message proves it can accept the new session.
    expect(&mut ws, "the connected service", |v| v["t"] == "state");
    send(&mut ws, json!({"t": "new", "rid": 2, "tool": "claude"}));
    let created = reply(&mut ws, 2);
    let id = created["id"].as_u64().unwrap_or_else(|| panic!("a new session: {created}"));
    wait_for_claude_session_state(&hub, id);

    // A file in two pieces, named so it cannot leave its folder.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let started = upload(&mut ws, 3, id, "../My shot (1).png", &data);
    let token = started["upload"].as_str().unwrap_or_else(|| panic!("an upload id: {started}")).to_string();
    chunk(&mut ws, 4, &token, 0, &data[..200_000]);
    assert_eq!(reply(&mut ws, 4)["t"], "ok");
    chunk(&mut ws, 5, &token, 0, &data[200_000..]);
    assert!(reply(&mut ws, 5)["error"].as_str().unwrap().contains("offset"), "a piece out of place");
    chunk(&mut ws, 6, &token, 200_000, &data[200_000..]);
    let done = reply(&mut ws, 6);
    let path = PathBuf::from(done["path"].as_str().expect("the stored file"));
    assert_eq!(path, uploads.join(id.to_string()).join("My_shot_1_.png"));
    assert_eq!(fs::read(&path).unwrap(), data);
    let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    // The same name again gets a name of its own.
    let again = upload(&mut ws, 7, id, "My shot (1).png", b"second");
    chunk(&mut ws, 8, again["upload"].as_str().unwrap(), 0, b"second");
    assert!(reply(&mut ws, 8)["path"].as_str().unwrap().ends_with("/My_shot_1_-2.png"));

    // Bytes that do not match the checksum are dropped.
    let sha = crate::remote::auth::sha256_hex(b"good");
    send(&mut ws, json!({"t": "upload", "rid": 9, "id": id, "name": "bad.txt", "size": 4, "sha256": sha}));
    let bad = reply(&mut ws, 9)["upload"].as_str().unwrap().to_string();
    chunk(&mut ws, 10, &bad, 0, b"evil");
    assert!(reply(&mut ws, 10)["error"].as_str().unwrap().contains("checksum"));
    let stored: Vec<String> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(stored.len(), 2, "no bad file and no leftover piece: {stored:?}");
    // Too big, or for no session, is refused before any byte.
    send(
        &mut ws,
        json!({"t": "upload", "rid": 11, "id": id, "name": "big.bin", "size": (25u64 << 20) + 1, "sha256": sha}),
    );
    assert!(reply(&mut ws, 11)["error"].as_str().unwrap().contains("25 MB"));
    send(&mut ws, json!({"t": "upload", "rid": 12, "id": 999, "name": "a.txt", "size": 4, "sha256": sha}));
    assert_eq!(reply(&mut ws, 12)["error"], "no such session");

    // A message naming a file that is not all there types nothing.
    send(&mut ws, json!({"t": "send", "rid": 13, "id": id, "text": "never typed", "files": [token, bad]}));
    assert!(reply(&mut ws, 13)["error"].as_str().unwrap().contains("upload"));
    // Claude takes a file through its own prompt interface, by the path the
    // host checked, never as typed input. This stand-in loads no plugin, so
    // no listener takes the message by its deadline and nothing is sent;
    // neither its text nor a path is typed.
    let mut rid = 20;
    let refused = loop {
        rid += 1;
        send(&mut ws, json!({"t": "send", "rid": rid, "id": id, "text": "look at this", "files": [token]}));
        let r = expect_for(&mut ws, &format!("the reply to {rid}"), Duration::from_secs(60), |v| v["rid"] == rid);
        let error = r["error"].as_str().unwrap_or_else(|| panic!("{r}")).to_string();
        if !error.contains("still starting") {
            break error;
        }
        assert!(rid < 80, "{r}");
        thread::sleep(Duration::from_millis(300));
    };
    assert!(
        refused.ends_with("(Claude's native owner input is not connected in this session; nothing was sent)"),
        "{refused}"
    );
    assert!(!screen_shows(&hub, id, "look at this") && !screen_shows(&hub, id, path.to_str().unwrap()));
    assert!(!screen_shows(&hub, id, "never typed"));

    // Files go after seven days, and with their session.
    let week_old = std::time::SystemTime::now() - Duration::from_secs(8 * 86_400);
    fs::File::options().write(true).open(&path).unwrap().set_modified(week_old).unwrap();
    let gone_session = uploads.join("999");
    fs::create_dir_all(&gone_session).unwrap();
    fs::write(gone_session.join("left.txt"), "x").unwrap();
    hub.clean_uploads(true);
    assert!(!path.exists(), "a week-old file is removed");
    assert!(uploads.join(id.to_string()).join("My_shot_1_-2.png").exists(), "a new one stays");
    assert!(!gone_session.exists(), "a removed session's files go with it");

    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

/// Owner, 2026-11-03: "the app from the staging branch doesnt let me
/// interrupt to send messages now like claude code works". The phone's Send
/// now under a queued message presses Claude Code's send-now key
/// (`ctrl+x ctrl+s`) after the message's Enter.
#[test]
fn a_phone_sends_its_queued_message_to_claude_now() {
    let dir = TempDir::new("remote-send-now");
    // The program shows every byte it reads, control keys as `^X`.
    let opts = service_options(&dir, r#"printf '\033[?2004h'; echo ready; stty raw -echo; exec cat -v"#);
    let socket = opts.socket.clone();
    let service = thread::spawn(move || crate::service::run(opts));
    let (hub, mut ws) = paired_phone(hub_options(&dir, vec![PHONE]));
    expect(&mut ws, "the connected service", |v| v["t"] == "state");
    send(&mut ws, json!({"t": "new", "rid": 2, "tool": "claude"}));
    let claude = reply(&mut ws, 2)["id"].as_u64().expect("a Claude session");
    send(&mut ws, json!({"t": "new", "rid": 3, "tool": "codex"}));
    let codex = reply(&mut ws, 3)["id"].as_u64().expect("a Codex session");

    let mut rid = 10;
    loop {
        rid += 1;
        send(&mut ws, json!({"t": "send", "rid": rid, "id": claude, "text": "steer now"}));
        let r = reply(&mut ws, rid);
        if r["t"] == "ok" {
            break;
        }
        assert!(r["error"].as_str().unwrap().contains("still starting") && rid < 80, "{r}");
        thread::sleep(Duration::from_millis(300));
    }
    // Tapped at once, while the message's Enter is still due.
    send(&mut ws, json!({"t": "send_now", "rid": 90, "id": claude}));
    let sent = reply(&mut ws, 90);
    assert_eq!(sent["t"], "ok", "{sent}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !screen_shows(&hub, claude, "steer now^[[201~^M^X^S") {
        assert!(Instant::now() < deadline, "no Enter then send-now key: {:?}", hub.screen(claude).unwrap().0);
        thread::sleep(Duration::from_millis(50));
    }

    // Codex has no such key; an unknown session is refused as for a message.
    send(&mut ws, json!({"t": "send_now", "rid": 91, "id": codex}));
    assert_eq!(reply(&mut ws, 91)["error"], "only Claude Code sends a queued message now");
    send(&mut ws, json!({"t": "send_now", "rid": 92, "id": 999}));
    assert_eq!(reply(&mut ws, 92)["error"], "no such session");

    crate::testsupport::shutdown_service(&socket).unwrap();
    service.join().unwrap().unwrap();
}

#[test]
fn the_gateway_refuses_strangers_before_they_reach_a_session() {
    let dir = TempDir::new("remote-refuse");
    let hub_opts = hub_options(&dir, vec![PHONE]);
    let remote_dir = hub_opts.dir.clone();
    let identity = store::load_or_create_identity(&remote_dir, "127.0.0.1").unwrap();
    let tls = server::tls_config(&identity.cert_der, &identity.key_der).unwrap();
    let hub = Arc::new(Hub::new(hub_opts, "e".into()));
    let addr = listen(&hub, &tls);
    let (device, _key) = auth::new_device("Pixel", 1).unwrap();
    hub.add_device(device.clone()).unwrap();

    // The phone checks the certificate against the pairing screen's pin.
    let other = auth::sha256_hex(b"another certificate");
    assert!(phone(addr, &other, "/v1").is_err(), "a certificate that is not the pinned one");
    assert_eq!(store::fingerprint(&remote_dir).as_deref(), Some(identity.fingerprint.as_str()));
    // Only the one path.
    let err = phone(addr, &identity.fingerprint, "/").map(|_| ()).unwrap_err();
    assert!(err.contains("404"), "{err}");

    // Five wrong keys lock the address out: the next connection, whatever key
    // it would send, ends before TLS.
    let wrong = "00".repeat(32);
    for _ in 0..5 {
        let mut ws = phone(addr, &identity.fingerprint, "/v1").unwrap();
        send(&mut ws, json!({"t": "auth", "device": device.id, "key": wrong}));
        let denied = expect(&mut ws, "denied", |v| v["t"] == "denied");
        assert!(denied["reason"].as_str().unwrap().contains("unknown phone"), "{denied}");
    }
    assert!(phone(addr, &identity.fingerprint, "/v1").is_err(), "locked out before TLS");
    assert!(!hub.limiter_open(PHONE));

    // Another VPN peer never gets as far as TLS.
    let elsewhere = Arc::new(Hub::new(hub_options(&dir, vec!["198.51.100.101".parse().unwrap()]), "e".into()));
    elsewhere.add_device(device).unwrap();
    let addr = listen(&elsewhere, &tls);
    assert!(phone(addr, &identity.fingerprint, "/v1").is_err());
}

#[test]
fn phone_can_request_a_sessions_complete_command_catalog() {
    let request = serde_json::from_str::<crate::remote::wire::Req>(r#"{"t":"commands","rid":42,"id":7}"#);
    assert!(request.is_ok(), "the phone needs a per-session command catalog: {request:?}");
    assert_eq!(request.unwrap().rid(), 42);
}

/// The next message on `rx` answering request `rid`.
fn answer(rx: &mpsc::Receiver<String>, rid: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(text) = rx.recv_timeout(left) else { break };
        let v: Value = serde_json::from_str(&text).unwrap();
        if v["rid"] == rid {
            return v;
        }
    }
    panic!("no answer to request {rid}");
}

fn last_state(rx: &mpsc::Receiver<String>) -> Value {
    rx.try_iter()
        .map(|t| serde_json::from_str::<Value>(&t).unwrap())
        .filter(|v| v["t"] == "state")
        .last()
        .expect("a state")
}

/// Owner, 2026-10-31: the phone is included ("Yes, include the phone"):
/// a session on another host is listed under it, reads as chat from that
/// host's own log, offers that host's skills, and the Stats screen has a
/// row for each host.
/// A file the phone attaches to a session on another host is kept there,
/// under its own name, and week-old ones are cleared out.
#[test]
fn a_hosts_copy_of_an_attached_file_keeps_its_name_and_old_ones_go() {
    let dir = TempDir::new("remote-host-files");
    let kept = dir.path().join("uploads");
    let first = crate::hosts::peer::keep_upload(&kept, "photo 1.jpg", b"one").unwrap();
    let second = crate::hosts::peer::keep_upload(&kept, "photo 1.jpg", b"two").unwrap();
    assert_ne!(first, second, "a taken name is not overwritten");
    assert_eq!((fs::read(&first).unwrap(), fs::read(&second).unwrap()), (b"one".to_vec(), b"two".to_vec()));
    let week_old = std::time::SystemTime::now() - Duration::from_secs(8 * 86_400);
    fs::File::options().write(true).open(&first).unwrap().set_modified(week_old).unwrap();
    crate::hosts::peer::keep_upload(&kept, "notes.txt", b"three").unwrap();
    assert!(!first.exists() && second.exists());
}

#[path = "remote_controls.rs"]
mod controls;

#[path = "remote_live.rs"]
mod live;

#[path = "remote_peer_chat.rs"]
mod peer_chat;
