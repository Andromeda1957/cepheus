use super::*;

use std::os::unix::fs::PermissionsExt;

use crate::testsupport::TempDir;

const KEY_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KEY_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn a_host_admits_every_recorded_hub_and_keeps_only_their_hashes() {
    let dir = TempDir::new("peer-keys");
    assert!(!authorized(dir.path(), KEY_A), "no key recorded: nobody");
    record_hub_key(dir.path(), KEY_A).unwrap();
    record_hub_key(dir.path(), KEY_B).unwrap();
    assert!(authorized(dir.path(), KEY_A), "the first hub still connects after a second is added");
    assert!(authorized(dir.path(), KEY_B));
    assert!(!authorized(dir.path(), &"c".repeat(64)));
    let stored = fs::read_to_string(dir.path().join(HUB_KEY_HASH)).unwrap();
    assert!(!stored.contains(KEY_A) && !stored.contains(KEY_B), "never the key itself");
    assert_eq!(stored.lines().count(), 2);
    record_hub_key(dir.path(), KEY_A).unwrap();
    assert_eq!(fs::read_to_string(dir.path().join(HUB_KEY_HASH)).unwrap().lines().count(), 2, "recorded once");
    let fleet: Vec<String> = (0..6).map(|index| format!("{index:064x}")).collect();
    for key in &fleet {
        record_hub_key(dir.path(), key).unwrap();
    }
    assert!(authorized(dir.path(), KEY_A) && authorized(dir.path(), KEY_B));
    assert!(fleet.iter().all(|key| authorized(dir.path(), key)), "every physical interface retains access");
    let oversized = "x".repeat(MAX_HUB_KEY_BYTES as usize + 1);
    fs::write(dir.path().join(HUB_KEY_HASH), &oversized).unwrap();
    assert!(!authorized(dir.path(), KEY_A), "oversized authorization data fails closed");
    assert!(record_hub_key(dir.path(), KEY_A).is_err(), "corrupt data is not silently replaced");
    assert_eq!(fs::read_to_string(dir.path().join(HUB_KEY_HASH)).unwrap(), oversized);
}

#[test]
fn the_oldest_hub_key_goes_once_there_are_too_many() {
    let dir = TempDir::new("peer-keys-max");
    let keys: Vec<String> = (0..=MAX_HUB_KEYS).map(|i| format!("{i:064x}")).collect();
    for key in &keys {
        record_hub_key(dir.path(), key).unwrap();
    }
    assert!(!authorized(dir.path(), &keys[0]));
    assert!(keys[1..].iter().all(|k| authorized(dir.path(), k)));
}

fn script(body: &str) -> Vec<u8> {
    format!("#!/bin/sh\n{body}\n").into_bytes()
}

#[test]
fn a_hub_build_replaces_the_program_dated_as_the_hubs() {
    let dir = TempDir::new("peer-update");
    let program = dir.path().join("cepheus");
    fs::write(&program, script("echo old")).unwrap();
    let new = script("exit 0");
    replace_program(&program, &new, &auth::sha256_hex(&new), 1_700_000_000).unwrap();
    assert_eq!(fs::read(&program).unwrap(), new);
    // The staged executable honors the caller's umask, including private test
    // containers. Compare with the same requested mode without changing the
    // process-wide umask while other tests may create files.
    let probe = dir.path().join("mode-probe");
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new().write(true).create_new(true).mode(0o755).open(&probe).unwrap();
    let expected_mode = fs::metadata(&probe).unwrap().permissions().mode() & 0o777;
    fs::remove_file(probe).unwrap();
    assert_eq!(fs::metadata(&program).unwrap().permissions().mode() & 0o777, expected_mode);
    assert_eq!(hosts::modified_secs(&program), 1_700_000_000);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "no update file is left behind");
}

#[test]
fn a_damaged_or_unrunnable_build_leaves_the_program_as_it_was() {
    let dir = TempDir::new("peer-update-bad");
    let program = dir.path().join("cepheus");
    let old = script("echo old");
    fs::write(&program, &old).unwrap();

    let new = script("exit 0");
    let err = replace_program(&program, &new, &auth::sha256_hex(b"something else"), 1).unwrap_err();
    assert!(format!("{err:#}").contains("damaged"), "{err:#}");
    assert_eq!(fs::read(&program).unwrap(), old);

    let broken = script("echo 'cannot start' >&2; exit 1");
    let err = replace_program(&program, &broken, &auth::sha256_hex(&broken), 1).unwrap_err();
    assert!(format!("{err:#}").contains("cannot start"), "{err:#}");
    assert_eq!(fs::read(&program).unwrap(), old);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "no update file is left behind");
}

