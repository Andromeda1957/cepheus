//! Phase 2 acceptance needs real concurrent processes and receipts: mocked job
//! counts cannot reject lost executions, duplicate recovery or cross-host IDs.
//! Two authenticated service hosts share one physical test machine; this does
//! not measure a physical network. No provider CLI or installed Deck is used.
//! Private services use injectable 16GiB probes, zero load/PSI and no provider
//! start gap. The hub's available memory changes from 1GiB to12GiB while the
//! peer has14GiB. These controlled inputs exercise admission, not host capacity.
//! Run with an optional exact CEPHEUS_JOB_TEST_BIN override, then:
//! cargo test --test job_sessions -- --show-output --test-threads=1
#[path = "job_sessions/support.rs"]
mod support;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use support::*;

fn provenance(backend: &Path) {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(include_bytes!("job_sessions.rs"));
    context.update(include_bytes!("job_sessions/support.rs"));
    let fixture = context.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!(
        "job-acceptance-provenance: {}",
        json!({"backend_sha256":hash(backend),"fixture_sha256":fixture,"backend_source":std::env::var("CEPHEUS_JOB_TEST_SOURCE").unwrap_or_else(|_|"cargo-built-or-explicit-override; retain build receipt".into()),"physical_hosts":1,"logical_tls_hosts":2})
    );
}

fn receipt(root: &Path, key: &str) -> Option<Value> {
    read_json(&root.join("started").join(key))
}
fn started_id(reply: &Value) -> u64 {
    reply["job"]["id"].as_u64().expect("accepted job has an opaque ID")
}
fn expect_exit(info: &Value, code: i32) {
    assert_eq!(info["exit_code"], code, "{info}");
    assert_eq!(info["status"], if code == 0 { "finished" } else { "failed" }, "{info}");
    assert!(info["signal"].is_null(), "normal exit must not invent a signal: {info}");
}
fn known_private(input: &Value) {
    no_private_metadata(input);
    if let Some(jobs) = input.get("jobs").and_then(Value::as_array) {
        for job in jobs {
            for field in ["argv", "env", "stdin", "key"] {
                assert!(job.get(field).is_none(), "private launch fields must not be public");
            }
        }
    }
}

