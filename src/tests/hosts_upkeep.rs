use super::*;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::testsupport::{loopback_peer, TempDir};

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=Deck Test", "-c", "user.email=deck@test.invalid", "-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write_json(path: &Path, value: Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value.to_string()).unwrap();
}

fn claude_login(home: &Path, account: &str, expires: i64) {
    write_json(
        &home.join(".claude/.credentials.json"),
        json!({"claudeAiOauth": {"refreshToken": format!("r{expires}"), "expiresAt": expires}}),
    );
    write_json(&home.join(".claude.json"), json!({"oauthAccount": {"accountUuid": account}, "numStartups": 3}));
}

fn paths_in(home: &Path) -> LoginPaths {
    LoginPaths { claude_dir: home.join(".claude"), codex_dir: home.join(".codex") }
}

struct Pair {
    _dir: TempDir,
    hub: HubCtx,
    host_home: PathBuf,
    host: HostConfig,
    key: String,
}

/// Hub sign-ins, and a host serving a repository and its own sign-ins behind
/// a peer listener on loopback.
fn pair(name: &str) -> Pair {
    let dir = TempDir::new(name);
    let host_repo = dir.path().join("host/repo");
    fs::create_dir_all(&host_repo).unwrap();
    git(&host_repo, &["init", "-q", "-b", "master"]);
    let host_home = dir.path().join("host/home");
    fs::create_dir_all(&host_home).unwrap();
    let (host, key) = loopback_peer(
        dir.path().join("host/peer"),
        &dir.path().join("hub/state"),
        host_repo,
        paths_in(&host_home),
        None,
    );
    let hub = HubCtx { logins: paths_in(&dir.path().join("hub/home")), sync_logins: true, refs: Default::default() };
    Pair { _dir: dir, hub, host_home, host, key }
}

fn channel(p: &Pair) -> UnixStream {
    let (stream, _) = hosts::open(&p.host, &p.key, Channel::Host).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    stream
}

/// A host that answers each mirror request with the next of `answers`,
/// counting the requests it was sent.
fn mirror_host(answers: Vec<HostResp>) -> (UnixStream, std::thread::JoinHandle<usize>) {
    let (hub, mut host) = UnixStream::pair().unwrap();
    let serving = std::thread::spawn(move || {
        let mut asked = 0;
        let mut answers = answers.into_iter();
        while let Ok(Some(req)) = read_msg::<_, HostReq>(&mut host) {
            assert_eq!(req, HostReq::MirrorFetch);
            asked += 1;
            write_msg(&mut host, &answers.next().expect("an answer for each request")).unwrap();
        }
        asked
    });
    (hub, serving)
}

fn fixture_host() -> HostConfig {
    HostConfig {
        role: Default::default(),
        name: "fedora".into(),
        label: Some("Fedora".into()),
        tag: 1,
        addresses: vec![],
        ssh: None,
        fingerprint: String::new(),
    }
}

#[test]
fn a_connected_host_fetches_once_and_again_after_each_settled_ref_change() {
    let welcome = Welcome { capabilities: vec![mirror::CAPABILITY.into()], ..Welcome::default() };
    let up_to_date = HostResp::Mirrored { ff: mirror::Ff::UpToDate };
    let (mut stream, serving) = mirror_host(vec![up_to_date.clone(), up_to_date]);
    let refs = AtomicU64::new(0);
    let (mut done, mut retry) = (None, None);
    let host = fixture_host();
    // A new connection fetches once even without a change.
    assert_eq!(mirror_round(&host, &mut stream, &welcome, &refs, &mut done, &mut retry), Ok(None));
    assert_eq!(mirror_round(&host, &mut stream, &welcome, &refs, &mut done, &mut retry), Ok(None));
    refs.fetch_add(1, Ordering::Release);
    assert_eq!(mirror_round(&host, &mut stream, &welcome, &refs, &mut done, &mut retry), Ok(None));
    assert_eq!(mirror_round(&host, &mut stream, &welcome, &refs, &mut done, &mut retry), Ok(None));
    drop(stream);
    assert_eq!(serving.join().unwrap(), 2, "one fetch per generation");
}

