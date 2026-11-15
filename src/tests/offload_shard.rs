use super::*;
use crate::testsupport::TempDir;

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn load(cpus: u32, load1: f64, cap: usize, slots_free: usize) -> Load {
    Load { cpus, load1, mem_available_kib: 64 * 1024 * 1024, cap, slots_free }
}

/// Every (suite, target) appears in exactly one shard.
fn every_target_once(shards: &[Shard], estimates: &[Vec<f64>]) {
    let mut seen: Vec<(usize, usize)> = shards.iter().flat_map(|s| s.items.clone()).collect();
    seen.sort();
    let all: Vec<(usize, usize)> =
        estimates.iter().enumerate().flat_map(|(s, targets)| (0..targets.len()).map(move |t| (s, t))).collect();
    assert_eq!(seen, all);
}

#[test]
fn every_target_lands_in_exactly_one_shard() {
    let estimates = vec![vec![5.0, 1.0, 1.0, 30.0, 2.0], vec![10.0, 10.0]];
    let shards = split(&[2, 1], &[vec![0, 1], vec![0, 1]], &estimates);
    every_target_once(&shards, &estimates);
    assert_eq!(shards.len(), 3);
    assert_eq!(shards.iter().filter(|s| s.host == 0).count(), 2);
}

#[test]
fn the_longest_targets_are_dealt_first_to_the_least_loaded_shard() {
    let estimates = vec![vec![30.0, 20.0, 10.0, 10.0, 10.0]];
    let shards = split(&[1, 1], &[vec![0, 1]], &estimates);
    let totals: Vec<f64> = shards.iter().map(|s| s.estimate).collect();
    assert_eq!(totals, [40.0, 40.0]);
    every_target_once(&shards, &estimates);
}

#[test]
fn a_small_selection_spreads_across_hosts_first() {
    let shards = split(&[3, 3], &[vec![0, 1]], &[vec![1.0, 1.0]]);
    let hosts: Vec<usize> = shards.iter().map(|s| s.host).collect();
    assert_eq!(hosts, [0, 1]);
}

#[test]
fn a_suite_goes_only_to_hosts_whose_setup_succeeded() {
    let estimates = vec![vec![1.0; 6], vec![1.0; 4]];
    let shards = split(&[2, 2], &[vec![0, 1], vec![1]], &estimates);
    every_target_once(&shards, &estimates);
    for shard in &shards {
        if shard.items.iter().any(|(suite, _)| *suite == 1) {
            assert_eq!(shard.host, 1);
        }
    }
}

#[test]
fn a_suite_no_host_can_run_stays_unplaced_for_the_report() {
    assert!(split(&[1], &[vec![]], &[vec![1.0, 2.0]]).is_empty());
}

#[test]
fn shares_follow_free_slots_and_a_named_host_takes_at_least_one() {
    let idle = load(8, 0.0, 4, 4);
    let busy = load(4, 14.0, 2, 2);
    assert_eq!(slots(&[idle, busy], false), [4, 0]);
    assert_eq!(slots(&[idle, busy], true), [4, 1]);
}

#[test]
fn with_no_slot_to_spare_the_first_usable_host_takes_one_and_waits() {
    // Offloaded work holds every slot of one host; the other is busy with its own work.
    let queued = load(4, 2.0, 2, 0);
    let busy = load(6, 57.0, 3, 3);
    assert_eq!(slots(&[busy, queued], false), [0, 1]);
    assert_eq!(slots(&[busy], false), [0]);
}

#[test]
fn an_unknown_target_is_expected_to_take_the_median_of_known_ones() {
    let mut history = History::default();
    assert_eq!(history.estimate(Runner::Pytest, "a.py"), 10.0);
    history.seconds.insert("pytest a.py".into(), 2.0);
    history.seconds.insert("pytest b.py".into(), 4.0);
    history.seconds.insert("pytest c.py".into(), 9.0);
    history.seconds.insert("unittest x".into(), 100.0);
    assert_eq!(history.estimate(Runner::Pytest, "b.py"), 4.0);
    assert_eq!(history.estimate(Runner::Pytest, "new.py"), 4.0);
    assert_eq!(history.estimate(Runner::Unittest, "y"), 100.0);
}

