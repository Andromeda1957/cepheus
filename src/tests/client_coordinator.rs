use super::*;

use std::fs;
use std::os::unix::net::UnixListener;
use std::thread;

use crate::hosts::logins::LoginPaths;
use crate::testsupport::{loopback_peer, TempDir};

#[test]
fn a_coordinator_selector_cannot_replace_enrolled_endpoint_trust() {
    let dir = TempDir::new("coordinator-selector");
    let config_path = dir.path().join("config.json");
    let mut config = Config::default();
    config.hosts.push(HostConfig {
        role: Default::default(),
        name: "coordinator".into(),
        label: None,
        tag: 1,
        addresses: vec!["127.0.0.1:8766".into()],
        ssh: None,
        fingerprint: "a".repeat(64),
    });
    select(&config_path, &config, Some("coordinator")).unwrap();
    let name = declared_host(&config_path).unwrap().unwrap();
    assert_eq!(enrolled(&config, &name).unwrap(), config.hosts[0]);
    assert!(select(&config_path, &config, Some("untrusted")).is_err());
    fs::write(declaration_path(&config_path), br#"{"host":"coordinator","addresses":["untrusted:8766"]}"#).unwrap();
    assert!(declared_host(&config_path).is_err(), "the selector cannot supply a different endpoint");
    fs::write(declaration_path(&config_path), b"{}").unwrap();
    assert!(declared_host(&config_path).is_err(), "a missing selector cannot elect a local coordinator");
    fs::write(declaration_path(&config_path), b"{").unwrap();
    assert!(declared_host(&config_path).is_err(), "a broken declaration cannot select a local fallback");
}

#[test]
fn the_shared_connection_reaches_the_pinned_coordinators_registry() {
    let dir = TempDir::new("coordinator-route");
    let state = dir.path().join("viewer");
    let socket = dir.path().join("registry.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (host, _) = loopback_peer(
        dir.path().join("coordinator/peer"),
        &state,
        dir.path().to_path_buf(),
        LoginPaths { claude_dir: dir.path().join("claude"), codex_dir: dir.path().join("codex") },
        Some(socket),
    );
    let authoritative = thread::spawn(move || {
        let (mut service, _) = listener.accept().unwrap();
        let message: String = read_msg(&mut service).unwrap().unwrap();
        assert_eq!(message, "public session registry");
        write_msg(&mut service, &"coordinator session 42".to_string()).unwrap();
    });
    let mut viewer = connect(&host, &state).unwrap();
    viewer.set_read_timeout(Some(crate::testsupport::scaled_timeout(hosts::tls::CONNECT_TIMEOUT))).unwrap();
    write_msg(&mut viewer, &"public session registry".to_string()).unwrap();
    assert_eq!(read_msg::<_, String>(&mut viewer).unwrap().unwrap(), "coordinator session 42");
    authoritative.join().unwrap();
    let mut wrong_pin = host;
    wrong_pin.fingerprint = "0".repeat(64);
    assert!(connect(&wrong_pin, &state).is_err(), "an unpinned coordinator cannot receive the viewer key");
}

#[test]
fn authenticated_provisioning_installs_the_reverse_route_without_replacing_trust() {
    use crate::hosts::peer::{self, Peer};
    use crate::remote::store;
    use std::net::TcpListener;
    use std::sync::Arc;
    let dir = TempDir::new("coordinator-provision");
    let state = dir.path().join("coordinator");
    let viewer_state = dir.path().join("viewer");
    let viewer_config = viewer_state.join("config/config.json");
    fs::create_dir_all(viewer_config.parent().unwrap()).unwrap();
    fs::write(&viewer_config, b"{}").unwrap();
    let own_identity = store::load_or_create_identity(&peer::peer_dir(&state), "coordinator").unwrap();
    let viewer_dir = peer::peer_dir(&viewer_state);
    let identity = store::load_or_create_identity(&viewer_dir, "viewer").unwrap();
    let key = hosts::hub_key(&state).unwrap();
    peer::record_hub_key(&viewer_dir, &key).unwrap();
    let tls = crate::remote::server::tls_config(&identity.cert_der, &identity.key_der).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let service = Arc::new(
        Peer::new(
            viewer_dir,
            hosts::parse_cidrs(&["127.0.0.1".into()]),
            "Viewer".into(),
            dir.path().to_path_buf(),
            None,
        )
        .with_config_file(viewer_config.clone()),
    );
    thread::spawn(move || peer::accept_all(&service, &tls, listener));
    let viewer = HostConfig {
        role: Default::default(),
        name: "viewer".into(),
        label: None,
        tag: 1,
        addresses: vec![address],
        ssh: None,
        fingerprint: identity.fingerprint,
    };
    let config =
        Config { host_name: Some("Coordinator".into()), peer_listen: "127.0.0.1:8766".into(), ..Config::default() };
    provision_viewer(&config, &viewer, &state).unwrap();
    let (installed, error) = Config::load_from(&viewer_config);
    assert!(error.is_none(), "{error:?}");
    let name = declared_host(&viewer_config).unwrap().unwrap();
    let route = enrolled(&installed, &name).unwrap();
    assert_eq!(route.fingerprint, own_identity.fingerprint);
    assert_eq!(route.addresses, vec!["127.0.0.1:8766"]);
    assert!(peer::authorized(&peer::peer_dir(&state), &hosts::hub_key(&viewer_state).unwrap()));
    provision_viewer(&config, &viewer, &state).unwrap();
    let (reinstalled, error) = Config::load_from(&viewer_config);
    assert!(error.is_none());
    assert_eq!(reinstalled.hosts, installed.hosts, "repeat provisioning keeps the same identity");
    let trust_file = crate::config::hosts_path(&viewer_config);
    let before = fs::read(&trust_file).unwrap();
    let mut replacement = route.clone();
    replacement.fingerprint = "0".repeat(64);
    assert!(configure_viewer(&viewer_config, replacement, "127.0.0.1".parse().unwrap()).is_err());
    let mut redirected = route;
    redirected.addresses = vec!["127.0.0.2:8766".into()];
    assert!(configure_viewer(&viewer_config, redirected, "127.0.0.1".parse().unwrap()).is_err());
    assert_eq!(fs::read(trust_file).unwrap(), before, "failed provisioning preserves installed trust");
    let partial_config = dir.path().join("partial/config.json");
    fs::create_dir_all(partial_config.parent().unwrap()).unwrap();
    fs::write(&partial_config, b"{}").unwrap();
    fs::write(crate::config::hosts_path(&partial_config), br#"{"hosts":[]}"#).unwrap();
    // Fail only the inventory's atomic write, after publishing the selector.
    fs::create_dir(partial_config.parent().unwrap().join(format!(".hosts.json.{}.tmp", std::process::id()))).unwrap();
    let partial_route = installed.hosts[0].clone();
    assert!(configure_viewer(&partial_config, partial_route, "127.0.0.1".parse().unwrap()).is_err());
    assert_eq!(
        declared_host(&partial_config).unwrap().as_deref(),
        Some("coordinator"),
        "a partial install must retain the remote selector so it cannot elect a second coordinator"
    );
    assert!(selected_from(&partial_config, &Config::default()).is_err());
    for choice in [serde_json::json!({"host": null}), serde_json::json!({"host": "another"})] {
        let selector = serde_json::to_vec(&choice).unwrap();
        fs::write(declaration_path(&viewer_config), &selector).unwrap();
        assert!(configure_viewer(&viewer_config, installed.hosts[0].clone(), "127.0.0.1".parse().unwrap()).is_err());
        assert_eq!(
            fs::read(declaration_path(&viewer_config)).unwrap(),
            selector,
            "automatic setup preserves explicit choice"
        );
        assert_eq!(fs::read(crate::config::hosts_path(&viewer_config)).unwrap(), before);
    }
    let corrupt_config = dir.path().join("corrupt/config.json");
    fs::create_dir_all(corrupt_config.parent().unwrap()).unwrap();
    fs::write(&corrupt_config, b"{}").unwrap();
    fs::write(crate::config::hosts_path(&corrupt_config), b"{").unwrap();
    assert!(configure_viewer(&corrupt_config, installed.hosts[0].clone(), "127.0.0.1".parse().unwrap()).is_err());
    assert_eq!(fs::read(crate::config::hosts_path(&corrupt_config)).unwrap(), b"{");
    assert!(
        !declaration_path(&corrupt_config).exists(),
        "unavailable inventory is preserved before provisioning starts"
    );
}