#[test]
fn held_diverged_and_failed_mirrors_are_reported_and_failures_retry() {
    let welcome = Welcome { capabilities: vec![mirror::CAPABILITY.into()], ..Welcome::default() };
    let (mut stream, serving) = mirror_host(vec![
        HostResp::Mirrored { ff: mirror::Ff::Held { why: "the checkout has uncommitted work".into() } },
        HostResp::Mirrored { ff: mirror::Ff::Diverged },
        HostResp::Error { text: "git fetch from omarchy: unreachable".into() },
    ]);
    let refs = AtomicU64::new(0);
    let (mut done, mut retry) = (None, None);
    let host = fixture_host();
    let note = mirror_round(&host, &mut stream, &welcome, &refs, &mut done, &mut retry).unwrap().unwrap();
    assert!(note.contains("Fedora's master was not fast-forwarded: the checkout has uncommitted work"), "{note}");
    refs.fetch_add(1, Ordering::Release);
    let note = mirror_round(&host, &mut stream, &welcome, &refs, &mut done, &mut retry).unwrap().unwrap();
    assert!(note.contains("diverged"), "{note}");
    refs.fetch_add(1, Ordering::Release);
    let note = mirror_round(&host, &mut stream, &welcome, &refs, &mut done, &mut retry).unwrap().unwrap();
    assert!(note.contains("unreachable"), "{note}");
    assert!(retry.is_some(), "a failed fetch is tried again");
    assert_eq!(
        mirror_round(&host, &mut stream, &welcome, &refs, &mut done, &mut retry),
        Ok(None),
        "not before its time"
    );
    drop(stream);
    assert_eq!(serving.join().unwrap(), 3);
}

#[test]
fn a_host_without_the_mirror_is_reported_and_never_sent_a_request() {
    let (mut stream, serving) = mirror_host(vec![]);
    let refs = AtomicU64::new(0);
    let (mut done, mut retry) = (None, None);
    let note =
        mirror_round(&fixture_host(), &mut stream, &Welcome::default(), &refs, &mut done, &mut retry).unwrap().unwrap();
    assert!(note.contains("does not accept the repository mirror"), "{note}");
    drop(stream);
    assert_eq!(serving.join().unwrap(), 0);
}

/// The next host event `run` reports, skipping update and mirror notes.
fn next_event(events: &std::sync::mpsc::Receiver<Ev>) -> HostEv {
    loop {
        let Ev::Host(event) = events.recv_timeout(Duration::from_secs(30)).unwrap() else { continue };
        if !matches!(*event, HostEv::Report { .. }) {
            return *event;
        }
    }
}

#[test]
fn a_build_host_reports_its_resources_and_outages_and_nothing_of_its_repository() {
    // Owner, 2026-11-08: Ubuntu and WSL are build hosts; the sidebar keeps
    // their CPU, RAM and SSD.
    let mut p = pair("upkeep-build");
    let repo = p._dir.path().join("host/repo");
    fs::write(repo.join("a.txt"), "one").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    p.host.role = crate::config::HostRole::Build;
    let (tx, events) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let running = {
        let (host, key, hub, stop) = (p.host.clone(), p.key.clone(), p.hub.clone(), Arc::clone(&stop));
        std::thread::spawn(move || run(&host, &key, &tx, &stop, &hub))
    };
    let HostEv::Stats { tag: 1, stats } = next_event(&events) else { panic!("expected the host's stats") };
    stop.store(true, Ordering::SeqCst);
    running.join().unwrap();
    assert!(stats.memory.is_some(), "resources are reported");
    assert_eq!((stats.branch, stats.head, stats.branches.len(), stats.worktrees), (None, None, 0, 0));

    // A build host has no session channel, so its host channel reports its outages.
    p.host.addresses.clear();
    let (tx, events) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let running = {
        let (host, key, hub, stop) = (p.host.clone(), p.key.clone(), p.hub.clone(), Arc::clone(&stop));
        std::thread::spawn(move || run(&host, &key, &tx, &stop, &hub))
    };
    assert!(matches!(next_event(&events), HostEv::Down { tag: 1, .. }));
    stop.store(true, Ordering::SeqCst);
    running.join().unwrap();

    // An agent host's outages come from its session channel instead.
    p.host.role = crate::config::HostRole::Agents;
    let (tx, events) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let running = {
        let (host, key, hub, stop) = (p.host.clone(), p.key.clone(), p.hub.clone(), Arc::clone(&stop));
        std::thread::spawn(move || run(&host, &key, &tx, &stop, &hub))
    };
    assert!(events.recv_timeout(Duration::from_secs(1)).is_err(), "no second Down for an agent host");
    stop.store(true, Ordering::SeqCst);
    running.join().unwrap();
}