#[test]
fn a_shards_time_is_shared_by_expectation_and_averaged_in() {
    let mut history = History::default();
    history.record(Runner::Unittest, &[("a".into(), 1.0), ("b".into(), 3.0)], 8.0);
    assert_eq!(history.seconds["unittest a"], 2.0);
    assert_eq!(history.seconds["unittest b"], 6.0);
    history.record(Runner::Unittest, &[("a".into(), 2.0)], 4.0);
    assert_eq!(history.seconds["unittest a"], 3.0);
}

#[test]
fn the_history_survives_a_save_and_load() {
    let dir = TempDir::new("offload-history");
    let path = dir.path().join("durations.json");
    assert_eq!(History::load(&path), History::default());
    let mut history = History::default();
    history.record(Runner::Django, &[("core".into(), 1.0)], 5.0);
    history.save(&path).unwrap();
    assert_eq!(History::load(&path), history);
}

#[test]
fn shard_takes_either_cis_selection_or_a_runner_and_targets() {
    let ci = parse_args(&argv(&["--select", "ci", "--base", "origin/main", "--host", "a,b"])).unwrap();
    assert_eq!(
        ci,
        ShardArgs {
            host: "a,b".into(),
            select: Some("ci".into()),
            base: Some("origin/main".into()),
            ..ShardArgs::default()
        }
    );
    let given = parse_args(&argv(&["--runner", "pytest", "--env", "A=1", "x.py", "y.py", "--", "-x"])).unwrap();
    assert_eq!(given.host, "auto");
    assert_eq!(given.runner, Some(Runner::Pytest));
    assert_eq!(given.environment, argv(&["A=1"]));
    assert_eq!(given.targets, argv(&["x.py", "y.py"]));
    assert_eq!(given.extra, argv(&["-x"]));
    for bad in [
        &["x.py"][..],
        &["--select", "all"],
        &["--select", "ci", "--runner", "pytest"],
        &["--select", "ci", "x.py"],
        &["--runner", "pytest", "--base", "m", "x"],
        &["--runner", "nose", "x"],
        &["--runner", "pytest", "--frobnicate"],
    ] {
        assert!(parse_args(&argv(bad)).is_err(), "{bad:?}");
    }
}

#[test]
fn select_ci_needs_a_plan_command_and_says_which_setting() {
    let ci = parse_args(&argv(&["--select", "ci"])).unwrap();
    let err = format!("{:#}", shard(&ci, &OffloadConfig::default()).unwrap_err());
    assert!(err.contains("--select ci needs the offload.plan setting"), "{err}");
    let cfg = OffloadConfig { plan: argv(&["plan", "--paths-from", "-"]), ..OffloadConfig::default() };
    assert_eq!(plan_command(&cfg).unwrap().to_vec(), argv(&["plan", "--paths-from", "-"]));
}

#[test]
fn the_plan_runs_in_the_python_environment_inside_the_wrap_reading_the_paths() {
    use std::io::Write;
    let home = TempDir::new("offload-plan-wrap");
    home.write("m/.cepheus-offload/keep", "");
    home.executable("bin/podman", super::super::tests::FAKE_EXEC);
    let path = format!("{}:{}", home.path().join("bin").display(), std::env::var("PATH").unwrap());
    let wrap = argv(&["sh", "-c", "echo \"$0 $VIRTUAL_ENV\"; exec \"$@\"", "@STATE@/scratch"]);
    let command = argv(&["cat"]);
    let mut child = Command::new("bash")
        .args(["-c", PLAN, "cepheus-offload"])
        .args(plan_args("m", &wrap, &command))
        .env("HOME", home.path())
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"a.py\0b.py\0").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "/scratch/source/.cepheus-offload/state/scratch /scratch/source/.venv\na.py\0b.py\0"
    );
}

#[test]
fn a_plan_names_its_suites_and_what_it_leaves_to_ci() {
    let plan: Plan = serde_json::from_str(
        r#"{"version":1,
            "suites":[{"name":"workflow-tests","runner":"unittest","targets":["tools.tests.test_a"],
                       "environment":{"DEBUG":"True","B":"2"},"setup":[["python","x.py"]]}],
            "not_run":[{"name":"browser-tests","reason":"needs a browser","command":"ci-local --phase browser"}]}"#,
    )
    .unwrap();
    assert_eq!(plan.suites[0].environment, ["B=2", "DEBUG=True"]);
    assert_eq!(plan.suites[0].runner, Runner::Unittest);
    assert_eq!(plan.suites[0].setup, [argv(&["python", "x.py"])]);
    assert_eq!(plan.not_run[0].name, "browser-tests");
}
