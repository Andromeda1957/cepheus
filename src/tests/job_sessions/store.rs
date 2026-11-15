use super::*;
use crate::testsupport::TempDir;
use std::os::unix::fs::MetadataExt;
fn spec(dir: &TempDir) -> Spec {
    Spec {
        argv: vec!["printf".into(), "private-secret".into()],
        cwd: dir.path().to_path_buf(),
        stdin: "private-secret".into(),
        env: BTreeMap::from([("PRIVATE".into(), "private-secret".into())]),
        ..Spec::default()
    }
}
#[test]
fn accepted_intent_survives_lost_ack_without_exposing_argv_or_input() {
    let dir = TempDir::new("job-intent");
    let mut jobs = Jobs::load(dir.path()).unwrap();
    let input = spec(&dir);
    let original = jobs.add("repeat-key".into(), "build".into(), input.clone(), "local".into(), None).unwrap();
    drop(jobs);
    let mut recovered = Jobs::load(dir.path()).unwrap();
    let duplicate = recovered.add("repeat-key".into(), "build".into(), input, "local".into(), None).unwrap();
    assert_eq!(original.id, duplicate.id);
    assert_eq!(recovered.snapshot().len(), 1);
    assert!(!serde_json::to_string(&recovered.snapshot()).unwrap().contains("private-secret"));
    let mut changed = spec(&dir);
    changed.argv.push("different".into());
    assert!(recovered.add("repeat-key".into(), "build".into(), changed, "local".into(), None).is_err());
}
#[test]
fn lost_supervisor_is_unknown_and_never_a_success_or_a_new_spawn() {
    let dir = TempDir::new("job-lost");
    let mut jobs = Jobs::load(dir.path()).unwrap();
    let info = jobs.add("lost".into(), "test".into(), spec(&dir), "local".into(), None).unwrap();
    let own = Identity::read(std::process::id()).unwrap();
    let mut dead = own.clone();
    dead.start += 1;
    store::write(
        &jobs.dir(info.id).join("launch.json"),
        &Launch {
            nonce: jobs.records[&info.id].intent.nonce.clone(),
            supervisor: dead,
            child: own.clone(),
            pgid: own.pid,
            started_at: 1,
        },
    )
    .unwrap();
    jobs.refresh().unwrap();
    let result = &jobs.snapshot()[0];
    assert_eq!(result.status, "failed");
    assert_eq!(result.exit_code, None);
    assert!(result.reason.contains("unknown"));
    assert!(jobs.spawned(info.id).unwrap());
}
#[test]
fn terminal_receipt_survives_reload_with_exact_code_and_stop_preserves_it() {
    let dir = TempDir::new("job-exit");
    let mut jobs = Jobs::load(dir.path()).unwrap();
    let info = jobs.add("exit".into(), "test".into(), spec(&dir), "local".into(), None).unwrap();
    let path = jobs.dir(info.id);
    store::write(
        &path.join("exit.json"),
        &Exit {
            nonce: jobs.records[&info.id].intent.nonce.clone(),
            code: Some(23),
            signal: None,
            finished_at: crate::util::now_secs(),
            stopped: false,
            error: None,
        },
    )
    .unwrap();
    drop(jobs);
    let mut jobs = Jobs::load(dir.path()).unwrap();
    assert_eq!(jobs.snapshot()[0].exit_code, Some(23));
    assert_eq!(jobs.stop(info.id).unwrap().exit_code, Some(23));
    assert_eq!(fs::metadata(path.join("exit.json")).unwrap().mode() & 0o777, 0o600);
    jobs.dismiss(info.id).unwrap();
    assert!(Jobs::load(dir.path()).unwrap().snapshot().is_empty());
}
#[test]
fn private_log_rejects_symlinks_and_preserves_existing_bytes() {
    use std::io::Write;
    use std::os::unix::fs::symlink;
    let dir = TempDir::new("job-private-log");
    let path = dir.path().join("output");
    store::append_log(&path).unwrap().write_all(b"first").unwrap();
    store::append_log(&path).unwrap().write_all(b"second").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"firstsecond");
    let alias = dir.path().join("alias");
    symlink(&path, &alias).unwrap();
    assert!(store::append_log(&alias).is_err());
    assert!(store::open_log(&alias).is_err());
}
#[test]
fn cancellation_before_remote_acceptance_survives_restart_and_never_launches() {
    let dir = TempDir::new("job-pre-cancel");
    let mut jobs = Jobs::load(dir.path()).unwrap();
    assert!(jobs.cancel_key("remote-key").unwrap().is_none());
    drop(jobs);
    let mut jobs = Jobs::load(dir.path()).unwrap();
    let result = jobs.add("remote-key".into(), "test".into(), spec(&dir), "local".into(), None).unwrap();
    assert_eq!(result.status, "failed");
    assert!(result.reason.contains("cancelled"));
    assert!(jobs.spawned(result.id).unwrap());
    assert_eq!(jobs.add("remote-key".into(), "test".into(), spec(&dir), "local".into(), None).unwrap(), result);
}
#[test]
fn raw_page_cursor_and_chat_snapshot_are_independent_and_eof_waits_for_all_bytes() {
    use std::io::Write;
    let dir = TempDir::new("job-log-pages");
    let mut jobs = Jobs::load(dir.path()).unwrap();
    let job = jobs.add("pages".into(), "test".into(), spec(&dir), "local".into(), None).unwrap();
    store::append_log(&jobs.dir(job.id).join("output.log")).unwrap().write_all(b"first\nsecond\n").unwrap();
    let page = jobs.log(job.id, 0, 4).unwrap();
    assert_eq!(page["text"], "firs");
    assert_eq!(page["offset"], 4);
    assert_eq!(page["eof"], false);
    jobs.stop(job.id).unwrap();
    let partial = jobs.log(job.id, 4, 4).unwrap();
    assert_eq!(partial["offset"], 8);
    assert_eq!(partial["eof"], false);
    let final_page = jobs.log(job.id, 8, 100).unwrap();
    assert_eq!(final_page["eof"], true);
    assert!(jobs.log(job.id, 999, 1).is_err());
}
#[test]
fn repository_binding_and_exit_nonce_cannot_be_reassigned() {
    let dir = TempDir::new("job-binding");
    let mut jobs = Jobs::load(dir.path()).unwrap();
    let job = jobs.add("bound".into(), "test".into(), spec(&dir), "local".into(), Some("one".into())).unwrap();
    assert!(jobs.add("bound".into(), "test".into(), spec(&dir), "local".into(), Some("two".into())).is_err());
    store::write(
        &jobs.dir(job.id).join("exit.json"),
        &Exit { nonce: "foreign".into(), code: Some(0), signal: None, finished_at: 1, stopped: false, error: None },
    )
    .unwrap();
    assert!(jobs.refresh().is_err());
}
#[test]
fn byte_pages_preserve_unicode_across_json_record_boundaries() {
    use std::io::Write;
    let dir = TempDir::new("job-log-unicode");
    let mut jobs = Jobs::load(dir.path()).unwrap();
    let job = jobs.add("unicode".into(), "test".into(), spec(&dir), "local".into(), None).unwrap();
    let original = "ab🙂é\n{\"text\":\"世界\"}\n";
    store::append_log(&jobs.dir(job.id).join("output.log")).unwrap().write_all(original.as_bytes()).unwrap();
    jobs.stop(job.id).unwrap();
    let mut text = String::new();
    let mut offset = 0;
    loop {
        let page = jobs.log(job.id, offset, 4).unwrap();
        text.push_str(page["text"].as_str().unwrap());
        let next = page["offset"].as_u64().unwrap();
        assert!(next > offset);
        offset = next;
        if page["eof"] == true {
            break;
        }
    }
    assert_eq!(text, original);
    assert_eq!(offset, original.len() as u64);
}
#[test]
fn expired_output_is_removed_but_exact_idempotency_receipt_survives() {
    use std::io::Write;
    let dir = TempDir::new("job-retention");
    let mut jobs = Jobs::load(dir.path()).unwrap();
    let input = spec(&dir);
    let job = jobs.add("old".into(), "test".into(), input.clone(), "local".into(), None).unwrap();
    let path = jobs.dir(job.id);
    store::append_log(&path.join("output.log")).unwrap().write_all(b"private output").unwrap();
    store::append_log(&path.join("input")).unwrap().write_all(b"private input").unwrap();
    store::write(
        &path.join("exit.json"),
        &Exit {
            nonce: jobs.records[&job.id].intent.nonce.clone(),
            code: Some(7),
            signal: None,
            finished_at: crate::util::now_secs() - 2 * 86400,
            stopped: false,
            error: None,
        },
    )
    .unwrap();
    drop(jobs);
    let mut jobs = Jobs::load_with_retention(dir.path(), 1).unwrap();
    assert!(jobs.snapshot().is_empty());
    assert!(!path.join("output.log").exists());
    assert!(!path.join("input").exists());
    let repeated = jobs.add("old".into(), "test".into(), input, "local".into(), None).unwrap();
    assert_eq!(repeated.id, job.id);
    assert_eq!(repeated.exit_code, Some(7));
}
#[test]
fn unpublished_staging_intent_is_not_admitted_after_a_crash() {
    let dir = TempDir::new("job-staging");
    let mut jobs = Jobs::load(dir.path()).unwrap();
    let stage = jobs.root.join(".new-interrupted");
    store::directory(&stage).unwrap();
    store::write(&stage.join("uncommitted"), &"not admitted").unwrap();
    drop(jobs);
    jobs = Jobs::load(dir.path()).unwrap();
    assert!(jobs.records.is_empty());
    assert!(jobs.add("fresh".into(), "test".into(), spec(&dir), "local".into(), None).is_ok());
}
#[test]
fn live_partial_unicode_appends_hold_the_cursor_until_character_completion() {
    use std::io::Write;
    for character in ["é", "界", "🙂"] {
        for cut in 1..character.len() {
            let dir = TempDir::new("job-live-unicode");
            let mut jobs = Jobs::load(dir.path()).unwrap();
            let job = jobs.add(format!("unicode-{cut}"), "test".into(), spec(&dir), "local".into(), None).unwrap();
            let mut log = store::append_log(&jobs.dir(job.id).join("output.log")).unwrap();
            log.write_all(b"x").unwrap();
            log.write_all(&character.as_bytes()[..cut]).unwrap();
            let first = jobs.log(job.id, 0, 4).unwrap();
            assert_eq!(first["text"], "x");
            assert_eq!(first["offset"], 1);
            assert_eq!(first["eof"], false);
            log.write_all(&character.as_bytes()[cut..]).unwrap();
            log.write_all(b"\n").unwrap();
            drop(log);
            jobs.stop(job.id).unwrap();
            let mut text = String::from("x");
            let mut offset = 1;
            loop {
                let page = jobs.log(job.id, offset, 4).unwrap();
                text.push_str(page["text"].as_str().unwrap());
                offset = page["offset"].as_u64().unwrap();
                if page["eof"] == true {
                    break;
                }
            }
            assert_eq!(text, format!("x{character}\n"));
        }
    }
}
#[test]
fn retention_preserves_old_unknown_workloads_until_local_exit_or_remote_stop_ack() {
    let dir = TempDir::new("job-unknown-retention");
    let mut jobs = Jobs::load_with_retention(dir.path(), 1).unwrap();
    let local = jobs.add("old-local".into(), "test".into(), spec(&dir), "local".into(), None).unwrap();
    let own = Identity::read(std::process::id()).unwrap();
    let mut dead = own.clone();
    dead.start += 1;
    let mut launch = Launch {
        nonce: jobs.records[&local.id].intent.nonce.clone(),
        supervisor: dead.clone(),
        child: own,
        pgid: std::process::id(),
        started_at: 1,
    };
    store::write(&jobs.dir(local.id).join("launch.json"), &launch).unwrap();
    jobs.records.get_mut(&local.id).unwrap().info.finished_at = Some(crate::util::now_secs() - 2 * 86400);
    let remote = jobs.add("old-remote".into(), "test".into(), spec(&dir), "peer".into(), None).unwrap();
    let record = jobs.records.get_mut(&remote.id).unwrap();
    record.executor = Some(Executor { host: Some("peer".into()), cwd: dir.path().into(), repository: None });
    record.info.status = "failed".into();
    record.info.reason = "supervisor lost; outcome unknown".into();
    record.info.finished_at = Some(crate::util::now_secs() - 2 * 86400);
    jobs.refresh().unwrap();
    assert_eq!(jobs.snapshot().len(), 2, "age cannot hide unknown work still in custody");
    assert!(jobs.dismiss(local.id).is_err());
    assert!(jobs.dismiss(remote.id).is_err());
    launch.child = dead;
    store::write(&jobs.dir(local.id).join("launch.json"), &launch).unwrap();
    store::write(&jobs.dir(remote.id).join("stop-acked.json"), &jobs.records[&remote.id].intent.nonce).unwrap();
    jobs.refresh().unwrap();
    assert!(jobs.snapshot().is_empty(), "verified termination permits expiry without fabricating exit status");
}
#[test]
fn polling_one_ten_and_a_hundred_jobs_keeps_unchanged_metadata_read_only() {
    for count in [1, 10, 100] {
        let dir = TempDir::new("job-readonly-poll");
        let mut jobs = Jobs::load(dir.path()).unwrap();
        let mut ids = Vec::new();
        for index in 0..count {
            let info = jobs.add(format!("job-{index}"), "held".into(), spec(&dir), "local".into(), None).unwrap();
            let record = jobs.records.get_mut(&info.id).unwrap();
            match index % 3 {
                0 => record.info.reason = "held: memory".into(),
                1 => {
                    record.executor =
                        Some(Executor { host: Some("peer".into()), cwd: dir.path().into(), repository: None });
                    record.info.host = Some("peer".into());
                    record.info.status = "working".into();
                    record.info.reason.clear();
                }
                _ => {
                    record.info.status = "failed".into();
                    record.info.exit_code = Some(23);
                    record.info.finished_at = Some(crate::util::now_secs());
                    record.info.reason.clear();
                }
            }
            let record = jobs.records[&info.id].clone();
            if let Some(executor) = record.executor {
                store::write(&jobs.dir(info.id).join("executor.json"), &executor).unwrap();
            }
            if record.info.exit_code.is_some() {
                store::write(
                    &jobs.dir(info.id).join("exit.json"),
                    &Exit {
                        nonce: record.intent.nonce,
                        code: Some(23),
                        signal: None,
                        finished_at: record.info.finished_at.unwrap(),
                        stopped: false,
                        error: None,
                    },
                )
                .unwrap();
            }
            jobs.save_public(info.id).unwrap();
            ids.push(info.id);
        }
        let inode: Vec<_> =
            ids.iter().map(|id| fs::metadata(jobs.dir(*id).join("public.json")).unwrap().ino()).collect();
        for _ in 0..3 {
            jobs.refresh().unwrap();
            for id in &ids {
                jobs.save_public(*id).unwrap();
            }
        }
        for (id, before) in ids.iter().zip(&inode) {
            assert_eq!(
                fs::metadata(jobs.dir(*id).join("public.json")).unwrap().ino(),
                *before,
                "polling{count}jobs must not rewrite unchanged held/status metadata"
            );
        }
        let id = ids[0];
        jobs.records.get_mut(&id).unwrap().info.reason = "held: quota".into();
        jobs.save_public(id).unwrap();
        assert_ne!(fs::metadata(jobs.dir(id).join("public.json")).unwrap().ino(), inode[0]);
        let persisted = Jobs::load(dir.path()).unwrap();
        assert_eq!(persisted.records[&id].info.reason, "held: quota");
    }
}

#[test]
fn remote_acceptance_survives_reload_and_rejects_foreign_nonce() {
    let dir = TempDir::new("job-acceptance");
    let mut jobs = Jobs::load(dir.path()).unwrap();
    let job = jobs.add("acceptance".into(), "test".into(), spec(&dir), "peer".into(), None).unwrap();
    assert!(!jobs.remote_accepted(job.id).unwrap(), "lost acknowledgment must retain submission retries");
    jobs.mark_remote_accepted(job.id).unwrap();
    let path = jobs.dir(job.id).join("accepted.json");
    let inode = fs::metadata(&path).unwrap().ino();
    jobs.mark_remote_accepted(job.id).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode, "acceptance is fsynced only once");
    let recovered = Jobs::load(dir.path()).unwrap();
    assert!(recovered.remote_accepted(job.id).unwrap());
    store::write(&path, &"foreign-nonce").unwrap();
    assert!(recovered.remote_accepted(job.id).is_err());
    assert!(recovered.mark_remote_accepted(job.id).is_err());
}