/// OPS-S11801: two hubs update this host at once. One build is staged and
/// running its check when the other fails; that failure must not take the
/// first stage away before it is put in place.
#[test]
fn a_failed_update_leaves_another_updates_stage_alone() {
    let dir = TempDir::new("peer-update-overlap");
    let program = dir.path().join("cepheus");
    fs::write(&program, script("echo old")).unwrap();
    let (started, go) = (dir.path().join("started"), dir.path().join("go"));
    // The first build's check waits until the second update has failed.
    let first = script(&format!(
        "touch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nexit 0",
        started.display(),
        go.display()
    ));
    let updating = {
        let (program, first) = (program.clone(), first.clone());
        thread::spawn(move || replace_program(&program, &first, &auth::sha256_hex(&first), 1))
    };
    let deadline = Instant::now() + crate::testsupport::scaled_timeout(Duration::from_secs(20));
    while !started.exists() {
        assert!(Instant::now() < deadline, "the first build's check never started");
        thread::sleep(Duration::from_millis(10));
    }
    let broken = script("echo 'cannot start' >&2; exit 1");
    assert!(replace_program(&program, &broken, &auth::sha256_hex(&broken), 2).is_err());
    fs::write(&go, "").unwrap();
    updating.join().unwrap().expect("the first update still completes");
    assert_eq!(fs::read(&program).unwrap(), first);
    fs::remove_file(&started).unwrap();
    fs::remove_file(&go).unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "no update file is left behind");
}

#[test]
fn a_request_cannot_claim_more_than_a_program_or_bundle_needs() {
    let mut empty: &[u8] = &[];
    assert!(read_payload(&mut empty, MAX_PAYLOAD + 1).is_err());
    let mut short: &[u8] = b"abc";
    assert!(read_payload(&mut short, 4).is_err(), "fewer bytes than promised");
    let mut exact: &[u8] = b"abcd";
    assert_eq!(read_payload(&mut exact, 4).unwrap(), b"abcd");
}

/// The hub's phone search of a host's sessions is answered by the host,
/// from its own logs read whole, with the sessions to read on screen.
#[test]
fn a_host_searches_its_own_sessions_for_the_hub() {
    let dir = TempDir::new("peer-search");
    let start = dir.path().join("site");
    let logins = LoginPaths { claude_dir: dir.path().join("claude"), codex_dir: dir.path().join("codex") };
    let id = "11111111-1111-4111-8111-111111111111";
    let transcript = crate::claude::transcript_path(&logins.claude_dir, &start.to_string_lossy(), id);
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let record = serde_json::json!({"type": "user", "message": {"role": "user", "content": "give agy a turn"}});
    fs::write(&transcript, format!("{record}\n")).unwrap();
    let peer = Peer::new(dir.path().join("peer"), vec![], "fedora".into(), start, None).with_logins(logins);
    let session = |id: u64, tool: crate::protocol::Tool, conversation: Option<&str>| crate::protocol::SessionInfo {
        spawned_ms: 1000 + id,
        account_root: None,
        metadata: Default::default(),
        id,
        tool,
        title: String::new(),
        status: crate::protocol::Status::Idle,
        detail: String::new(),
        pid: None,
        conversation: conversation.map(str::to_string),
        worktree: None,
        containers: Vec::new(),
        review: false,
        alive: true,
        has_screen: true,
        created_at: 0,
        status_since: 0,
        subagents: Vec::new(),
        activity: None,
        last_active: 0,
        cwd: None,
        job: None,
        host: None,
    };
    let (hub_end, host_end) = UnixStream::pair().unwrap();
    thread::scope(|scope| {
        let serving = scope.spawn(|| requests_using(&peer, host_end, Config::load));
        let mut hub_end = hub_end;
        let sessions =
            vec![session(5, crate::protocol::Tool::Claude, Some(id)), session(6, crate::protocol::Tool::Grok, None)];
        write_msg(&mut hub_end, &HostReq::Search { sessions: sessions.clone(), words: vec!["AGY".into()] }).unwrap();
        let resp = read_msg::<_, HostResp>(&mut hub_end).unwrap().unwrap();
        let HostResp::Search { found } = resp else { panic!("{resp:?}") };
        assert_eq!(found.hits.iter().map(|h| (h.id, h.mine)).collect::<Vec<_>>(), [(5, 1)], "the words are folded");
        assert_eq!(found.hits[0].snippets[0].text, "give agy a turn");
        assert_eq!(found.screen, [6]);
        write_msg(&mut hub_end, &HostReq::Search { sessions, words: vec![] }).unwrap();
        let resp = read_msg::<_, HostResp>(&mut hub_end).unwrap().unwrap();
        assert!(matches!(resp, HostResp::Error { .. }), "nothing to search for: {resp:?}");
        drop(hub_end);
        serving.join().unwrap().unwrap();
    });
}

