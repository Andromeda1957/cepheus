use super::*;
use crate::protocol::UsageWindow;
use crate::testsupport::TempDir;
use std::process::{Child, Command};

struct Sleeping(Child);
impl Sleeping {
    fn new() -> Self {
        Self(Command::new("sleep").arg("60").spawn().unwrap())
    }
    fn owner(&self, session: u64) -> Owner {
        Owner::read(self.0.id(), Some(session)).unwrap()
    }
}
impl Drop for Sleeping {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn probe(dir: &Path, available: u64, mem: f64, cpu: f64, load: f64) {
    fs::create_dir_all(dir.join("pressure")).unwrap();
    fs::write(dir.join("meminfo"), format!("MemAvailable: {} kB\nMemTotal: {} kB\n", available * 1024, 16384 * 1024))
        .unwrap();
    fs::write(dir.join("pressure/memory"), format!("full avg10={mem}\n")).unwrap();
    fs::write(dir.join("pressure/cpu"), format!("some avg60={cpu}\n")).unwrap();
    fs::write(dir.join("loadavg"), format!("{load} 0 0 1/1 1\n")).unwrap();
}
fn setup(dir: &TempDir, slots: usize) -> Scheduler {
    let root = dir.path().join("proc");
    probe(&root, 12000, 0.0, 0.0, 0.0);
    let mut settings = Settings::default();
    settings.probe.root = Some(root);
    settings.quota_percent = 90.0; // Explicit policy under test; defaults remain tunable.
    settings.pools.insert("heavy".into(), slots);
    Scheduler::new(settings, dir.path())
}
fn start(tool: Tool) -> Start {
    Start {
        host: "local".into(),
        key: "start-key".into(),
        target: None,
        attempted_at: None,
        account: None,
        id: 1,
        provider: tool,
        model: None,
        cwd: None,
        title: None,
        prompt: None,
        fallback: None,
        reason: "queued".into(),
        reset: None,
        session: None,
    }
}
fn window(p: f64, reset: Option<u64>) -> UsageWindow {
    UsageWindow { label: "weekly".into(), used_percent: Some(p), resets_at: reset, updated_at: 1 }
}

fn poll(s: &mut Scheduler) {
    s.last_tick = std::time::Instant::now() - std::time::Duration::from_secs(1);
    s.tick();
}
#[test]
fn five_processes_share_two_slots_in_fifo_and_sigkill_releases() {
    let dir = TempDir::new("scheduler-fifo");
    let mut s = setup(&dir, 2);
    let mut workers: Vec<_> = (0..5).map(|_| Sleeping::new()).collect();
    let mut ids = Vec::new();
    for (i, worker) in workers.iter().enumerate() {
        ids.push(s.hold("heavy", worker.owner(i as u64 + 1), "test", None, format!("request-{i}")).unwrap());
        poll(&mut s);
        assert!(s.snapshot().holders.len() <= 2);
    }
    assert_eq!(s.snapshot().holders.iter().map(|s| s.id).collect::<Vec<_>>(), ids[..2]);
    workers[0].0.kill().unwrap();
    workers[0].0.wait().unwrap();
    poll(&mut s);
    assert_eq!(s.snapshot().holders.iter().map(|s| s.id).collect::<Vec<_>>(), ids[1..3]);
    s.release(ids[1]).unwrap();
    poll(&mut s);
    assert_eq!(s.snapshot().holders.iter().map(|s| s.id).collect::<Vec<_>>(), ids[2..4]);
    s.ended(3);
    poll(&mut s);
    assert_eq!(s.snapshot().holders.iter().map(|s| s.id).collect::<Vec<_>>(), ids[3..5]);
    for id in &ids[3..] {
        s.release(*id).unwrap();
    }
    assert!(s.snapshot().holders.is_empty());
}

#[test]
fn one_hundred_session_requests_drain_in_arrival_order() {
    let dir = TempDir::new("scheduler-fleet100");
    let mut s = setup(&dir, 2);
    let mut expected = Vec::new();
    for i in 1..=100 {
        let owner = Owner::read(std::process::id(), Some(i)).unwrap();
        expected.push(s.hold("heavy", owner, "fleet simulation", None, format!("fleet-{i}")).unwrap());
    }
    let mut granted = Vec::new();
    while !s.slots.is_empty() {
        poll(&mut s);
        let holders = s.snapshot().holders;
        assert!(!holders.is_empty() && holders.len() <= 2);
        for holder in holders {
            granted.push(holder.id);
            s.release(holder.id).unwrap();
        }
    }
    assert_eq!(granted, expected);
}

#[test]
fn injected_resource_readings_gate_fifo_head_and_admission() {
    let dir = TempDir::new("scheduler-pressure");
    let mut s = setup(&dir, 1);
    let root = s.settings.probe.root.clone().unwrap();
    let owner = Owner::read(std::process::id(), Some(1)).unwrap();
    s.hold("heavy", owner, "build", None, "pressure".into()).unwrap();
    for (available, mem, cpu) in [(1000, 0.0, 0.0), (12000, 5.0, 0.0), (12000, 0.0, 80.0)] {
        probe(&root, available, mem, cpu, 0.0);
        assert!(!s.settings.probe.read().admits());
    }
    probe(&root, 1000, 0.0, 0.0, 0.0);
    poll(&mut s);
    assert!(s.snapshot().holders.is_empty());
    probe(&root, 12000, 0.0, 0.0, 100000.0);
    poll(&mut s);
    assert!(s.snapshot().holders.is_empty());
    s.slots[0].queued_at = 0;
    poll(&mut s);
    assert!(s.snapshot().holders.is_empty(), "slot and memory waiting never spend the load allowance");
    s.slots[0].load_waited = s.settings.max_load_wait;
    poll(&mut s);
    assert!(s.snapshot().holders.is_empty(), "elapsed waiting never overrides resource admission");
    probe(&root, 12000, 0.0, 81.0, 0.0);
    poll(&mut s);
    assert!(s.snapshot().holders.is_empty(), "CPU pressure holds work even when load is low");
    probe(&root, 12000, 0.0, 0.0, 0.0);
    poll(&mut s);
    assert_eq!(s.snapshot().holders.len(), 1);
}

#[test]
fn quota_holds_until_reset_and_explicit_fallback_is_respected() {
    let dir = TempDir::new("scheduler-quota");
    let s = setup(&dir, 1);
    let mut usage = Usage { claude: vec![window(92.0, Some(200))], codex: vec![], ..Usage::default() };
    let mut item = start(Tool::Claude);
    assert_eq!(s.dispatch_provider(&item, &usage, 100), (Tool::Claude, true, Some(200)));
    assert_eq!(s.dispatch_provider(&item, &usage, 200), (Tool::Claude, false, None));
    item.fallback = Some(Tool::Codex);
    assert_eq!(s.dispatch_provider(&item, &usage, 100), (Tool::Codex, false, None));
    usage.codex = vec![window(91.0, None)];
    assert_eq!(s.dispatch_provider(&item, &usage, 100), (Tool::Claude, true, Some(200)));
    item.provider = Tool::Codex;
    item.fallback = None;
    assert_eq!(s.dispatch_provider(&item, &usage, 100), (Tool::Codex, true, None));
}

#[test]
fn compatibility_flock_and_legacy_capacity_prevent_double_admission() {
    let dir = TempDir::new("scheduler-legacy");
    let mut s = setup(&dir, 2);
    let legacy = dir.path().join("legacy");
    fs::create_dir(&legacy).unwrap();
    fs::write(legacy.join("config.json"), r#"{"slots":1}"#).unwrap();
    let f = File::create(legacy.join("slot-0.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let owner = Owner::read(std::process::id(), Some(1)).unwrap();
    s.hold("heavy", owner, "legacy", Some(legacy), "legacy-key".into()).unwrap();
    poll(&mut s);
    assert!(s.snapshot().holders.is_empty());
    drop(f);
    poll(&mut s);
    assert_eq!(s.snapshot().holders.len(), 1);
}

#[test]
fn handover_retains_the_exact_lock_and_prunes_dead_holders() {
    let dir = TempDir::new("scheduler-handover");
    let mut s = setup(&dir, 1);
    let worker = Sleeping::new();
    let id = s.hold("heavy", worker.owner(1), "retained", None, "handover".into()).unwrap();
    poll(&mut s);
    let saved = s.saved();
    let descriptors: Vec<_> = s
        .descriptors()
        .into_iter()
        .map(|(id, fd)| {
            let copied = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            assert!(copied > 2);
            (id, copied)
        })
        .collect();
    let settings = s.settings.clone();
    drop(s);
    let mut next = Scheduler::load(settings, dir.path(), true);
    next.restore(saved, &descriptors);
    assert_eq!(next.snapshot().holders[0].id, id);
    let lock = File::options().read(true).write(true).open(dir.path().join("slots/heavy/slot-0.lock")).unwrap();
    assert_ne!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    drop(worker);
    poll(&mut next);
    assert!(next.snapshot().holders.is_empty());
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
}

#[test]
fn cancellation_tombstone_precedes_delayed_hold_and_survives_restart() {
    let dir = TempDir::new("scheduler-cancel");
    let mut s = setup(&dir, 1);
    s.cancel_key("cancelled-before-arrival").unwrap();
    let settings = s.settings.clone();
    drop(s);
    let mut s = Scheduler::new(settings, dir.path());
    assert!(s
        .hold(
            "heavy",
            Owner::read(std::process::id(), None).unwrap(),
            "cancelled",
            None,
            "cancelled-before-arrival".into()
        )
        .is_err());
    assert!(s.slots.is_empty());
}

#[test]
fn invalid_state_is_preserved_and_stale_next_counter_cannot_reuse_ids() {
    let dir = TempDir::new("scheduler-persistence");
    let mut s = setup(&dir, 1);
    s.hold("heavy", Owner::read(std::process::id(), None).unwrap(), "persisted", None, "persisted".into()).unwrap();
    let mut saved = s.saved();
    saved.next = 1;
    let max = saved.slots[0].id;
    fs::write(dir.path().join("scheduler.json"), serde_json::to_vec(&saved).unwrap()).unwrap();
    let settings = s.settings.clone();
    drop(s);
    let mut recovered = Scheduler::new(settings.clone(), dir.path());
    assert!(recovered.id() > max);
    drop(recovered);
    fs::write(dir.path().join("scheduler.json"), b"broken state").unwrap();
    let mut broken = Scheduler::new(settings, dir.path());
    assert!(broken.error.is_some());
    assert!(broken.hold("heavy", Owner::read(std::process::id(), None).unwrap(), "new", None, "new".into()).is_err());
    broken.tick();
    assert_eq!(fs::read(dir.path().join("scheduler.json")).unwrap(), b"broken state");
}

#[test]
fn an_unchanged_state_costs_no_write_or_fsync() {
    use std::os::unix::fs::MetadataExt;
    let dir = TempDir::new("scheduler-unchanged");
    let mut s = setup(&dir, 1);
    s.save().unwrap();
    let state = dir.path().join("scheduler.json");
    // Every write replaces the file by rename, so a new inode marks one.
    let written = fs::metadata(&state).unwrap().ino();
    for _ in 0..20 {
        s.last_tick = std::time::Instant::now() - std::time::Duration::from_secs(1);
        s.tick();
        s.save().unwrap();
    }
    assert_eq!(fs::metadata(&state).unwrap().ino(), written, "idle ticks must not rewrite or sync the state");
    s.hold("heavy", Owner::read(std::process::id(), None).unwrap(), "changed", None, "changed".into()).unwrap();
    assert_ne!(fs::metadata(&state).unwrap().ino(), written, "a real change is written durably");
    let saved: Saved = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    assert_eq!(saved.slots.len(), 1);
}

#[test]
fn recycled_pid_identity_cannot_keep_a_slot() {
    let dir = TempDir::new("scheduler-pid");
    let mut s = setup(&dir, 1);
    let mut owner = Owner::read(std::process::id(), None).unwrap();
    owner.start_ticks += 1;
    s.hold("heavy", owner, "stale pid", None, "stale".into()).unwrap();
    poll(&mut s);
    assert!(s.slots.is_empty());
}

#[test]
fn a_holder_dying_during_exec_recovery_releases_its_inherited_descriptor() {
    let dir = TempDir::new("scheduler-dead-during-handover");
    let mut scheduler = setup(&dir, 1);
    let worker = Sleeping::new();
    scheduler.hold("heavy", worker.owner(1), "dies during exec", None, "handover-race".into()).unwrap();
    poll(&mut scheduler);
    let saved = scheduler.saved();
    let settings = scheduler.settings.clone();
    let fds: Vec<_> = scheduler
        .descriptors()
        .into_iter()
        .map(|(id, fd)| (id, unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) }))
        .collect();
    drop(scheduler);
    drop(worker);
    let mut restored = Scheduler::load(settings, dir.path(), true);
    restored.restore(saved, &fds);
    assert!(restored.snapshot().holders.is_empty());
    let lock = File::options().read(true).write(true).open(dir.path().join("slots/heavy/slot-0.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "unadopted inherited lease must close"
    );
}

#[test]
fn explicit_repository_identity_wins_over_discovery_and_conflicting_aliases_are_visible() {
    let dir = TempDir::new("scheduler-explicit-repository");
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    let mut settings = Settings::default();
    settings.repositories.insert("a-explicit".into(), dir.path().to_path_buf());
    settings.add_discovered_repository("z-shallow-derived".into(), dir.path().to_path_buf());
    assert_eq!(settings.repositories.len(), 1);
    let scheduler = Scheduler::new(settings.clone(), &dir.path().join("state"));
    assert_eq!(scheduler.repository_for(dir.path()).unwrap().0, "a-explicit");
    settings.repositories.insert("z-conflicting".into(), dir.path().to_path_buf());
    let scheduler = Scheduler::new(settings, &dir.path().join("conflict"));
    assert!(scheduler.snapshot().error.unwrap().contains("Conflicting repository mappings"));
    assert!(scheduler.repository_for(dir.path()).is_none());
    assert!(scheduler.snapshot().repositories.is_empty());
    assert!(!scheduler.snapshot().admitted);
    let sibling = TempDir::new("scheduler-explicit-sibling");
    let git = dir.path().join(".git/worktrees/sibling");
    fs::create_dir_all(&git).unwrap();
    fs::write(git.join("commondir"), "../..\n").unwrap();
    fs::write(sibling.path().join(".git"), format!("gitdir: {}\n", git.display())).unwrap();
    let mut settings = Settings::default();
    settings.repositories.insert("explicit".into(), dir.path().to_path_buf());
    settings.add_discovered_repository("shallow-sibling".into(), sibling.path().to_path_buf());
    assert_eq!(settings.repositories.len(), 1, "a sibling shares the explicit common-directory identity");
    settings.repositories.insert("conflicting-sibling".into(), sibling.path().to_path_buf());
    let scheduler = Scheduler::new(settings, &dir.path().join("common-conflict"));
    assert!(scheduler.snapshot().error.unwrap().contains("Conflicting repository mappings"));
    assert!(scheduler.repository_for(sibling.path()).is_none());
}

#[test]
fn inherited_dead_and_duplicate_leases_close_only_validated_descriptors() {
    let dir = TempDir::new("scheduler-inherited-validation");
    let mut scheduler = setup(&dir, 1);
    let worker = Sleeping::new();
    let id = scheduler.hold("heavy", worker.owner(1), "dead before recovery", None, "dead-inherited".into()).unwrap();
    poll(&mut scheduler);
    let saved = scheduler.saved();
    let fd = scheduler.descriptors()[0].1;
    let inherited = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    let unrelated = File::create(dir.path().join("unrelated")).unwrap();
    let unrelated_fd = unrelated.as_raw_fd();
    let lock_identity = std::fs::metadata(format!("/proc/self/fd/{fd}")).unwrap();
    let unrelated_identity = unrelated.metadata().unwrap();
    let settings = scheduler.settings.clone();
    drop(scheduler);
    drop(worker);
    let mut restored = Scheduler::load(settings, dir.path(), true);
    restored.restore(saved, &[(id, unrelated_fd), (id, inherited), (id, inherited), (id, duplicate)]);
    assert!(restored.snapshot().holders.is_empty());
    // Other test threads can reuse a closed raw FD immediately. Assert that
    // neither descriptor retains this exact lock, then prove capacity is free.
    for fd in [inherited, duplicate] {
        assert!(!std::fs::metadata(format!("/proc/self/fd/{fd}"))
            .is_ok_and(|m| m.dev() == lock_identity.dev() && m.ino() == lock_identity.ino()));
    }
    let untouched = unrelated.metadata().unwrap();
    assert_eq!(
        (untouched.dev(), untouched.ino()),
        (unrelated_identity.dev(), unrelated_identity.ino()),
        "unvalidated descriptors stay untouched"
    );
    let lock = File::options().read(true).write(true).open(dir.path().join("slots/heavy/slot-0.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
}

#[test]
fn repository_advertisements_reject_missing_relative_and_nondirectory_roots() {
    let dir = TempDir::new("scheduler-invalid-repository");
    fs::write(dir.path().join("file"), "not a directory").unwrap();
    for root in [PathBuf::from("relative"), dir.path().join("missing"), dir.path().join("file")] {
        let mut settings = Settings::default();
        settings.repositories.insert("invalid".into(), root);
        let scheduler = Scheduler::new(settings, &dir.path().join("state"));
        assert!(scheduler.snapshot().error.unwrap().contains("existing absolute directories"));
        assert!(scheduler.snapshot().repositories.is_empty());
    }
}