#[test]
fn a_hundred_real_jobs_are_concurrent_across_authenticated_hosts() {
    let scratch = Scratch::new("fleet100");
    let root = &scratch.0;
    let backend = binary();
    provenance(&backend);
    let script = actor(root);
    let mut fleet = Fleet {
        hub: Service::new(root.join("hub"), &backend, 1024),
        peer: Service::new(root.join("peer"), &backend, 14336),
    };
    let (hub, peer) = (&mut fleet.hub, &mut fleet.peer);
    connect(&hub, &peer);
    peer.start();
    hub.start();
    wait("authenticated remote job-capable state", || hub.state()["host_schedulers"]["worker"].is_object());
    assert_eq!(hub.state()["job_capable"], true);
    let began = Instant::now();
    let mut jobs = Vec::new();
    let mut requests = Vec::new();
    for index in 0..100 {
        let key = index.to_string();
        let placement = if index % 2 == 0 { "local" } else { "auto" };
        let code = [0, 7, 23][index % 3];
        let form = if index == 1 { "json" } else { "plain" };
        let request = request(&script, root, &key, code, form, &hub.root.join("work"), placement);
        let reply = hub.job("start", &request);
        known_private(&reply);
        jobs.push(started_id(&reply));
        requests.push(request);
    }
    assert_eq!(jobs.iter().collect::<HashSet<_>>().len(), 100, "origin opaque IDs must be unique");
    wait("all 50 peer actors while local memory is held", || {
        (0..100).filter(|i| i % 2 == 1).all(|i| receipt(root, &i.to_string()).is_some())
    });
    for index in (0..100).filter(|i| i % 2 == 0) {
        assert!(receipt(root, &index.to_string()).is_none(), "low-memory local jobs must not spawn");
        let held = hub.info(jobs[index]);
        assert_eq!(held["status"], "starting");
        assert!(
            held["reason"].as_str().is_some_and(|r| r.contains("headroom") || r.contains("memory")),
            "observable admission hold: {held}"
        );
    }
    assert_eq!(started_id(&hub.job("start", &requests[0])), jobs[0], "same queued key is one intent");
    hub.memory(12288);
    wait("100 actual actors simultaneously gated", || {
        (0..100).all(|i| receipt(root, &i.to_string()).is_some())
            && hub.jobs().len() == 100
            && hub.jobs().iter().all(|j| j["status"] == "working")
    });
    let merged = hub.state();
    known_private(&merged);
    let listed = merged["jobs"].as_array().expect("jobs in the single merged fleet state");
    assert_eq!(listed.len(), 100);
    assert_eq!(listed.iter().filter(|j| j["host"] == "worker").count(), 50);
    let ids: HashSet<_> = listed.iter().map(|j| j["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, jobs.iter().copied().collect(), "one federated origin view, not independently summed results");
    for index in 0..100 {
        let rec = receipt(root, &index.to_string()).unwrap();
        let expected = if index % 2 == 0 { hub.root.join("work") } else { peer.root.join("work") };
        assert_eq!(rec["cwd"].as_str(), expected.to_str());
        assert!(alive(rec["pid"].as_u64().unwrap() as u32), "all jobs really run behind the gate");
        assert_eq!(rec["argv"], json!(["literal $(touch must-not-be-executed)", "argv-private-marker"]));
        assert_eq!(rec["env"], "env-private-marker");
        let expected_stdin = if index == 1 {
            "<cross-session-message source=\"cepheus\" encoding=\"html\">\nstdin-private-marker\n</cross-session-message>"
        } else {
            "stdin-private-marker"
        };
        assert_eq!(rec["stdin"], expected_stdin);
        assert!(rec["inherited"].is_null(), "env_clear must remove an inherited private value");
    }
    for service in [&hub, &peer] {
        assert!(!service.root.join("work/must-not-be-executed").exists());
    }
    eprintln!(
        "jobs100-real-federated: startup_ms={} merged_state_bytes={}",
        began.elapsed().as_millis(),
        serde_json::to_vec(&merged).unwrap().len()
    );
    fs::write(root.join("release"), "release").unwrap();
    wait("all 100 exact completion receipts", || hub.jobs().len() == 100 && hub.jobs().iter().all(terminal));
    for (index, id) in jobs.iter().enumerate() {
        expect_exit(&hub.info(*id), [0, 7, 23][index % 3]);
    }
    let mut offset = 0;
    let mut output = String::new();
    loop {
        let page = hub.job("log", &json!({"job":jobs[0],"offset":offset,"limit":8}));
        assert_eq!(page["job"], jobs[0]);
        let text = page["text"].as_str().unwrap();
        assert!(text.len() <= 8);
        output.push_str(text);
        offset += text.len();
        assert_eq!(page["offset"], offset);
        if page["eof"] == true {
            break;
        }
        assert!(!text.is_empty() && offset < 128, "bounded forward progress through raw output");
    }
    assert_eq!(output, "begin 0\nend 0\n");
    let chat = hub.job("log", &json!({"job":jobs[1],"offset":0,"limit":32}));
    assert!(chat["text"].as_str().unwrap().len() <= 32);
    let items = chat["items"].as_array().unwrap();
    assert!(!items.is_empty() && items.len() <= 600);
    assert!(
        items.iter().any(|item| item["text"] == "fleet result 1"),
        "scripted Codex stream must normalize into shared chat items: {chat}"
    );
    known_private(&hub.job("status", &json!({})));
    private_modes(&hub.root.join("state/jobs"));
    private_modes(&peer.root.join("state/jobs"));
    assert_eq!(fs::read_dir(root.join("duplicates")).unwrap().count(), 0);
    for key in 0..100 {
        assert_eq!(
            fs::read_to_string(root.join("finished").join(key.to_string())).unwrap(),
            [0, 7, 23][key % 3].to_string()
        );
    }
    fs::remove_file(root.join("release")).unwrap();
    let long = request(&script, root, "remote-shutdown", 0, "tree", &hub.root.join("work"), "worker");
    let long_id = started_id(&hub.job("start", &long));
    wait("remote long job before origin shutdown", || {
        receipt(root, "remote-shutdown").and_then(|r| r["child"].as_u64()).is_some()
    });
    let owned = receipt(root, "remote-shutdown").unwrap();
    assert_eq!(owned["cwd"].as_str(), peer.root.join("work").to_str());
    let lost_remote = request(&script, root, "remote-unknown", 0, "tree", &hub.root.join("work"), "worker");
    let lost_remote_id = started_id(&hub.job("start", &lost_remote));
    wait("remote actor before supervisor loss", || {
        receipt(root, "remote-unknown").and_then(|r| r["child"].as_u64()).is_some()
    });
    let unknown_owned = receipt(root, "remote-unknown").unwrap();
    kill_supervisor(&peer, lost_remote_id, &unknown_owned);
    wait("origin observes remote unknown outcome", || {
        hub.info(lost_remote_id)["reason"].as_str().is_some_and(|r| r.contains("supervisor lost"))
    });
    assert!(alive(unknown_owned["pid"].as_u64().unwrap() as u32), "unknown outcome still owns a running body");
    hub.stop();
    wait("origin stop forwards durable remote stop", || {
        terminal(&peer.info(long_id))
            && !alive(owned["pid"].as_u64().unwrap() as u32)
            && !alive(owned["child"].as_u64().unwrap() as u32)
    });
    assert!(peer.info(long_id)["signal"].as_i64().is_some(), "remote supervisor retains exact signal status");
    wait("origin shutdown also stops failed-unknown remote workload", || {
        let _ = peer.info(lost_remote_id);
        !alive(unknown_owned["pid"].as_u64().unwrap() as u32) && !alive(unknown_owned["child"].as_u64().unwrap() as u32)
    });
    assert!(peer.info(lost_remote_id)["exit_code"].is_null(), "unknown custody must not invent a success code");
}

#[test]
fn jobs_recover_receipts_without_duplicate_work_and_stop_the_owned_group() {
    let scratch = Scratch::new("recovery");
    let root = &scratch.0;
    let backend = binary();
    provenance(&backend);
    let script = actor(root);
    let mut service = Service::new(root.join("host"), &backend, 12288);
    service.start();
    let intent = request(&script, root, "recovery", 17, "plain", &service.root.join("work"), "local");
    let id = started_id(&service.job("start", &intent));
    wait("the recovery actor", || receipt(root, "recovery").is_some());
    let initial = receipt(root, "recovery").unwrap();
    let actor_pid = initial["pid"].as_u64().unwrap() as u32;
    let service_pid = service.pid();
    service.control(&["restart"]);
    assert_eq!(service.pid(), service_pid);
    assert!(alive(actor_pid));
    assert_eq!(receipt(root, "recovery"), Some(initial.clone()));
    assert_eq!(started_id(&service.job("start", &intent)), id);
    service.replace_binary(&backend);
    wait("actual exec handover", || !exe(service_pid).ends_with(" (deleted)"));
    assert!(alive(actor_pid));
    assert_eq!(receipt(root, "recovery"), Some(initial));
    assert_eq!(started_id(&service.job("start", &intent)), id);
    service.crash();
    assert!(alive(actor_pid), "daemon SIGKILL must not kill its independently supervised job");
    fs::write(root.join("release"), "release while daemon is absent").unwrap();
    wait("completion with no daemon", || root.join("finished/recovery").is_file() && !alive(actor_pid));
    let private = service.root.join("state/jobs").join(format!("{id:010x}"));
    wait("durable supervisor exit receipt", || private.join("exit.json").is_file());
    service.start();
    wait("recovered exact exit", || terminal(&service.info(id)));
    expect_exit(&service.info(id), 17);
    assert_eq!(
        started_id(&service.job("start", &intent)),
        id,
        "retry after lost daemon acknowledgement cannot launch again"
    );
    assert_eq!(fs::read_dir(root.join("duplicates")).unwrap().count(), 0);
    service.job("dismiss", &json!({"job":id}));
    assert!(!service.jobs().iter().any(|j| j["id"] == id));
    assert_eq!(service.job("status", &json!({"key":"fleet-recovery"}))["jobs"][0]["id"], id);
    assert_eq!(started_id(&service.job("start", &intent)), id, "dismissal does not erase the idempotency receipt");
    fs::remove_file(root.join("release")).unwrap();
    let tree = request(&script, root, "tree", 0, "tree", &service.root.join("work"), "local");
    let stopped = started_id(&service.job("start", &tree));
    wait("owned stop process group", || receipt(root, "tree").and_then(|v| v["child"].as_u64()).is_some());
    let tree = receipt(root, "tree").unwrap();
    let parent = tree["pid"].as_u64().unwrap() as u32;
    let child = tree["child"].as_u64().unwrap() as u32;
    service.job("stop", &json!({"job":stopped}));
    wait("stop receipt and both owned processes", || {
        terminal(&service.info(stopped)) && !alive(parent) && !alive(child)
    });
    assert!(
        service.info(stopped)["signal"].as_i64().is_some_and(|s| s == libc::SIGTERM as i64 || s == libc::SIGINT as i64),
        "actual stop wait status"
    );
    let lost = request(&script, root, "supervisor-lost", 0, "tree", &service.root.join("work"), "local");
    let lost_id = started_id(&service.job("start", &lost));
    wait("supervisor-loss actor and child", || {
        receipt(root, "supervisor-lost").and_then(|v| v["child"].as_u64()).is_some()
    });
    let owned = receipt(root, "supervisor-lost").unwrap();
    kill_supervisor(&service, lost_id, &owned);
    wait("explicit unknown outcome after actual supervisor SIGKILL", || {
        service.info(lost_id)["reason"].as_str().is_some_and(|r| r.contains("supervisor lost") && r.contains("unknown"))
    });
    let unknown = service.info(lost_id);
    assert_eq!(unknown["status"], "failed");
    assert!(unknown["exit_code"].is_null() && unknown["signal"].is_null(), "lost supervisor cannot invent wait status");
    let no_retry_before = Instant::now() + Duration::from_secs(3);
    while Instant::now() < no_retry_before {
        assert_eq!(started_id(&service.job("start", &lost)), lost_id);
        assert_eq!(receipt(root, "supervisor-lost"), Some(owned.clone()));
        assert!(alive(owned["pid"].as_u64().unwrap() as u32));
        thread::sleep(Duration::from_millis(100));
    }
    service.job("stop", &json!({"job":lost_id}));
    wait("explicit stop cleans group after supervisor loss", || {
        let _ = service.info(lost_id);
        !alive(owned["pid"].as_u64().unwrap() as u32) && !alive(owned["child"].as_u64().unwrap() as u32)
    });
    assert_eq!(fs::read_dir(root.join("duplicates")).unwrap().count(), 0);
    let mut bad = request(&script, root, "spawn-failure", 0, "plain", &service.root.join("work"), "local");
    bad["argv"] = json!(["/no-such-test-executable", "argv-private-marker"]);
    let failed = started_id(&service.job("start", &bad));
    wait("failed spawn category", || terminal(&service.info(failed)));
    assert_eq!(service.info(failed)["status"], "failed");
    known_private(&service.state());
    known_private(&service.job("status", &json!({})));
    private_modes(&service.root.join("state/jobs"));
}