/// A range dropped from `peer_allow` stops reaching the host once the
/// listener restarts with the new list, which the service does when it exits.
#[test]
fn a_listener_gives_way_when_what_it_serves_changes() {
    let allow = vec!["192.0.2.0/24".to_string(), "198.51.100.102/32".to_string()];
    let config = Config { peer_listen: "0.0.0.0:8766".into(), peer_allow: allow.clone(), ..Config::default() };
    assert_eq!(settings_changed(&config, "0.0.0.0:8766", &allow), None);
    let narrowed = Config { peer_allow: allow[..1].to_vec(), ..config.clone() };
    assert_eq!(settings_changed(&narrowed, "0.0.0.0:8766", &allow), Some("\"peer_allow\" changed"));
    let moved = Config { peer_listen: "0.0.0.0:8767".into(), ..config.clone() };
    assert_eq!(settings_changed(&moved, "0.0.0.0:8766", &allow), Some("\"peer_listen\" changed"));
    let off = Config { peer_listen: String::new(), ..config };
    assert_eq!(settings_changed(&off, "0.0.0.0:8766", &allow), Some("\"peer_listen\" was turned off"));
}

#[test]
fn disabled_sync_preserves_credentials_and_keeps_the_host_channel_usable() {
    let dir = TempDir::new("peer-sync-disabled");
    let logins = LoginPaths { claude_dir: dir.path().join("claude"), codex_dir: dir.path().join("codex") };
    fs::create_dir_all(&logins.codex_dir).unwrap();
    let auth_path = logins.codex_dir.join("auth.json");
    let original = r#"{"tokens":{"account_id":"fixture","refresh_token":"fixture-refresh"}}"#;
    fs::write(&auth_path, original).unwrap();
    let peer =
        Peer::new(dir.path().join("peer"), vec![], "fixture".into(), dir.path().join("repo"), None).with_logins(logins);
    let mut config = Config::default();
    config.plugins.insert("git-mirror".into(), false);
    config.plugins.insert("login-sync".into(), false);
    let (mut hub, host) = UnixStream::pair().unwrap();
    hub.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    thread::scope(|scope| {
        let serving = scope.spawn(|| requests_using(&peer, host, || (config.clone(), None)));
        let requests = [
            HostReq::MirrorFetch,
            HostReq::Logins,
            HostReq::GetLogin { tool: logins::LoginTool::Codex },
            HostReq::PutLogin {
                tool: logins::LoginTool::Codex,
                login: serde_json::json!({"tokens":{"account_id":"other"}}),
            },
        ];
        for request in requests {
            write_msg(&mut hub, &request).unwrap();
            assert!(matches!(read_msg::<_, HostResp>(&mut hub).unwrap(), Some(HostResp::Error { .. })));
        }
        write_msg(&mut hub, &HostReq::Stats { resources_only: false }).unwrap();
        assert!(matches!(read_msg::<_, HostResp>(&mut hub).unwrap(), Some(HostResp::Stats { .. })));
        drop(hub);
        serving.join().unwrap().unwrap();
    });
    assert_eq!(fs::read_to_string(auth_path).unwrap(), original);

    // A readable host remains usable when its enabled credential file is damaged.
    fs::write(peer.logins.codex_dir.join("auth.json"), b"{broken").unwrap();
    config.plugins.insert("login-sync".into(), true);
    let (mut hub, host) = UnixStream::pair().unwrap();
    hub.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    thread::scope(|scope| {
        let serving = scope.spawn(|| requests_using(&peer, host, || (config.clone(), None)));
        write_msg(&mut hub, &HostReq::Logins).unwrap();
        assert!(matches!(read_msg::<_, HostResp>(&mut hub).unwrap(), Some(HostResp::Error { .. })));
        write_msg(&mut hub, &HostReq::Stats { resources_only: false }).unwrap();
        assert!(matches!(read_msg::<_, HostResp>(&mut hub).unwrap(), Some(HostResp::Stats { .. })));
        drop(hub);
        serving.join().unwrap().unwrap();
    });
}

