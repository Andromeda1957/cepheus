use super::*;

use crate::config::Config;
use crate::testsupport::TempDir;

fn run(repo: &Path, args: &[&str]) -> String {
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

fn commit(repo: &Path, file: &str, text: &str) -> String {
    fs::write(repo.join(file), text).unwrap();
    run(repo, &["add", file]);
    run(repo, &["commit", "-q", "-m", text]);
    run(repo, &["rev-parse", "HEAD"])
}

/// A hub repository with one commit on master, and a host clone of it whose
/// mirror remote is `omarchy`.
fn hub_and_host(dir: &TempDir) -> (PathBuf, PathBuf) {
    let hub = dir.path().join("hub");
    fs::create_dir_all(&hub).unwrap();
    run(&hub, &["init", "-q", "-b", "master"]);
    commit(&hub, "a.txt", "one");
    let host = dir.path().join("host");
    run(dir.path(), &["clone", "-q", "-o", "omarchy", hub.to_str().unwrap(), host.to_str().unwrap()]);
    (hub, host)
}

fn settings(value: serde_json::Value) -> Result<Settings> {
    let mut config = Config::default();
    config.plugin_settings.insert("git-mirror".into(), value);
    Settings::from_config(&config)
}

#[test]
fn the_host_names_only_a_plain_remote_of_its_own() {
    assert!(Settings::from_config(&Config::default()).is_err(), "no remote configured");
    assert_eq!(settings(serde_json::json!({"remote": "omarchy"})).unwrap().remote, "omarchy");
    let long = "r".repeat(65);
    for bad in ["", "-u", ".hidden", "a/b", "host:path", "ssh://x/y", "a b", long.as_str()] {
        assert!(settings(serde_json::json!({"remote": bad})).is_err(), "{bad:?} accepted");
    }
    assert!(settings(serde_json::json!({"remote": "omarchy", "url": "elsewhere"})).is_err(), "unknown key accepted");
}

#[test]
fn a_clean_checked_out_master_moves_forward_and_dirty_or_busy_ones_are_held() {
    let dir = TempDir::new("mirror-ff");
    let (hub, host) = hub_and_host(&dir);
    let tip = commit(&hub, "a.txt", "two");
    run(&host, &["fetch", "-q", "omarchy"]);
    fs::write(host.join("a.txt"), "edited here").unwrap();
    assert_eq!(
        fast_forward(&host, "refs/remotes/omarchy/master"),
        Ff::Held { why: "the checkout has uncommitted work".into() }
    );
    run(&host, &["checkout", "-q", "--", "a.txt"]);
    let merge_head = PathBuf::from(run(&host, &["rev-parse", "--git-path", "MERGE_HEAD"]));
    let merge_head = if merge_head.is_absolute() { merge_head } else { host.join(merge_head) };
    fs::write(&merge_head, format!("{tip}\n")).unwrap();
    assert_eq!(fast_forward(&host, "refs/remotes/omarchy/master"), Ff::Held { why: "a merge is in progress".into() });
    fs::remove_file(&merge_head).unwrap();
    assert!(matches!(fast_forward(&host, "refs/remotes/omarchy/master"), Ff::Forwarded { to, .. } if to == tip));
    assert_eq!(run(&host, &["rev-parse", "HEAD"]), tip);
    assert_eq!(fs::read_to_string(host.join("a.txt")).unwrap(), "two");
    assert_eq!(fast_forward(&host, "refs/remotes/omarchy/master"), Ff::UpToDate);
}

#[test]
fn a_master_nobody_has_checked_out_moves_alone_and_ahead_or_diverged_ones_stay() {
    let dir = TempDir::new("mirror-ref");
    let (hub, host) = hub_and_host(&dir);
    run(&host, &["checkout", "-q", "-b", "work"]);
    let tip = commit(&hub, "a.txt", "two");
    run(&host, &["fetch", "-q", "omarchy"]);
    assert!(matches!(fast_forward(&host, "refs/remotes/omarchy/master"), Ff::Forwarded { to, .. } if to == tip));
    assert_eq!(run(&host, &["rev-parse", "master"]), tip);
    assert_eq!(run(&host, &["symbolic-ref", "--short", "HEAD"]), "work", "the checkout is untouched");

    run(&host, &["checkout", "-q", "master"]);
    commit(&host, "b.txt", "host only");
    assert_eq!(fast_forward(&host, "refs/remotes/omarchy/master"), Ff::Ahead);
    commit(&hub, "a.txt", "three");
    run(&host, &["fetch", "-q", "omarchy"]);
    let before = run(&host, &["rev-parse", "master"]);
    assert_eq!(fast_forward(&host, "refs/remotes/omarchy/master"), Ff::Diverged);
    assert_eq!(run(&host, &["rev-parse", "master"]), before);
}

#[test]
fn a_fetch_prunes_the_hubs_deleted_branches_and_keeps_the_hosts_own() {
    let dir = TempDir::new("mirror-fetch");
    let (hub, host) = hub_and_host(&dir);
    run(&hub, &["branch", "agent/gone"]);
    run(&hub, &["tag", "v1"]);
    run(&host, &["branch", "host/own"]);
    run(&host, &["tag", "host-tag"]);
    let tip = commit(&hub, "a.txt", "two");
    assert!(matches!(fetch(&host, "omarchy").unwrap(), Ff::Forwarded { to, .. } if to == tip));
    assert!(rev(&host, "refs/remotes/omarchy/agent/gone").is_some());
    assert!(rev(&host, "refs/tags/v1").is_some(), "the hub's tags arrive");

    run(&hub, &["branch", "-D", "agent/gone"]);
    run(&hub, &["tag", "-d", "v1"]);
    assert_eq!(fetch(&host, "omarchy").unwrap(), Ff::UpToDate);
    assert!(rev(&host, "refs/remotes/omarchy/agent/gone").is_none(), "pruned with the hub's branch");
    assert!(rev(&host, "refs/heads/host/own").is_some());
    assert!(rev(&host, "refs/tags/host-tag").is_some());
    assert!(rev(&host, "refs/tags/v1").is_some(), "a tag is never pruned");

    let error = fetch(&host, "nowhere").unwrap_err();
    assert!(format!("{error:#}").contains("git fetch from nowhere"), "{error:#}");
}

#[test]
fn a_linked_worktree_watches_its_repositorys_shared_git_directory() {
    let dir = TempDir::new("mirror-common");
    let (hub, _) = hub_and_host(&dir);
    let linked = dir.path().join("linked");
    run(&hub, &["worktree", "add", "-q", "-b", "side", linked.to_str().unwrap()]);
    assert_eq!(common_dir(&hub).unwrap(), hub.join(".git"));
    assert_eq!(common_dir(&linked).unwrap().canonicalize().unwrap(), hub.join(".git").canonicalize().unwrap());
    assert!(common_dir(dir.path()).is_err());
}

/// Whether a ref change arrives within `within`.
fn changes_within(refs: &mut RefWatch, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        if refs.wait(Some(left)).unwrap() {
            return true;
        }
    }
}

