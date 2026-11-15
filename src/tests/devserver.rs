use super::*;
use crate::testsupport::TempDir;

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

#[test]
fn the_port_comes_from_the_last_address_argument() {
    assert_eq!(port_of(&argv(&[".venv/bin/python", "manage.py", "runserver", "0.0.0.0:8000"])), Some(8000));
    assert_eq!(port_of(&argv(&["server", "--port", "9001", "--verbose"])), Some(9001));
    assert_eq!(port_of(&argv(&["python", "manage.py", "runserver"])), None);
    assert_eq!(port_of(&argv(&["8000"])), None, "the program name is not an argument");
    assert_eq!(port_of(&argv(&["x", "[::]:70000"])), None);
}

#[test]
fn listening_sockets_are_read_from_the_kernel_table() {
    let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1 0000000000000000 100 0 0 10 0\n\
   1: 0100007F:2328 0100007F:A1B2 01 00000000:00000000 00:00000000 00000000  1000        0 2 1 0000000000000000 20 4 30 10 -1\n";
    assert!(table_listens(table, 8000));
    assert!(!table_listens(table, 9000), "an established connection on 9000 is not a listener");
    assert!(!table_listens(table, 8001));
    assert!(!table_listens("", 8000));
}

#[test]
fn a_server_that_fails_reports_its_status_and_log() {
    let dir = TempDir::new("dev-fail");
    let log = dir.path().join("dev.log");
    let cmd = argv(&["sh", "-c", "echo starting; echo 'Error: That port is already in use.' >&2; exit 3"]);
    let mut dev = ManagedProcess::start(&cmd, dir.path(), &log, &[]).unwrap();
    let ended = (0..200).find_map(|_| {
        std::thread::sleep(Duration::from_millis(20));
        dev.poll(Instant::now())
    });
    match ended {
        Some(Ended::Failed { status, tail, .. }) => {
            assert_eq!(status.code(), Some(3));
            assert_eq!(tail, "starting\nError: That port is already in use.");
        }
        _ => panic!("the failed server was not reported"),
    }
}

#[test]
fn stopping_ends_the_whole_process_group() {
    let dir = TempDir::new("dev-stop");
    let log = dir.path().join("dev.log");
    let pid_file = dir.path().join("child.pid");
    // A parent that ignores nothing and a child it started, as Django's
    // autoreloader runs the real server in a child process.
    let script = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
    let mut dev = ManagedProcess::start(&argv(&["sh", "-c", &script]), dir.path(), &log, &[]).unwrap();
    let child: i32 = (0..200)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(10));
            fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse().ok())
        })
        .expect("the child started");
    assert!(dev.poll(Instant::now()).is_none(), "still running");
    dev.stop();
    assert!(dev.stopping());
    let ended = (0..300).find_map(|_| {
        std::thread::sleep(Duration::from_millis(20));
        dev.poll(Instant::now())
    });
    assert!(matches!(ended, Some(Ended::Stopped)), "a stop is not a failure");
    let child_gone = (0..100).any(|_| {
        std::thread::sleep(Duration::from_millis(10));
        // SAFETY: signal 0 only checks that the process exists.
        let exists = unsafe { libc::kill(child, 0) } == 0;
        let zombie = fs::read_to_string(format!("/proc/{child}/stat"))
            .map(|s| s.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')))
            .unwrap_or(true);
        !exists || zombie
    });
    assert!(child_gone, "the server's child process {child} outlived the stop");
}

#[test]
fn a_relative_program_is_found_in_the_working_directory() {
    let dir = TempDir::new("dev-relative");
    // A link, as `.venv/bin/python` is: writing a script here could race a
    // fork in another test thread and fail to start with "text file busy".
    fs::create_dir_all(dir.path().join(".venv/bin")).unwrap();
    std::os::unix::fs::symlink("/bin/sh", dir.path().join(".venv/bin/server")).unwrap();
    let log = dir.path().join("dev.log");
    let cmd = argv(&[".venv/bin/server", "-c", "echo \"ran in $(pwd -P)\"; exit 4"]);
    let mut dev = ManagedProcess::start(&cmd, dir.path(), &log, &[]).unwrap();
    let ended = (0..200).find_map(|_| {
        std::thread::sleep(Duration::from_millis(20));
        dev.poll(Instant::now())
    });
    match ended {
        Some(Ended::Failed { tail, .. }) => {
            assert_eq!(tail, format!("ran in {}", dir.path().canonicalize().unwrap().display()))
        }
        _ => panic!("the server did not run"),
    }
}

#[test]
fn a_server_handed_over_by_the_previous_build_is_still_watched() {
    let dir = TempDir::new("dev-adopt");
    let log = dir.path().join("dev.log");
    let cmd = argv(&["sh", "-c", "echo up; sleep 0.3; echo 'Error: gone' >&2; exit 5"]);
    let first = ManagedProcess::start(&cmd, dir.path(), &log, &[]).unwrap();
    // What the handover carries; the old build's value is dropped, as exec
    // discards it, while the server keeps running.
    let (pid, started_at) = (first.pid, first.started_at - 100);
    drop(first);
    let mut dev = ManagedProcess::adopt(pid, started_at, log.clone(), false);
    assert!(!dev.stopping());
    let ended = (0..300).find_map(|_| {
        std::thread::sleep(Duration::from_millis(20));
        dev.poll(Instant::now())
    });
    match ended {
        Some(Ended::Failed { status, ran, tail }) => {
            assert_eq!(status.code(), Some(5));
            assert!(ran >= Duration::from_secs(100), "the run time counts from the original start: {ran:?}");
            assert_eq!(tail, "up\nError: gone");
        }
        _ => panic!("the adopted server's exit was not reported as a failure"),
    }

    // A server that was stopping when handed over still ends as a stop.
    let mut dev = ManagedProcess::start(&argv(&["sleep", "30"]), dir.path(), &log, &[]).unwrap();
    dev.stop();
    let mut dev = ManagedProcess::adopt(dev.pid, dev.started_at, log, true);
    assert!(dev.stopping());
    let ended = (0..300).find_map(|_| {
        std::thread::sleep(Duration::from_millis(20));
        dev.poll(Instant::now())
    });
    assert!(matches!(ended, Some(Ended::Stopped)));
}
