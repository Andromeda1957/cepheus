//! Phase3 exercises actual queue dispatch and receipt reconciliation. Two private
//! authenticated TLS hosts run on one physical machine; network throughput and
//! physical host capacity are not measured. The100job case uses explicit zero
//! provider spacing and injected calm16GiB probes; only available memory changes.
//! A separate case uses explicit2s provider spacing and real quota reset time.
//! No CLI provider/model or installed Deck service is invoked.
//! CEPHEUS_QUEUE_TEST_BIN selects an exact externally built protocol14 binary;
//! retain its source/build receipt alongside the emitted binary+fixture hashes.
//! cargo test --test work_queue -- --show-output --test-threads=1
#[path = "job_sessions/support.rs"]
mod jobs_support;
#[path = "work_queue/pause.rs"]
mod pause;
#[path = "work_queue/support.rs"]
mod queue_support;
#[path = "work_queue/source_runtime.rs"]
mod source_runtime;
use jobs_support::*;
use queue_support::*;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn a_hundred_queue_items_dispatch_once_through_two_authenticated_hosts() {
    let scratch = Scratch::new("queue100");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let script = actor_script(root);
    let mut fleet = QueueFleet(Fleet {
        hub: Service::new(root.join("hub"), &binary, 1024),
        peer: Service::new(root.join("peer"), &binary, 1024),
    });
    let (hub, peer) = (&mut fleet.0.hub, &mut fleet.0.peer);
    connect(hub, peer);
    peer.start();
    hub.start();
    ready(hub);
    ready(peer);
    wait("single authenticated fleet state", || hub.state()["host_schedulers"]["worker"].is_object());
    assert_eq!(call(hub, "queue-list", &json!({}))["enabled"], false);
    let requests: Vec<_> = (0..100)
        .map(|index| {
            spec(
                &script,
                root,
                &index.to_string(),
                [0, 7, 23][index % 3],
                if index == 1 { "json" } else { "plain" },
                hub,
                if index % 2 == 0 { "local" } else { "auto" },
            )
        })
        .collect();
    let mut invalid = requests[0].clone();
    invalid["key"] = json!("invalid-batch-tail");
    invalid["kind"] = json!("invalid");
    assert!(result(hub, "queue-batch", &json!({"items":[requests[0],invalid]})).is_err());
    assert!(items(hub).is_empty(), "invalid tail cannot accept a prefix of a batch");
    let accepted = call(hub, "queue-batch", &json!({"items":requests}));
    public(&accepted);
    let ids: Vec<_> = accepted["items"].as_array().unwrap().iter().map(|v| v["id"].as_u64().unwrap()).collect();
    assert_eq!(ids.len(), 100);
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 100);
    observe("disabled queue has no executors", Duration::from_secs(2), || {
        assert!(hub.jobs().is_empty() && peer.jobs().is_empty());
        assert_eq!(fs::read_dir(root.join("started")).unwrap().count(), 0);
    });
    assert_eq!(add(hub, &requests[0]), ids[0], "same key retains the accepted intent");
    call(hub, "queue-enable", &json!({"enabled":true}));
    wait("all100 held for actual admission", || {
        let values = items(hub);
        values.len() == 100 && values.iter().all(|v| v["state"] == "held")
    });
    assert_eq!(fs::read_dir(root.join("started")).unwrap().count(), 0, "low memory may not over-admit queued work");
    peer.memory(14336);
    wait("all50 remote actors while local memory stays held", || {
        (0..100).filter(|i| i % 2 == 1).all(|i| started(root, &i.to_string()).is_some())
    });
    for index in (0..100).filter(|i| i % 2 == 0) {
        assert!(started(root, &index.to_string()).is_none());
        assert!(matches!(item(hub, ids[index])["state"].as_str(), Some("held" | "queued")));
    }
    let mut changed = requests[0].clone();
    changed["argv"] = json!(["/bin/false"]);
    assert!(result(hub, "queue-add", &changed).is_err(), "accepted key cannot be reassigned to a different launch");
    let began = Instant::now();
    hub.memory(12288);
    wait("all100 actual bodies gated concurrently", || {
        (0..100).all(|i| started(root, &i.to_string()).is_some())
            && hub.jobs().len() == 100
            && hub.jobs().iter().all(|v| v["status"] == "working")
    });
    let state = hub.state();
    public(&state);
    let listed = state["jobs"].as_array().unwrap();
    assert_eq!(listed.len(), 100, "one merged view, not the sum of independent services");
    assert_eq!(listed.iter().filter(|v| v["host"] == "worker").count(), 50);
    let jobs: Vec<_> = ids.iter().map(|id| job_id(&item(hub, *id))).collect();
    assert_eq!(jobs.iter().collect::<HashSet<_>>().len(), 100);
    assert_eq!(
        listed.iter().map(|v| v["id"].as_u64().unwrap()).collect::<HashSet<_>>(),
        jobs.iter().copied().collect()
    );
    for index in 0..100 {
        let actual = started(root, &index.to_string()).unwrap();
        assert!(alive(actual["pid"].as_u64().unwrap() as u32));
        let cwd = if index % 2 == 0 { hub.root.join("work") } else { peer.root.join("work") };
        assert_eq!(actual["cwd"].as_str(), cwd.to_str());
        assert_eq!(actual["argv"], json!(["literal $(touch must-not-be-executed)", "argv-private-marker"]));
        assert_eq!(actual["env"], "env-private-marker");
        assert!(actual["inherited"].is_null());
        let stdin = if index == 1 {
            "<cross-session-message source=\"cepheus\" encoding=\"html\">\nstdin-private-marker\n</cross-session-message>"
        } else {
            "stdin-private-marker"
        };
        assert_eq!(actual["stdin"], stdin);
    }
    eprintln!("queue100-real-federated: released_local_startup_ms={} merged_state_bytes={} concurrent_bodies=100 logical_tls_hosts=2 physical_hosts=1",began.elapsed().as_millis(),serde_json::to_vec(&state).unwrap().len());
    fs::write(root.join("release"), "release").unwrap();
    wait("100 exact queue outcomes", || {
        items(hub).iter().all(|v| matches!(v["state"].as_str(), Some("done" | "failed")))
    });
    for index in 0..100 {
        let expected = [0, 7, 23][index % 3];
        let outcome = item(hub, ids[index]);
        assert_eq!(outcome["state"], if expected == 0 { "done" } else { "failed" });
        let info = hub.info(jobs[index]);
        assert_eq!(info["exit_code"], expected);
        assert_eq!(info["status"], if expected == 0 { "finished" } else { "failed" });
        assert_eq!(fs::read_to_string(root.join("finished").join(index.to_string())).unwrap(), expected.to_string());
        assert_eq!(add(hub, &requests[index]), ids[index]);
    }
    let log = hub.job("log", &json!({"job":jobs[0],"offset":0,"limit":8}));
    assert!(log["text"].as_str().unwrap().len() <= 8);
    let chat = hub.job("log", &json!({"job":jobs[1],"offset":0,"limit":32}));
    assert!(chat["text"].as_str().unwrap().len() <= 32);
    let chat_items = chat["items"].as_array().unwrap();
    assert!(chat_items.len() <= 600);
    assert!(chat_items.iter().any(|v| v["text"] == "fleet result 1"));
    assert_eq!(fs::read_dir(root.join("duplicates")).unwrap().count(), 0);
    for service in [&hub, &peer] {
        assert!(!service.root.join("work/must-not-be-executed").exists());
        private_modes(&service.root.join("state/jobs"));
    }
    private_modes(&hub.root.join("state/work-queue"));
    public(&call(hub, "queue-list", &json!({})));
}