fn changed(refs: &mut RefWatch) -> bool {
    let seen = changes_within(refs, Duration::from_secs(5));
    while changes_within(refs, Duration::from_millis(300)) {}
    seen
}

#[test]
fn ref_writes_new_ref_directories_and_packed_refs_are_seen_and_lock_files_are_not() {
    let dir = TempDir::new("mirror-watch");
    let (hub, _) = hub_and_host(&dir);
    let tip = run(&hub, &["rev-parse", "HEAD"]);
    let mut refs = RefWatch::new(&hub.join(".git")).unwrap();

    fs::write(hub.join(".git/refs/heads/held.lock"), format!("{tip}\n")).unwrap();
    fs::remove_file(hub.join(".git/refs/heads/held.lock")).unwrap();
    assert!(!changes_within(&mut refs, Duration::from_millis(500)), "a lock file is not a ref");

    run(&hub, &["update-ref", "refs/heads/loose", &tip]);
    assert!(changed(&mut refs), "a loose branch");
    run(&hub, &["update-ref", "refs/heads/agent/new/deep", &tip]);
    assert!(changed(&mut refs), "a branch in new directories");
    commit(&hub, "a.txt", "two");
    assert!(changed(&mut refs), "a commit on master");
    run(&hub, &["update-ref", "refs/heads/agent/new/deep", "HEAD"]);
    assert!(changed(&mut refs), "a branch inside a directory created after the watch began");
    run(&hub, &["tag", "v1"]);
    assert!(changed(&mut refs), "a tag");
    run(&hub, &["pack-refs", "--all"]);
    assert!(changed(&mut refs), "packing");
    run(&hub, &["update-ref", "-d", "refs/heads/loose"]);
    assert!(changed(&mut refs), "deleting a packed branch rewrites packed-refs");
    run(&hub, &["config", "core.dummy", "1"]);
    assert!(!changes_within(&mut refs, Duration::from_millis(500)), "other Git files are not refs");
}

#[test]
fn a_burst_of_ref_changes_is_mirrored_once_it_settles() {
    let dir = TempDir::new("mirror-settle");
    let (hub, _) = hub_and_host(&dir);
    let generation = Arc::new(AtomicU64::new(0));
    watch(&hub, Arc::clone(&generation)).unwrap();
    for name in ["one", "two", "three"] {
        run(&hub, &["branch", name]);
    }
    let deadline = Instant::now() + QUIET + Duration::from_secs(5);
    while generation.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(generation.load(Ordering::Acquire), 1);
    std::thread::sleep(QUIET + Duration::from_millis(500));
    assert_eq!(generation.load(Ordering::Acquire), 1, "one burst, one generation");
}
