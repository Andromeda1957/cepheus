//! Command-source custody is an independent runtime path from public queue jobs.
//! All source commands, observations and birth identities belong to this fixture.
use super::*;
use std::os::unix::fs::MetadataExt;

fn guard(seconds: u64) -> Duration {
    let configured = std::env::var("CEPHEUS_TEST_TIMEOUT_SCALE").unwrap_or_default();
    let scale = if configured.trim().is_empty() || configured.eq_ignore_ascii_case("auto") {
        let mut load = 0.0;
        let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as f64;
        if unsafe { libc::getloadavg(&mut load, 1) } == 1 {
            (4.0 * load / cpus).clamp(1.0, 10.0)
        } else {
            1.0
        }
    } else {
        let value = configured.parse::<f64>().expect("test timeout scale must be numeric");
        assert!(value.is_finite() && value >= 1.0);
        value
    };
    Duration::from_secs_f64(seconds as f64 * scale)
}

/// This observes exact recorded identities; it never sends a signal. A PID
/// reused by another process means the recorded old identity is already gone.
fn identity_quiet(value: &Value) -> bool {
    let (Some(pid), Some(start), Some(uid), Some(boot)) =
        (value["pid"].as_u64(), value["start"].as_u64(), value["uid"].as_u64(), value["boot"].as_str())
    else {
        return false;
    };
    let path = std::path::PathBuf::from(format!("/proc/{pid}"));
    let stat = match fs::read_to_string(path.join("stat")) {
        Ok(value) => value,
        Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
    };
    let Some((_, tail)) = stat.rsplit_once(')') else { return false };
    let fields: Vec<_> = tail.split_whitespace().collect();
    let Some(actual_start) = fields.get(19).and_then(|s| s.parse::<u64>().ok()) else { return false };
    if start != actual_start || fields.first() == Some(&"Z") {
        return true;
    }
    let Ok(metadata) = fs::metadata(&path) else { return false };
    if uid != metadata.uid() as u64 {
        return true;
    }
    fs::read_to_string("/proc/sys/kernel/random/boot_id").is_ok_and(|actual| actual.trim() != boot)
}

fn runtime_quiet(service: &Service) -> bool {
    let queue = service.root.join("state/work-queue");
    let Ok(entries) = fs::read_dir(&queue) else { return false };
    for entry in entries {
        let Ok(entry) = entry else { return false };
        if entry.file_name().to_string_lossy().starts_with("source-") && entry.path().is_file() {
            let Some(record) = read_json(&entry.path()) else { return false };
            if !record["refresh_job"].is_null() {
                return false;
            }
        }
    }
    let jobs = queue.join("source-runtime/jobs");
    if jobs.exists() {
        let Ok(entries) = fs::read_dir(jobs) else { return false };
        for entry in entries {
            let Ok(entry) = entry else { return false };
            for name in ["owner.json", "launch.json"] {
                let path = entry.path().join(name);
                if !path.exists() {
                    continue;
                }
                let Some(record) = read_json(&path) else { return false };
                if name == "owner.json" {
                    if !identity_quiet(&record) {
                        return false;
                    }
                } else if !identity_quiet(&record["supervisor"]) || !identity_quiet(&record["child"]) {
                    return false;
                }
            }
        }
    }
    let started = service.root.parent().unwrap().join("source-started");
    if started.exists() {
        let Ok(entries) = fs::read_dir(started) else { return false };
        for entry in entries {
            let Ok(entry) = entry else { return false };
            if !read_json(&entry.path()).is_some_and(|record| identity_quiet(&record)) {
                return false;
            }
        }
    }
    true
}

/// Called before canonical P2 teardown: disable source launches, then preserve
/// private runtime receipts until supervisors, timeout leaders and fake-source
/// descendants have all exited. Direct owner controls are intentional; only
/// the corresponding Lua API is restricted to service execution context.
pub(super) fn quiesce(service: &Service) -> bool {
    if !service.root.join("state/work-queue/source-runtime").exists() {
        return true;
    }
    let _ = result(service, "source-reconcile", &json!({"owner":"owner","ids":[]}));
    let end = Instant::now() + guard(30);
    loop {
        if runtime_quiet(service) {
            return true;
        }
        if Instant::now() >= end {
            let _=fs::write(service.root.parent().unwrap().join("cleanup-incomplete"),"Private source-runtime identities or pending refresh receipts remain active; preserve for active-task cleanup.\n");
            eprintln!("source-runtime cleanup incomplete: {}", service.root.display());
            return false;
        }
        let _ = result(service, "source-list", &json!({}));
        thread::sleep(Duration::from_millis(50));
    }
}