#[test]
fn provider_quota_and_start_spacing_hold_queue_work_until_eligible() {
    let scratch = Scratch::new("queue-provider");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let script = actor_script(root);
    let mut service = QueueService::new(Service::new(root.join("host"), &binary, 12288));
    // Explicit policy: two seconds gives a meaningful wall-clock observation
    // despite whole-second durable launch timestamps. Pure clock tests own the
    // exact boundary; these real processes must not burst on quota release.
    service.configure(|c| {
        c["scheduler"]["stagger_seconds"] = json!(2);
        c["scheduler"]["quota_percent"] = json!(90);
    });
    let usage_path = service.root.join("state/claude-usage.json");
    fs::write(
        &usage_path,
        json!({"updated_at":now(),"rate_limits":{"five_hour":{"used_percentage":95,"resets_at":null}}}).to_string(),
    )
    .unwrap();
    service.start();
    ready(&service);
    wait("private Claude quota observation", || {
        service.state()["usage"]["claude"]
            .as_array()
            .is_some_and(|xs| xs.iter().any(|w| w["used_percent"].as_f64() == Some(95.0)))
    });
    let ids: Vec<_> = (0..3)
        .map(|index| {
            let mut value = spec(&script, root, &format!("paced-{index}"), 0, "plain", &service, "local");
            value["provider"] = json!("claude");
            add(&service, &value)
        })
        .collect();
    call(&service, "queue-enable", &json!({"enabled":true}));
    wait("explicit unknown-reset quota hold", || {
        items(&service)
            .iter()
            .all(|v| v["state"] == "held" && v["reason"].as_str().is_some_and(|s| s.contains("quota")))
    });
    observe("unknown reset never dispatches", Duration::from_secs(2), || {
        // Durable P2 acceptance is not execution: a legitimate race may leave
        // an immutable starting intent, which still must not run a body
        // before the observed quota reset.
        assert!(service
            .jobs()
            .iter()
            .all(|job| job["status"] == "starting"
                && job["reason"].as_str().is_some_and(|reason| reason.contains("quota"))));
        for queued in items(&service) {
            if queued["dispatch"].is_object() {
                assert_eq!(queued["cancelable"], false, "accepted intent retains custody; stop is a job control");
            }
        }
        assert_eq!(fs::read_dir(root.join("started")).unwrap().count(), 0);
    });
    let reset = now() + 4;
    let next = usage_path.with_extension("next");
    fs::write(
        &next,
        json!({"updated_at":now(),"rate_limits":{"five_hour":{"used_percentage":95,"resets_at":reset}}}).to_string(),
    )
    .unwrap();
    fs::rename(next, &usage_path).unwrap();
    wait("all three quota-resumed bodies", || (0..3).all(|i| started(root, &format!("paced-{i}")).is_some()));
    let mut launches: Vec<_> =
        ids.iter().map(|id| service.info(job_id(&item(&service, *id)))["started_at"].as_u64().unwrap()).collect();
    launches.sort_unstable();
    assert!(launches[0] >= reset, "no launch before the observed reset: {launches:?}, reset={reset}");
    assert!(
        launches.windows(2).all(|pair| pair[1].saturating_sub(pair[0]) >= 2),
        "configured2s spacing must preserve at least2s between durable starts: {launches:?}"
    );
    eprintln!(
        "queue-provider-policy: configured_gap_seconds=2 launch_seconds={launches:?} recorded_quota_reset={reset}"
    );
    fs::write(root.join("release"), "release").unwrap();
    wait("three successful queue receipts", || items(&service).iter().all(|v| v["state"] == "done"));
    assert_eq!(fs::read_dir(root.join("duplicates")).unwrap().count(), 0);
    public(&service.state());
}