#[test]
fn a_host_without_a_sign_in_gets_the_hubs_and_a_newer_renewal_comes_back() {
    let p = pair("upkeep-logins");
    let mut stream = channel(&p);
    let hub_home = p.hub.logins.claude_dir.parent().unwrap().to_path_buf();
    claude_login(&hub_home, "acct", 2_000_000_000_000);
    write_json(
        &p.hub.logins.codex_dir.join("auth.json"),
        json!({"last_refresh": "2026-10-31T12:00:00Z", "tokens": {"account_id": "acct", "refresh_token": "c1"}}),
    );

    let notes = sync_logins(&p.host, &mut stream, &p.hub).unwrap();
    assert!(notes.iter().all(|(_, note)| note.is_none()), "{notes:?}");
    let host_paths = paths_in(&p.host_home);
    assert_eq!(logins::state(&host_paths, LoginTool::Claude, 0).account.as_deref(), Some("acct"));
    assert_eq!(logins::state(&host_paths, LoginTool::Codex, 0).account.as_deref(), Some("acct"));

    // The host renews its Claude login: the hub takes the newer one.
    claude_login(&p.host_home, "acct", 2_000_000_100_000);
    sync_logins(&p.host, &mut stream, &p.hub).unwrap();
    assert_eq!(logins::state(&p.hub.logins, LoginTool::Claude, 0).renewed, Some(2_000_000_100_000));
    let hub_json: Value = serde_json::from_str(&fs::read_to_string(hub_home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(hub_json["numStartups"], 3, "the hub's own Claude settings stay");

    // Another account on the host is preserved without an error, and the hub's own
    // login is not replaced by it.
    claude_login(&p.host_home, "someone-else", 2_000_000_200_000);
    let notes = sync_logins(&p.host, &mut stream, &p.hub).unwrap();
    assert!(notes.iter().all(|(_, note)| note.is_none()), "preserved sign-ins are status, not failures: {notes:?}");
    assert_eq!(logins::state(&p.hub.logins, LoginTool::Claude, 0).account.as_deref(), Some("acct"));
    assert_eq!(logins::state(&host_paths, LoginTool::Claude, 0).account.as_deref(), Some("someone-else"));
}

#[test]
fn the_hub_is_never_given_a_login_when_it_has_none() {
    let p = pair("upkeep-logins-none");
    let mut stream = channel(&p);
    claude_login(&p.host_home, "acct", 2_000_000_000_000);
    sync_logins(&p.host, &mut stream, &p.hub).unwrap();
    assert_eq!(logins::state(&p.hub.logins, LoginTool::Claude, 0), Default::default());
    assert!(!p.hub.logins.claude_dir.exists());
}

/// Exercise the real update request and binary payload without installing over
/// either the test runner or the owner's service.
fn attempted_update(version: u32, built: u64) -> bool {
    attempted_update_selected(version, built, true)
}

fn attempted_update_selected(version: u32, built: u64, selected: bool) -> bool {
    use std::thread;
    let p = pair("upkeep-update");
    let (mut hub, mut host) = UnixStream::pair().unwrap();
    host.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let receiving = thread::spawn(move || {
        let Some(req) = read_msg::<_, HostReq>(&mut host).unwrap() else { return false };
        let HostReq::Update { len, sha256, built } = req else { panic!("expected a build update") };
        let bytes = peer::read_payload(&mut host, len).unwrap();
        assert_eq!(sha256, crate::remote::auth::sha256_hex(&bytes));
        assert_eq!(sha256, hosts::this_build().0);
        assert_eq!(built, hosts::this_build().1);
        write_msg(&mut host, &HostResp::Updated).unwrap();
        true
    });
    let welcome = Welcome { version, build: "b".repeat(64), built, ..Welcome::default() };
    let policy = crate::config::RefreshPolicy {
        build_host: if selected { p.host.name.clone() } else { "other".into() },
        hosts: vec![if selected { p.host.name.clone() } else { "other".into() }],
        excluded_hosts: if selected { vec![] } else { vec![p.host.name.clone()] },
    };
    let updated = update_host(&p.host, &mut hub, &welcome, Some(&policy)).unwrap();
    drop(hub);
    assert_eq!(receiving.join().unwrap(), updated, "the return value matches the actual transfer");
    updated
}

#[test]
fn an_excluded_peer_receives_no_update_request_or_binary_bytes() {
    assert!(!attempted_update_selected(0, 0, false));
}

#[test]
fn an_outdated_protocol_updates_even_when_its_install_timestamp_is_newer() {
    let ours = hosts::this_build().1;
    assert!(attempted_update(crate::protocol::PROTOCOL_VERSION - 1, ours + 3600));
}

#[test]
fn a_newer_protocol_is_never_downgraded_even_with_an_older_timestamp() {
    assert!(!attempted_update(crate::protocol::PROTOCOL_VERSION + 1, 1));
}