fn plan(root: &Path, title: &str, gated: bool) {
    let path = root.join("source-plan.next");
    fs::write(&path, json!({"title":title,"gated":gated}).to_string()).unwrap();
    fs::rename(path, root.join("source-plan.json")).unwrap();
}
fn count(root: &Path) -> usize {
    fs::read_dir(root.join("source-started")).unwrap().count()
}
fn started_source(root: &Path, index: usize) -> Option<Value> {
    read_json(&root.join("source-started").join(index.to_string()))
}
fn view(service: &Service) -> Value {
    call(service, "source-list", &json!({"source":"runtime-source"}))
}
fn source(service: &Service) -> Value {
    view(service)["sources"][0].clone()
}
fn title(service: &Service) -> Option<String> {
    view(service)["tasks"].as_array()?.first()?["title"].as_str().map(str::to_owned)
}
fn fresh(service: &Service, expected: &str) -> bool {
    source(service)["fresh"] == true && title(service).as_deref() == Some(expected)
}
fn private_runtime(service: &Service) -> std::path::PathBuf {
    let root = service.root.join("state/work-queue/source-runtime/jobs");
    let mut found = None;
    wait("private source launch and independent timeout envelope", || {
        for entry in fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            if let (Some(execution), Some(launch)) =
                (read_json(&path.join("execution.json")), read_json(&path.join("launch.json")))
            {
                if !identity_quiet(&launch["supervisor"]) && !identity_quiet(&launch["child"]) {
                    assert_eq!(
                        &execution["argv"].as_array().unwrap()[..5],
                        &json!(["/usr/bin/timeout", "--signal=TERM", "--kill-after=2", "--", "60"]).as_array().unwrap()
                            [..]
                    );
                    let intent = read_json(&path.join("intent.json")).unwrap();
                    assert_eq!(launch["nonce"], intent["nonce"]);
                    found = Some(path);
                    return true;
                }
            }
        }
        false
    });
    found.unwrap()
}

