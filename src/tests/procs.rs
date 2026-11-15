use super::*;

fn info(pid: u32, ppid: u32) -> ProcInfo {
    ProcInfo { pid, ppid, comm: format!("p{pid}"), state: 'S', start_ticks: 0 }
}

#[test]
fn parse_stat_handles_spaces_and_parens_in_comm() {
    let text = "4242 (my (odd) prog) S 17 4242 4242 34816 4242 4194560 1 0 0 0 5 6 0 0 20 0 3 0 987654 1000 200 18446744073709551615";
    let p = parse_stat(4242, text).unwrap();
    assert_eq!(p.comm, "my (odd) prog");
    assert_eq!(p.state, 'S');
    assert_eq!(p.ppid, 17);
    assert_eq!(p.start_ticks, 987654);
    assert!(parse_stat(1, "1 (init) S 0").is_none());
}

#[test]
fn own_process_is_visible_and_alive() {
    let me = std::process::id();
    let p = stat(me).unwrap();
    assert!(alive_with_start(me, Some(p.start_ticks)));
    assert!(alive_with_start(me, None));
    assert!(!alive_with_start(me, Some(p.start_ticks + 1)), "a reused pid has a different start time");
    assert!(scan().contains_key(&me));
    assert!(!cmdline(me).is_empty());
    assert_eq!(cwd(me), std::env::current_dir().ok());
    assert!(environ(me).contains_key("PATH") || std::env::var_os("PATH").is_none());
    let started = start_secs(p.start_ticks);
    let now = crate::util::now_secs();
    assert!(started <= now && now - started < 24 * 3600, "{started} vs {now}");
}

#[test]
fn ancestry_walks() {
    let procs: HashMap<u32, ProcInfo> =
        [info(10, 1), info(20, 10), info(30, 20), info(31, 20), info(40, 1)].into_iter().map(|p| (p.pid, p)).collect();
    assert!(is_descendant(&procs, 30, 10));
    assert!(is_descendant(&procs, 20, 10));
    assert!(!is_descendant(&procs, 40, 10));
    assert!(!is_descendant(&procs, 99, 10));
    let mut d = descendants(&procs, 10);
    d.sort_unstable();
    assert_eq!(d, vec![20, 30, 31]);
    assert!(descendants(&procs, 40).is_empty());
}

#[test]
fn ancestry_survives_cycles() {
    let procs: HashMap<u32, ProcInfo> = [info(5, 6), info(6, 5)].into_iter().map(|p| (p.pid, p)).collect();
    assert!(!is_descendant(&procs, 5, 99));
    let mut tree = program_tree(&procs, 5, "p5");
    tree.sort_unstable();
    assert_eq!(tree, vec![5, 6]);
}

#[test]
fn a_program_tree_leaves_out_another_copy_it_runs() {
    // A hosted Codex that runs `codex exec` as a tool: that copy writes a
    // conversation of its own, which is not the session's.
    let named = |pid: u32, ppid: u32, comm: &str| ProcInfo { comm: comm.into(), ..info(pid, ppid) };
    let procs: HashMap<u32, ProcInfo> = [
        named(10, 1, "sh"),
        named(11, 10, "codex"),
        named(12, 11, "node"),
        named(13, 11, "bash"),
        named(14, 13, "codex"),
        named(15, 14, "bash"),
        named(20, 1, "codex"),
    ]
    .into_iter()
    .map(|p| (p.pid, p))
    .collect();
    let tree = |root: u32| {
        let mut t = program_tree(&procs, root, "codex");
        t.sort_unstable();
        t
    };
    assert_eq!(tree(10), vec![10, 11, 12, 13], "a launcher may run the program as its child");
    assert_eq!(tree(11), vec![11, 12, 13]);
    assert_eq!(tree(99), vec![99], "a process the scan missed is still its own tree");
}

#[test]
fn only_descriptors_open_for_writing_count_as_written() {
    // Review CR-20261026-d5c080aeb2: a log reader such as `tail -f` holds a
    // conversation log open too. Codex opens its own read-write and append
    // (`flags: 02102002`, read live from the shared Codex server).
    assert!(fdinfo_writable("pos:\t0\nflags:\t02102002\nmnt_id:\t31\n"));
    assert!(fdinfo_writable("flags:\t0100001\n"), "write-only");
    assert!(!fdinfo_writable("pos:\t0\nflags:\t0100000\n"), "read-only");
    assert!(!fdinfo_writable("pos:\t0\n"), "no flags line");
    assert!(!fdinfo_writable("flags:\tzz\n"));

    let dir = crate::testsupport::TempDir::new("procs-written");
    let read = dir.write("read.jsonl", "{}\n");
    let appended = dir.write("appended.jsonl", "{}\n");
    let _read = fs::File::open(&read).unwrap();
    let _appended = fs::OpenOptions::new().append(true).open(&appended).unwrap();
    // Descriptor targets are resolved paths, even under an aliased TMPDIR.
    let real = fs::canonicalize(dir.path()).unwrap();
    let mine = |p: &Path| p.starts_with(&real);
    assert_eq!(files_open_for_writing(std::process::id(), mine), vec![fs::canonicalize(&appended).unwrap()]);
    assert!(files_open_for_writing(u32::MAX - 1, mine).is_empty(), "a process that is gone holds nothing");
}
