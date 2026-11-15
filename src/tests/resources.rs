use super::*;

#[test]
fn cpu_use_is_the_busy_share_of_the_time_between_samples() {
    let before = parse_cpu("cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 1 2 3 4\n").unwrap();
    assert_eq!(before, CpuTimes { total: 1000, idle: 800 });
    let after = parse_cpu("cpu  250 0 150 1300 100 0 0 0 0 0\n").unwrap();
    assert_eq!(cpu_percent(before, after), Some(25.0), "200 busy ticks of 800");
    assert_eq!(cpu_percent(after, after), None, "no time passed");
    assert_eq!(cpu_percent(after, before), None, "a counter that went backwards");
    assert_eq!(parse_cpu("cpu0 1 2 3 4\n"), None, "only the all-CPU line counts");
    assert_eq!(parse_cpu("cpu  1 2\n"), None);
}

#[test]
fn memory_in_use_leaves_out_what_the_kernel_can_give_back() {
    let meminfo = "MemTotal:       32000000 kB\nMemFree:         1000000 kB\nMemAvailable:   24000000 kB\n";
    assert_eq!(memory_percent(meminfo), Some(25.0), "free plus reclaimable cache is available");
    assert_eq!(memory_percent("MemTotal: 0 kB\nMemAvailable: 0 kB\n"), None);
    assert_eq!(memory_percent("MemFree: 5 kB\n"), None);
}

#[test]
fn sizes_read_like_df() {
    let g = 1024u64 * 1024 * 1024;
    assert_eq!(short_size(312 * g + g / 3), "312G");
    assert_eq!(short_size(1500 * g), "1.5T");
    assert_eq!(short_size(5 * g / 2), "2.5G");
    assert_eq!(short_size(850 * 1024 * 1024), "850M");
}

#[test]
fn disk_use_counts_like_df() {
    // 1000 blocks of 4 KiB: 300 free, of which 50 are reserved for root, so
    // 700 are used and 250 are left for this user. df's Used column counts
    // the 700 and its Use% is 700 / (700 + 250); the line does the same.
    let d = disk_usage(1000, 300, 250, 4096);
    assert_eq!(d, DiskSpace { used: 700 * 4096, available: 250 * 4096 });
    assert_eq!(format!("{:.1}", d.percent_used().unwrap()), "73.7");
    assert_eq!(disk_usage(1000, 1000, 1000, 4096).percent_used(), Some(0.0), "an empty disk");
    assert_eq!(disk_usage(0, 0, 0, 4096).percent_used(), None, "no blocks at all");
    // A filesystem that reports more free than total blocks reads as empty.
    assert_eq!(disk_usage(10, 20, 20, 4096).used, 0);
}

#[test]
fn the_disk_and_a_live_sample_are_read_from_this_machine() {
    let d = disk_space(Path::new("/")).unwrap();
    assert!(d.used > 0, "the root filesystem is never empty");
    assert!(d.percent_used().is_some_and(|p| p > 0.0 && p <= 100.0), "{d:?}");
    assert_eq!(disk_space(Path::new("/no/such/cepheus/path")), None);
    let mut s = Sampler::default();
    s.sample(Path::new("/"));
    assert_eq!(s.now.cpu, None, "one sample has no interval yet");
    assert!(s.now.memory.is_some_and(|m| (0.0..=100.0).contains(&m)));
    s.last_cpu = Some(CpuTimes { total: 0, idle: 0 });
    s.sample(Path::new("/"));
    assert!(s.now.cpu.is_some_and(|c| (0.0..=100.0).contains(&c)));
}
