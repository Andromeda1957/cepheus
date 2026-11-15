use std::path::PathBuf;

use super::*;
use crate::testsupport::TempDir;

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

#[test]
fn a_program_runs_from_its_words_with_its_folder_and_environment() {
    let dir = TempDir::new("run-words");
    // Shell syntax in a word is only text.
    let ran = run(
        &argv(&["printf", "%s", "$(touch made); `touch made` > made"]),
        Some(dir.path()),
        &[],
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(ran.stdout, "$(touch made); `touch made` > made");
    assert!(!dir.path().join("made").exists());
    assert!(ran.ok());

    let env = [("DECK_TEST_VALUE".to_string(), "seen".to_string())];
    let ran = run(
        &argv(&["sh", "-c", "pwd; echo $DECK_TEST_VALUE; echo oops >&2; exit 4"]),
        Some(dir.path()),
        &env,
        Duration::from_secs(10),
    )
    .unwrap();
    let here = dir.path().canonicalize().unwrap();
    assert_eq!(ran.stdout, format!("{}\nseen\n", here.display()));
    assert_eq!((ran.stderr.as_str(), ran.code, ran.ok()), ("oops\n", Some(4), false));
}

#[test]
fn a_program_reads_no_input() {
    let started = Instant::now();
    let ran = run_args(&["cat"], None, Duration::from_secs(10)).unwrap();
    assert!(ran.ok() && ran.stdout.is_empty());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn the_time_limit_ends_the_program_and_everything_it_started() {
    let mut observations = Vec::new();
    for command in ["sleep 30 & sleep 30", "sleep 2 &", "setsid sleep 2 &"] {
        let started = Instant::now();
        // A descendant keeps output open even after its group leader exits.
        let ran = run_args(&["sh", "-c", command], None, Duration::from_millis(300)).unwrap();
        observations.push((command, ran, started.elapsed(), true));
    }

    let dir = TempDir::new("run-cancel");
    let marker = dir.path().join("leader");
    let observed = marker.clone();
    let token = Arc::new(AtomicBool::new(false));
    let cancel = token.clone();
    let canceller = thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(10);
        let exited = loop {
            let gone = std::fs::read_to_string(&observed)
                .ok()
                .and_then(|pid| {
                    pid.parse::<u32>().ok().map(|pid| match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                        Ok(stat) => stat.rsplit_once(") ").is_some_and(|(_, fields)| fields.starts_with('Z')),
                        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
                    })
                })
                .unwrap_or(false);
            if gone || Instant::now() >= until {
                break gone;
            }
            thread::sleep(Duration::from_millis(10));
        };
        cancel.store(true, Ordering::SeqCst);
        exited
    });
    let started = Instant::now();
    let ran = with_cancellation(token, || {
        run_args(
            &["sh", "-c", "printf '%s' \"$$\" > \"$1\"; sleep 30 &", "sh", &marker.to_string_lossy()],
            None,
            Duration::from_secs(10),
        )
    })
    .unwrap();
    assert!(canceller.join().unwrap(), "the group leader did not exit");
    observations.push(("cancel after leader exit", ran, started.elapsed(), false));

    assert!(
        observations.iter().all(|(_, ran, _, timed_out)| ran.timed_out == *timed_out && ran.code.is_none()),
        "{observations:?}"
    );
    for (_, ran, elapsed, timed_out) in observations {
        assert!(elapsed < Duration::from_secs(10), "waited {elapsed:?}");
        if timed_out {
            assert_eq!(ran.reason(), "it took too long and was stopped");
        }
    }
}

#[test]
fn output_is_capped() {
    let ran = run_args(&["head", "-c", "1000000", "/dev/zero"], None, Duration::from_secs(20)).unwrap();
    assert!(ran.ok());
    assert_eq!(ran.stdout.len(), OUTPUT_CAP);
}

#[test]
fn nothing_to_run_is_an_error() {
    assert!(run(&[], None, &[], Duration::from_secs(1)).is_err());
    assert!(run_args(&["/nonexistent/deck-program"], None, Duration::from_secs(1)).is_err());
}

#[test]
fn the_reason_is_the_last_line_it_printed_about_a_failure() {
    let ran = |stdout: &str, stderr: &str, code: Option<i32>| Ran {
        code,
        stdout: stdout.into(),
        stderr: stderr.into(),
        timed_out: false,
    };
    assert_eq!(ran("progress\n", "warning\nfatal: no such repo\n\n", Some(128)).reason(), "fatal: no such repo");
    assert_eq!(ran("only here\n", "  \n", Some(1)).reason(), "only here");
    assert_eq!(ran("", "", Some(2)).reason(), "it exited with status 2");
    assert_eq!(ran("", "", None).reason(), "it was ended by a signal");
}

#[test]
fn git_gives_its_output_only_when_it_succeeds() {
    let dir = TempDir::new("run-git");
    run_args(&["git", "init", "-q", &dir.path().to_string_lossy()], None, Duration::from_secs(10)).unwrap();
    let top = git(dir.path(), &["rev-parse", "--show-toplevel"], Duration::from_secs(10));
    assert_eq!(top.map(|t| PathBuf::from(t.trim())), Some(dir.path().canonicalize().unwrap()));
    assert_eq!(git(dir.path(), &["rev-parse", "--verify", "--quiet", "no-such-branch"], Duration::from_secs(10)), None);
}