#[test]
fn command_sources_keep_one_refresh_through_handover_crash_and_removal() {
    let scratch = Scratch::new("queue-source-runtime");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    for dir in ["source-started", "source-finished"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let source_script = root.join("source.py");
    fs::write(&source_script,r#"import fcntl,json,os,pathlib,sys,time
root=pathlib.Path(sys.argv[1])
with (root/'source-counter').open('a+') as counter:
    fcntl.flock(counter,fcntl.LOCK_EX)
    counter.seek(0)
    raw=counter.read().strip()
    index=int(raw or '0')+1
    counter.seek(0);counter.truncate();counter.write(str(index));counter.flush();os.fsync(counter.fileno())
plan=json.loads((root/'source-plan.json').read_text())
identity={'pid':os.getpid(),'start':int(pathlib.Path('/proc/self/stat').read_text().rsplit(')',1)[1].split()[19]),'uid':os.getuid(),'boot':pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip()}
path=root/'source-started'/str(index)
temporary=path.with_suffix('.next')
temporary.write_text(json.dumps(identity));temporary.replace(path)
while plan['gated'] and not (root/('source-release-'+str(index))).exists():time.sleep(0.02)
print(json.dumps({'tasks':[{'id':'runtime-task','title':plan['title'],'state':'ready'}]}),flush=True)
(root/'source-finished'/str(index)).write_text('completed')
"#).unwrap();
    plan(root, "initial-ready", false);
    let script = actor_script(root);
    let mut service = QueueService::new(Service::new(root.join("host"), &binary, 12288));
    service.start();
    ready(&service);
    // Force each finite refresh explicitly instead of waiting for a schedule.
    // The long private interval prevents an unrelated periodic sixth launch.
    let registration = json!({"id":"runtime-source","owner":"owner","repository":"job-fixture","mode":"command","interval_seconds":86400,"enabled":true,"cwd":service.root.join("work"),"argv":["/usr/bin/python3",source_script,root]});
    call(&service, "source-register", &registration);
    wait("actual command-source fresh ready metadata", || fresh(&service, "initial-ready"));
    assert_eq!(count(root), 1);
    assert!(service.jobs().is_empty(), "private refresh jobs do not appear as public owner work");
    plan(root, "handover-ready", true);
    call(&service, "source-refresh", &json!({"source":"runtime-source"}));
    wait("gated second command-source body", || started_source(root, 2).is_some());
    let second = started_source(root, 2).unwrap();
    let private = private_runtime(&service);
    assert!(!identity_quiet(&second));
    assert_eq!(source(&service)["fresh"], false);
    let pid = service.pid();
    let birth = service_birth(&service);
    service.replace_binary(&binary);
    wait("actual exec handover with active source command", || handover_ready(&service, &birth));
    assert_eq!(service.pid(), pid);
    assert_eq!(count(root), 2, "handover must not relaunch the active refresh");
    assert!(!identity_quiet(&second));
    fs::write(root.join("source-release-2"), "release").unwrap();
    wait("one adopted refresh result published", || {
        fresh(&service, "handover-ready") && root.join("source-finished/2").is_file()
    });
    assert_eq!(count(root), 2);
    assert!(private.starts_with(service.root.join("state/work-queue/source-runtime/jobs")));

    plan(root, "cold-ready", true);
    call(&service, "source-refresh", &json!({"source":"runtime-source"}));
    wait("gated source before daemon death", || started_source(root, 3).is_some());
    let third = started_source(root, 3).unwrap();
    let receipt = private_runtime(&service);
    service.crash();
    assert!(!identity_quiet(&third));
    fs::write(root.join("source-release-3"), "release").unwrap();
    wait("independent source completion receipt without daemon", || {
        root.join("source-finished/3").is_file() && receipt.join("exit.json").is_file()
    });
    service.start();
    wait("cold daemon adopts exact source result", || fresh(&service, "cold-ready"));
    assert_eq!(count(root), 3, "completed source receipt must not be executed twice");

    plan(root, "obsolete-ready", true);
    call(&service, "source-refresh", &json!({"source":"runtime-source"}));
    wait("gated source before removal", || started_source(root, 4).is_some());
    let fourth = started_source(root, 4).unwrap();
    let removed = call(&service, "source-reconcile", &json!({"owner":"owner","ids":[]}));
    assert_eq!(removed["sources"][0]["enabled"], false);
    assert_eq!(removed["sources"][0]["fresh"], false);
    assert_eq!(removed["tasks"][0]["title"], "cold-ready", "removal retains cached rows as stale");
    fs::write(root.join("source-release-4"), "release if stop raced completion").unwrap();
    wait("removed refresh workload and receipt quiesce", || runtime_quiet(&service) && identity_quiet(&fourth));
    observe("removed source cannot publish or launch again", Duration::from_secs(2), || {
        assert_eq!(count(root), 4);
        assert_eq!(source(&service)["fresh"], false);
        assert_eq!(title(&service).as_deref(), Some("cold-ready"));
    });
    let mut dependency = spec(&script, root, "runtime-dependent", 0, "plain", &service, "local");
    dependency["dependencies"] = json!([{"source":"runtime-source","task":"runtime-task","condition":"ready"}]);
    let dependent = add(&service, &dependency);
    call(&service, "queue-enable", &json!({"enabled":true}));
    wait("removed source blocks its ready-dependent item", || item(&service, dependent)["state"] == "held");
    assert!(started(root, "runtime-dependent").is_none());
    plan(root, "reenabled-ready", true);
    let enabled = call(&service, "source-register", &registration);
    assert_eq!(enabled["source"]["fresh"], false, "registration cannot revive cached ready evidence");
    wait("new refresh after explicit re-registration", || started_source(root, 5).is_some());
    observe("new registration still waits for fresh metadata", Duration::from_secs(2), || {
        assert!(started(root, "runtime-dependent").is_none());
        assert_eq!(source(&service)["fresh"], false);
        assert_eq!(count(root), 5);
    });
    fs::write(root.join("source-release-5"), "release").unwrap();
    wait("fresh source result alone releases dependency", || {
        fresh(&service, "reenabled-ready") && started(root, "runtime-dependent").is_some()
    });
    release(root, "runtime-dependent");
    settled(&service, dependent, "done");
    assert_eq!(count(root), 5);
    assert_eq!(fs::read_dir(root.join("duplicates")).unwrap().count(), 0);
    assert!(quiesce(&service));
    public(&service.state());
    private_modes(&service.root.join("state/work-queue"));
    eprintln!("queue-command-source-runtime: invocations=5 exec_adoptions=1 cold_receipt_adoptions=1 removed_refreshes=1 cached_rows_stale_until_new_result=true own_birth_identities_quiet=true");
}
