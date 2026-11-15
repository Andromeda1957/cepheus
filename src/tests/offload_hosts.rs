use std::os::fd::AsRawFd;
use std::process::Command;

use super::*;
use crate::testsupport::TempDir;

fn load(cpus: u32, load1: f64, gib: u64, cap: usize, slots_free: usize) -> Load {
    Load { cpus, load1, mem_available_kib: gib * 1024 * 1024, cap, slots_free }
}

fn host(name: &str) -> BuildHost {
    BuildHost { name: name.into(), ssh: format!("andro@{name}") }
}

fn names(ranked: &[(BuildHost, Load)]) -> Vec<&str> {
    ranked.iter().map(|(host, _)| host.name.as_str()).collect()
}

#[test]
fn a_probe_answer_gives_cores_load_memory_and_slots() {
    let out = "8\n1.50 1.20 0.90 2/345 6789\nMemAvailable:   16384000 kB\nslots 4 3\n";
    assert_eq!(
        parse_probe(out),
        Some(Load { cpus: 8, load1: 1.5, mem_available_kib: 16384000, cap: 4, slots_free: 3 })
    );
}

#[test]
fn a_probe_answer_missing_anything_is_no_answer() {
    assert_eq!(parse_probe(""), None);
    assert_eq!(parse_probe("8\n1.5 1 1 1/1 1\nMemAvailable: 100 kB\n"), None);
    assert_eq!(parse_probe("0\n1.5 1 1 1/1 1\nMemAvailable: 100 kB\nslots 1 1\n"), None);
    assert_eq!(parse_probe("8\n1.5 1 1 1/1 1\nMemAvailable: 100 kB\nslots 4 5\n"), None);
    assert_eq!(parse_probe("8\nnan 1 1 1/1 1\nMemAvailable: 100 kB\nslots 4 4\n"), None);
}

#[test]
fn the_probe_counts_slots_other_work_holds() {
    let home = TempDir::new("offload-probe");
    let run = || {
        let out = Command::new("bash").args(["-c", PROBE]).env("HOME", home.path()).output().unwrap();
        parse_probe(&String::from_utf8_lossy(&out.stdout)).expect("a usable answer")
    };
    let idle = run();
    assert_eq!(idle.cap, (idle.cpus as usize / 2).max(1));
    assert_eq!(idle.slots_free, idle.cap);
    let slot = std::fs::File::create(home.path().join(".cache/cepheus-offload/slots/0")).unwrap();
    // SAFETY: flock on a descriptor this test owns.
    assert_eq!(unsafe { libc::flock(slot.as_raw_fd(), libc::LOCK_EX) }, 0);
    assert_eq!(run().slots_free, idle.cap - 1);
    drop(slot);
    assert_eq!(run().slots_free, idle.cap);
}

#[test]
fn offloaded_work_keeps_to_half_a_hosts_cores() {
    // An idle 8-core host takes 4 shards; one already loaded by 2.5 takes 1.
    assert_eq!(load(8, 0.0, 32, 4, 4).slots(), 4);
    assert_eq!(load(8, 2.5, 32, 4, 4).slots(), 1);
    // Slots other callers hold are not offered again.
    assert_eq!(load(8, 0.0, 32, 4, 1).slots(), 1);
    // Nor more than free memory holds at a gibibyte each.
    assert_eq!(load(8, 0.0, 2, 4, 4).slots(), 2);
}

#[test]
fn a_host_busy_with_its_own_work_is_not_for_auto() {
    // Fedora at load 57 on 6 cores: the owner's agents.
    assert!(!load(6, 57.0, 8, 3, 3).eligible());
    assert_eq!(load(6, 57.0, 8, 3, 3).slots(), 0);
    // Half its cores busy with its own work is too busy.
    assert!(!load(6, 3.0, 8, 3, 3).eligible());
    assert!(load(6, 2.9, 8, 3, 3).eligible());
    // Load from offloaded work holding the slots does not count against it;
    // new work waits for those slots instead.
    let offloaded = load(4, 2.4, 8, 2, 0);
    assert!(offloaded.eligible());
    assert_eq!(offloaded.slots(), 0);
}

#[test]
fn usable_idle_hosts_with_the_most_free_cores_come_first() {
    let ranked = rank(vec![
        (host("busy"), load(6, 5.0, 8, 3, 3)),
        (host("queued"), load(8, 4.0, 8, 4, 0)),
        (host("small"), load(4, 0.5, 8, 2, 2)),
        (host("big"), load(8, 1.0, 8, 4, 4)),
    ]);
    assert_eq!(names(&ranked), ["big", "small", "queued", "busy"]);
    let even = rank(vec![(host("a"), load(4, 0.0, 8, 2, 2)), (host("b"), load(4, 0.0, 8, 2, 2))]);
    assert_eq!(names(&even), ["a", "b"]);
}

#[test]
fn auto_never_picks_a_host_busy_with_its_own_work() {
    let busy = vec![(host("fedora"), load(6, 57.0, 8, 3, 3)), (host("ubuntu"), load(4, 14.0, 16, 2, 2))];
    let err = pick(rank(busy.clone()), false).unwrap_err().to_string();
    assert!(err.contains("fedora") && err.contains("ubuntu"), "{err}");
    // A host named on the command line is used; its slots make the command wait its turn.
    assert_eq!(pick(rank(busy), true).unwrap().0.name, "ubuntu");
    let mixed = vec![(host("fedora"), load(6, 57.0, 8, 3, 3)), (host("wsl"), load(8, 1.0, 8, 4, 4))];
    assert_eq!(pick(rank(mixed), false).unwrap().0.name, "wsl");
}

#[test]
fn hosts_are_chosen_by_name_alias_or_destination() {
    let configured = vec![
        BuildHost { name: "build-worker".into(), ssh: "builder@192.0.2.10".into() },
        BuildHost { name: "windows-builder".into(), ssh: "wsl-windows-builder".into() },
    ];
    assert_eq!(choose("auto", &configured).unwrap(), configured);
    let chosen = choose("WSL-windows-builder, build-worker,other@box,windows-builder", &configured).unwrap();
    assert_eq!(
        chosen,
        vec![
            configured[1].clone(),
            configured[0].clone(),
            BuildHost { name: "other@box".into(), ssh: "other@box".into() }
        ]
    );
    assert!(choose("auto", &[]).is_err());
    assert!(choose(" , ", &configured).is_err());
}