#[test]
fn mirror_fetch_uses_only_the_hosts_own_remote_and_keeps_its_work() {
    let dir = TempDir::new("peer-mirror");
    let git = |repo: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "user.name=Deck Test", "-c", "user.email=deck@test.invalid", "-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let hub_repo = dir.path().join("hub");
    fs::create_dir_all(&hub_repo).unwrap();
    git(&hub_repo, &["init", "-q", "-b", "master"]);
    git(&hub_repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    let target = dir.path().join("target");
    git(dir.path(), &["clone", "-q", "-o", "omarchy", hub_repo.to_str().unwrap(), target.to_str().unwrap()]);
    git(&target, &["branch", "host-only"]);
    git(&hub_repo, &["branch", "agent/feature"]);
    git(&hub_repo, &["commit", "-q", "--allow-empty", "-m", "merged on the hub"]);
    let tip = git(&hub_repo, &["rev-parse", "HEAD"]);
    let peer = Peer::new(dir.path().join("peer"), vec![], "fixture".into(), target.clone(), None);
    let mut config = Config::default();
    config.plugins.insert("git-mirror".into(), true);
    let (mut hub, host) = UnixStream::pair().unwrap();
    hub.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    thread::scope(|scope| {
        let serving = scope.spawn(|| requests_using(&peer, host, || (config.clone(), None)));
        // Without its own remote setting the host refuses; the hub never names one.
        write_msg(&mut hub, &HostReq::MirrorFetch).unwrap();
        assert!(matches!(read_msg::<_, HostResp>(&mut hub).unwrap(), Some(HostResp::Error { .. })));
        drop(hub);
        serving.join().unwrap().unwrap();
    });
    for remote in ["--upload-pack=touch", "../hub", "a b"] {
        config.plugin_settings.insert("git-mirror".into(), serde_json::json!({"remote": remote}));
        assert!(crate::hosts::mirror::Settings::from_config(&config).is_err(), "{remote}");
    }
    config.plugin_settings.insert("git-mirror".into(), serde_json::json!({"remote": "omarchy"}));
    assert!(plugin_capabilities(&(config.clone(), None)).iter().any(|c| c == crate::hosts::mirror::CAPABILITY));
    let (mut hub, host) = UnixStream::pair().unwrap();
    hub.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    thread::scope(|scope| {
        let serving = scope.spawn(|| requests_using(&peer, host, || (config.clone(), None)));
        write_msg(&mut hub, &HostReq::MirrorFetch).unwrap();
        assert!(matches!(
            read_msg::<_, HostResp>(&mut hub).unwrap(),
            Some(HostResp::Mirrored { ff: crate::hosts::mirror::Ff::Forwarded { .. } })
        ));
        drop(hub);
        serving.join().unwrap().unwrap();
    });
    assert_eq!(git(&target, &["rev-parse", "master"]), tip);
    assert_eq!(git(&target, &["rev-parse", "refs/remotes/omarchy/agent/feature"]).len(), 40);
    assert_eq!(git(&target, &["rev-parse", "refs/heads/host-only"]).len(), 40, "the host's own branch stays");
}

#[test]
fn viewer_enrollment_requires_a_trusted_channel_and_moves_only_a_digest() {
    let dir = TempDir::new("peer-viewer-enrollment");
    let state = dir.path().join("coordinator");
    let viewer = dir.path().join("viewer");
    let (host, key) = crate::testsupport::loopback_peer(
        viewer.join("peer"),
        &state,
        dir.path().to_path_buf(),
        LoginPaths { claude_dir: dir.path().join("claude"), codex_dir: dir.path().join("codex") },
        None,
    );
    assert!(hosts::open(&host, KEY_B, Channel::Host).is_err(), "unknown keys cannot reach enrollment");
    let (mut channel, welcome) = hosts::open(&host, &key, Channel::Host).unwrap();
    assert!(welcome.capabilities.iter().any(|value| value == client::coordinator::CAPABILITY));
    channel.set_read_timeout(Some(crate::testsupport::scaled_timeout(tls::CONNECT_TIMEOUT))).unwrap();
    write_msg(&mut channel, &HostReq::ViewerIdentity).unwrap();
    let HostResp::ViewerIdentity { sha256 } = read_msg(&mut channel).unwrap().unwrap() else {
        panic!("expected a public viewer identity");
    };
    assert_eq!(sha256, hosts::hub_key_hash(&viewer).unwrap());
    assert!(!authorized(&peer_dir(&state), &hosts::hub_key(&viewer).unwrap()));
    client::coordinator::enroll_viewer(&host, &state).unwrap();
    assert!(authorized(&peer_dir(&state), &hosts::hub_key(&viewer).unwrap()));
    let extra = auth::sha256_hex(KEY_B.as_bytes());
    write_msg(&mut channel, &HostReq::AuthorizeViewer { sha256: extra }).unwrap();
    assert_eq!(read_msg::<_, HostResp>(&mut channel).unwrap().unwrap(), HostResp::Done);
    assert!(hosts::open(&host, KEY_B, Channel::Host).is_ok());
    let before = fs::read(viewer.join("peer").join(HUB_KEY_HASH)).unwrap();
    write_msg(&mut channel, &HostReq::AuthorizeViewer { sha256: "../invalid".into() }).unwrap();
    assert!(matches!(read_msg::<_, HostResp>(&mut channel).unwrap().unwrap(), HostResp::Error { .. }));
    assert_eq!(fs::read(viewer.join("peer").join(HUB_KEY_HASH)).unwrap(), before);
    assert!(!String::from_utf8(before).unwrap().contains(KEY_B));
}

#[test]
fn phone_forwarding_rejects_noncontent_operations_before_reading_or_mutating() {
    let dir = TempDir::new("peer-phone-boundary");
    let peer = Peer::new(dir.path().join("peer"), vec![], "coordinator".into(), dir.path().join("repo"), None);
    let requests = [
        (None, HostReq::Stats { resources_only: false }),
        (None, HostReq::Logins),
        (None, HostReq::Update { len: 0, sha256: String::new(), built: 0 }),
        (None, HostReq::PhoneForward { host: None, request: Box::new(HostReq::PhoneModels) }),
        (None, HostReq::PutFile { name: "oversize.txt".into(), len: uploads::FILE_MAX + 1 }),
        (Some("fedora"), HostReq::PhoneModels),
        (Some("fedora"), HostReq::PhoneStats),
        (Some("fedora"), HostReq::PhoneAskHistory),
    ];
    for (host, request) in requests {
        let (mut viewer, server) = UnixStream::pair().unwrap();
        thread::scope(|scope| {
            let serving = scope.spawn(|| requests_using(&peer, server, || (Config::default(), None)));
            write_msg(
                &mut viewer,
                &HostReq::PhoneForward { host: host.map(str::to_owned), request: Box::new(request) },
            )
            .unwrap();
            assert!(read_msg::<_, HostResp>(&mut viewer).unwrap().is_none());
            assert!(serving.join().unwrap().is_err());
        });
    }
    assert!(!peer.uploads_dir().exists());
    let (mut viewer, server) = UnixStream::pair().unwrap();
    thread::scope(|scope| {
        let serving = scope.spawn(|| requests_using(&peer, server, || (Config::default(), None)));
        write_msg(
            &mut viewer,
            &HostReq::PhoneForward {
                host: Some("unenrolled".into()),
                request: Box::new(HostReq::ScanReport { id: "missing".into() }),
            },
        )
        .unwrap();
        assert!(matches!(read_msg::<_, HostResp>(&mut viewer).unwrap(), Some(HostResp::Error { .. })));
        write_msg(&mut viewer, &HostReq::Stats { resources_only: false }).unwrap();
        assert!(
            matches!(read_msg::<_, HostResp>(&mut viewer).unwrap(), Some(HostResp::Stats { .. })),
            "valid content failure retains channel framing"
        );
        drop(viewer);
        serving.join().unwrap().unwrap();
    });
}

#[test]
fn profile_requests_resolve_only_configured_roots_and_preserve_newer_logins() {
    use logins::LoginTool;
    let dir = TempDir::new("peer-profile-logins");
    let defaults =
        LoginPaths { claude_dir: dir.path().join("default-claude"), codex_dir: dir.path().join("default-codex") };
    let peer = Peer::new(dir.path().join("peer"), vec![], "fixture".into(), dir.path().join("repo"), None)
        .with_logins(defaults.clone());
    let root = dir.path().join("profile");
    let mut config = Config::default();
    config.plugin_settings.insert(
        "provider-accounts".into(),
        serde_json::json!({"profiles":{"work":{"provider":"codex","directory":root}}}),
    );
    let (mut hub, host) = UnixStream::pair().unwrap();
    hub.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let login = |id: &str, hour: u8| serde_json::json!({"tokens":{"account_id":id,"refresh_token":"synthetic"},"last_refresh":format!("2026-11-05T{hour:02}:00:00Z")});
    thread::scope(|scope| {
        let serving = scope.spawn(|| requests_using(&peer, host, || (config.clone(), None)));
        for request in [
            HostReq::PutProfileLogin {
                profile: "../outside".into(),
                tool: LoginTool::Codex,
                login: login("first", 10),
            },
            HostReq::PutProfileLogin { profile: "work".into(), tool: LoginTool::Claude, login: login("first", 10) },
        ] {
            write_msg(&mut hub, &request).unwrap();
            assert!(matches!(read_msg::<_, HostResp>(&mut hub).unwrap(), Some(HostResp::Error { .. })));
        }
        write_msg(
            &mut hub,
            &HostReq::PutProfileLogin { profile: "work".into(), tool: LoginTool::Codex, login: login("first", 10) },
        )
        .unwrap();
        assert!(matches!(read_msg::<_, HostResp>(&mut hub).unwrap(), Some(HostResp::Done)));
        for value in [login("first", 9), login("second", 11)] {
            write_msg(
                &mut hub,
                &HostReq::PutProfileLogin { profile: "work".into(), tool: LoginTool::Codex, login: value },
            )
            .unwrap();
            assert!(matches!(read_msg::<_, HostResp>(&mut hub).unwrap(), Some(HostResp::Error { .. })));
        }
        write_msg(&mut hub, &HostReq::ProfileLogins).unwrap();
        let Some(HostResp::ProfileLogins { logins }) = read_msg::<_, HostResp>(&mut hub).unwrap() else {
            panic!("missing profile status")
        };
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0].profile, "work");
        assert_eq!(logins[0].state.account.as_deref(), Some("first"));
        write_msg(&mut hub, &HostReq::GetProfileLogin { profile: "work".into(), tool: LoginTool::Codex }).unwrap();
        let Some(HostResp::Login { login: saved }) = read_msg::<_, HostResp>(&mut hub).unwrap() else {
            panic!("missing profile export")
        };
        assert_eq!(saved["last_refresh"], "2026-11-05T10:00:00Z");
        let hub_root = dir.path().join("hub-profile");
        let mut hub_config = config.clone();
        hub_config.plugin_settings.get_mut("provider-accounts").unwrap()["profiles"]["work"]["directory"] =
            serde_json::json!(hub_root);
        let settings = logins::Settings::from_config(&hub_config).unwrap();
        let profile = settings.accounts.profile("work", crate::protocol::Tool::Codex).unwrap();
        logins::import_profile(profile, &login("first", 11), 0).unwrap();
        let context = crate::hosts::upkeep::HubCtx {
            logins: LoginPaths {
                claude_dir: dir.path().join("hub-default-claude"),
                codex_dir: dir.path().join("hub-default-codex"),
            },
            sync_logins: true,
            refs: Default::default(),
        };
        let host_config = crate::config::HostConfig {
            role: Default::default(),
            name: "fixture".into(),
            label: None,
            tag: 1,
            addresses: vec![],
            ssh: None,
            fingerprint: String::new(),
        };
        let notes =
            logins::sync_with_settings(&host_config, &mut hub, &context, &settings, logins::PROFILE_PROTOCOL).unwrap();
        assert!(notes.iter().all(|(_, note)| note.is_none()));
        write_msg(
            &mut hub,
            &HostReq::PutProfileLogin { profile: "work".into(), tool: LoginTool::Codex, login: login("first", 12) },
        )
        .unwrap();
        assert!(matches!(read_msg::<_, HostResp>(&mut hub).unwrap(), Some(HostResp::Done)));
        logins::sync_with_settings(&host_config, &mut hub, &context, &settings, logins::PROFILE_PROTOCOL).unwrap();
        assert_eq!(logins::state(profile, LoginTool::Codex, 0).renewed, logins::rfc3339_ms("2026-11-05T12:00:00Z"));
        drop(hub);
        serving.join().unwrap().unwrap();
    });
    assert!(!defaults.codex_dir.exists(), "profile sync never touches the default account");
}