#[test]
fn dependency_states_and_private_claims_survive_exec_and_daemon_crash() {
    let scratch = Scratch::new("queue-recovery");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let script = actor_script(root);
    let mut service = QueueService::new(Service::new(root.join("host"), &binary, 12288));
    service.start();
    ready(&service);
    let mut order_specs = Vec::new();
    for (key, priority) in [("order-low", -20), ("order-high", 40), ("order-middle", 10)] {
        let mut value = spec(&script, root, key, 0, "plain", &service, "local");
        value["priority"] = json!(priority);
        order_specs.push(value);
    }
    let ordered: Vec<_> = order_specs.iter().map(|value| add(&service, value)).collect();
    assert!(
        result(&service, "queue-reorder", &json!({"ids":[ordered[0]]})).is_err(),
        "partial reorder cannot silently discard newer items"
    );
    let reordered = call(&service, "queue-reorder", &json!({"ids":ordered}));
    assert_eq!(
        reordered["items"].as_array().unwrap().iter().map(|v| v["id"].as_u64().unwrap()).collect::<Vec<_>>(),
        ordered,
        "explicit complete order replaces prior priority differences"
    );
    for (index, id) in ordered.iter().enumerate() {
        call(&service, "queue-cancel", &json!({"id":id}));
        assert!(!item(&service, *id)["cancelable"].as_bool().unwrap());
        assert!(item(&service, *id)["dispatch"].is_null());
        assert_eq!(add(&service, &order_specs[index]), *id, "cancelled keys cannot revive execution");
    }
    let source = call(
        &service,
        "source-register",
        &json!({"id":"private-task-source","owner":"owner","repository":"job-fixture","mode":"push","interval_seconds":20,"enabled":true}),
    );
    let token = source["publisher_token"].as_str().unwrap().to_string();
    let mut sequence = source["sequence"].as_u64().unwrap();
    let publish = |sequence: &mut u64, state: &str| {
        *sequence += 1;
        call(
            &service,
            "source-publish",
            &json!({"source":"private-task-source","owner":"owner","publisher_token":token,"sequence":sequence,"tasks":[{"id":"task-a","title":"Synthetic task","state":state}]}),
        );
    };
    publish(&mut sequence, "active");
    let a_spec = spec(&script, root, "a", 0, "plain", &service, "local");
    let a = add(&service, &a_spec);
    let f = add(&service, &spec(&script, root, "failed", 7, "plain", &service, "local"));
    let u = add(&service, &spec(&script, root, "unknown", 0, "tree", &service, "local"));
    let mut dependent = spec(&script, root, "and-ready", 0, "plain", &service, "local");
    dependent["dependencies"] = json!([{"queue":a,"condition":"finished"},{"source":"private-task-source","task":"task-a","condition":"ready"}]);
    let b = add(&service, &dependent);
    let mut merged = spec(&script, root, "merged", 0, "plain", &service, "local");
    merged["dependencies"] = json!([{"source":"private-task-source","task":"task-a","condition":"merged"}]);
    let c = add(&service, &merged);
    let mut failed_dep = spec(&script, root, "after-failed", 0, "plain", &service, "local");
    failed_dep["dependencies"] = json!([{"queue":f,"condition":"finished"}]);
    let d = add(&service, &failed_dep);
    let mut unknown_dep = spec(&script, root, "after-unknown", 0, "plain", &service, "local");
    unknown_dep["dependencies"] = json!([{"queue":u,"condition":"finished"}]);
    let e = add(&service, &unknown_dep);
    let chore = json!({"id":"private-daily","owner":"owner","repository":"job-fixture","title":"Private daily claim","target":"job","schedule":{"cron":"0 0 * * *","timezone":"UTC"},"enabled":false,"spec":spec(&script,root,"daily",0,"plain",&service,"local")});
    call(&service, "chore-register", &chore);
    let claim_request = json!({"id":"private-daily","key":"private-day-request","session":"private-claimant"});
    let claim = call(&service, "chore-claim", &claim_request)["claim"].clone();
    assert!(claim["token"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(claim["generation"], 1);
    assert_eq!(claim["stale_at"].as_u64().unwrap() - claim["claimed_at"].as_u64().unwrap(), 8 * 3600);
    public(&service.state());
    assert_eq!(call(&service, "chore-claim", &claim_request)["claim"], claim, "same request is one fresh claim");
    assert!(
        result(
            &service,
            "chore-claim",
            &json!({"id":"private-daily","key":"competing-request","session":"other-claimant"})
        )
        .is_err(),
        "fresh occurrence claim must have one winner"
    );
    call(&service, "queue-enable", &json!({"enabled":true}));
    wait("dependency prerequisite actors", || {
        ["a", "failed", "unknown"].iter().all(|key| started(root, key).is_some())
    });
    for key in ["and-ready", "merged", "after-failed", "after-unknown"] {
        assert!(started(root, key).is_none());
    }
    let owned = started(root, "unknown").unwrap();
    wait("unknown actor child receipt", || started(root, "unknown").and_then(|v| v["child"].as_u64()).is_some());
    kill_supervisor(&service, job_id(&item(&service, u)), &owned);
    wait("unknown result propagates without success", || item(&service, u)["state"] == "failed");
    let process = service.pid();
    service.control(&["restart"]);
    assert_eq!(service.pid(), process);
    assert_eq!(call(&service, "queue-list", &json!({}))["enabled"], true);
    assert_eq!(call(&service, "chore-claim", &claim_request)["claim"], claim);
    let birth = service_birth(&service);
    service.replace_binary(&binary);
    wait("real queue exec handover", || handover_ready(&service, &birth));
    assert_eq!(add(&service, &a_spec), a);
    assert_eq!(call(&service, "chore-claim", &claim_request)["claim"], claim);
    service.crash();
    release(root, "a");
    release(root, "failed");
    wait("independent receipts while daemon absent", || {
        root.join("finished/a").is_file() && root.join("finished/failed").is_file()
    });
    service.start();
    settled(&service, a, "done");
    settled(&service, f, "failed");
    assert_eq!(item(&service, u)["state"], "failed");
    assert_eq!(add(&service, &a_spec), a);
    call(&service, "chore-register", &chore);
    assert_eq!(
        call(&service, "chore-claim", &claim_request)["claim"],
        claim,
        "claim day/token/fence survive cold daemon recovery"
    );
    assert!(started(root, "and-ready").is_none(), "finished job satisfies only one of the two AND prerequisites");
    // `merged` is an exact source state, not a synonym for `ready`.
    sequence += 1;
    call(
        &service,
        "source-publish",
        &json!({"source":"private-task-source","owner":"owner","publisher_token":token,"sequence":sequence,"tasks":[{"id":"task-a","title":"Synthetic task","state":"merged"}]}),
    );
    wait("named merged dependency", || started(root, "merged").is_some());
    assert!(started(root, "and-ready").is_none());
    release(root, "merged");
    settled(&service, c, "done");
    sequence += 1;
    call(
        &service,
        "source-fail",
        &json!({"source":"private-task-source","owner":"owner","publisher_token":token,"sequence":sequence,"error":"unavailable"}),
    );
    let mut stale = spec(&script, root, "stale-merged", 0, "plain", &service, "local");
    stale["dependencies"] = merged["dependencies"].clone();
    let stale_id = add(&service, &stale);
    observe("retained source rows are not fresh evidence", Duration::from_secs(2), || {
        assert!(started(root, "stale-merged").is_none());
        assert!(started(root, "and-ready").is_none());
        assert!(started(root, "after-failed").is_none());
        assert!(started(root, "after-unknown").is_none());
    });
    sequence += 1;
    call(
        &service,
        "source-publish",
        &json!({"source":"private-task-source","owner":"owner","publisher_token":token,"sequence":sequence,"tasks":[{"id":"task-a","title":"Synthetic task","state":"ready"}]}),
    );
    wait("AND now satisfies both exact states", || started(root, "and-ready").is_some());
    assert!(started(root, "stale-merged").is_none(), "ready does not satisfy merged");
    release(root, "and-ready");
    settled(&service, b, "done");
    sequence += 1;
    call(
        &service,
        "source-publish",
        &json!({"source":"private-task-source","owner":"owner","publisher_token":token,"sequence":sequence,"tasks":[{"id":"task-a","title":"Synthetic task","state":"merged"}]}),
    );
    wait("fresh merged snapshot unblocks retained item", || started(root, "stale-merged").is_some());
    release(root, "stale-merged");
    settled(&service, stale_id, "done");
    for id in [d, e] {
        assert!(item(&service, id)["dispatch"].is_null());
        call(&service, "queue-cancel", &json!({"id":id}));
    }
    service.job("stop", &json!({"job":job_id(&item(&service,u))}));
    wait("unknown workload still receives explicit stop", || {
        !alive(owned["pid"].as_u64().unwrap() as u32) && !alive(owned["child"].as_u64().unwrap() as u32)
    });
    assert_eq!(fs::read_dir(root.join("duplicates")).unwrap().count(), 0);
    assert!(!root.join("started/daily").exists(), "manual claim alone never enables or runs a chore");
    let completion = json!({"id":"private-daily","day":claim["day"],"generation":claim["generation"],"token":claim["token"],"session":claim["session"],"outcome":"done"});
    call(&service, "chore-complete", &completion);
    call(&service, "chore-complete", &completion);
    service.control(&["restart"]);
    assert!(
        result(&service, "chore-claim", &json!({"id":"private-daily","key":"after-done","session":"another-claimant"}))
            .is_err(),
        "completed local-day occurrence cannot be reclaimed"
    );
    let chores = call(&service, "chore-list", &json!({}));
    assert_eq!(
        chores["chores"].as_array().unwrap().iter().find(|v| v["id"] == "private-daily").unwrap()["outcome"],
        "done"
    );
    public(&service.state());
    private_modes(&service.root.join("state/work-queue"));
    eprintln!(
        "queue-recovery: exact_ready_merged_and_finished=true claim_crash_exec_fence_preserved=true duplicate_bodies=0"
    );
}
