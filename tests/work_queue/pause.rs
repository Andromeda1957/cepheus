//! An accepted remote intent must obey the origin queue's acknowledged pause.
//! Two private TLS hosts share one physical machine. The peer remains resource
//! held while disconnected: a pending pause is explicitly not a guarantee.
use super::*;
use std::net::TcpListener;

/// Only the fixture hub's own configured address is changed. The bound local
/// listener cannot authenticate TLS and is kept alive until the original
/// configuration is restored. Drop restores connectivity before Fleet custody
/// teardown, including when an assertion unwinds.
struct DisconnectedPeer<'a> {
    hub: &'a Service,
    original_hosts: Value,
    listener: TcpListener,
    restored: bool,
}
impl<'a> DisconnectedPeer<'a> {
    fn new(hub: &'a Service, binary: &Path) -> Self {
        let listener = loop {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            if listener.local_addr().unwrap().port() != 8000 {
                break listener;
            }
        };
        let original_hosts = read_json(&hub.root.join("config.json")).unwrap()["hosts"].clone();
        assert_eq!(original_hosts.as_array().unwrap().len(), 1);
        assert_eq!(original_hosts[0]["name"], "worker");
        let disconnected = Self { hub, original_hosts, listener, restored: false };
        let address = disconnected.listener.local_addr().unwrap().to_string();
        hub.configure(|config| config["hosts"][0]["addresses"] = json!([address]));
        let birth = service_birth(hub);
        hub.replace_binary(binary);
        hub.control(&["restart"]);
        wait("same owning daemon execs with only its worker address disconnected", || {
            handover_ready(hub, &birth) && hub.state()["host_schedulers"]["worker"].is_null()
        });
        disconnected
    }
    fn restore_config(&self) -> std::io::Result<()> {
        let path = self.hub.root.join("config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path)?)?;
        config["hosts"] = self.original_hosts.clone();
        let next = path.with_extension("reconnect");
        fs::write(&next, config.to_string())?;
        fs::rename(next, path)
    }
    fn restore(&mut self, binary: &Path) {
        self.restore_config().unwrap();
        let birth = service_birth(self.hub);
        self.hub.replace_binary(binary);
        self.hub.control(&["restart"]);
        wait("same owning daemon execs and reconnects authenticated worker", || {
            handover_ready(self.hub, &birth) && self.hub.state()["host_schedulers"]["worker"].is_object()
        });
        self.restored = true;
    }
}
impl Drop for DisconnectedPeer<'_> {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        if self.restore_config().is_err() || self.hub.run(&["restart"], None).is_err() {
            let _ = fs::write(
                self.hub.root.parent().unwrap().join("cleanup-incomplete"),
                "Fixture peer address could not be restored; preserve private intent receipts for custody cleanup.\n",
            );
            eprintln!("private queue pause fixture could not restore its own worker address");
        }
    }
}

fn quota(service: &Service, percent: u64) {
    let path = service.root.join("state/claude-usage.json");
    let next = path.with_extension("next");
    fs::write(
        &next,
        json!({"updated_at":now(),"rate_limits":{"five_hour":{"used_percentage":percent,"resets_at":null}}})
            .to_string(),
    )
    .unwrap();
    fs::rename(next, path).unwrap();
}
fn observed_quota(service: &Service, percent: u64) -> bool {
    service.state()["usage"]["claude"]
        .as_array()
        .is_some_and(|windows| windows.iter().any(|window| window["used_percent"].as_f64() == Some(percent as f64)))
}
fn paused(value: &Value) -> bool {
    value["enabled"] == false
        && value["pause_effective"] == true
        && value["pause_pending"].as_array().is_some_and(Vec::is_empty)
}

#[test]
fn accepted_remote_intent_waits_for_effective_pause_and_runs_once_after_reenable() {
    let scratch = Scratch::new("queue-peer-pause");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let script = actor_script(root);
    let mut fleet = QueueFleet(Fleet {
        hub: Service::new(root.join("hub"), &binary, 12288),
        peer: Service::new(root.join("peer"), &binary, 14336),
    });
    let (hub, peer) = (&mut fleet.0.hub, &mut fleet.0.peer);
    connect(hub, peer);
    quota(peer, 95);
    peer.start();
    hub.start();
    ready(hub);
    ready(peer);
    wait("worker advertises eligible memory but has a provider quota hold", || {
        hub.state()["host_schedulers"]["worker"].is_object() && observed_quota(peer, 95)
    });
    let mut request = spec(&script, root, "paused-peer", 0, "plain", hub, "worker");
    // The literal Python body spends no provider quota. This tag exercises the
    // actual provider policy using private measured usage, not a real CLI.
    request["provider"] = json!("claude");
    let id = add(hub, &request);
    call(hub, "queue-enable", &json!({"enabled":true}));
    wait("remote immutable intent accepted but held before its body", || {
        let item = item(hub, id);
        item["state"] == "held"
            && item["dispatch"].is_object()
            && item["cancelable"] == false
            && peer.jobs().len() == 1
            && peer.jobs()[0]["status"] == "starting"
            && peer.jobs()[0]["reason"].as_str().is_some_and(|reason| reason.contains("quota"))
    });
    let job = job_id(&item(hub, id));
    assert_eq!(peer.info(job)["status"], "starting");
    assert!(started(root, "paused-peer").is_none());
    // Admission stays blocked until the returning peer acknowledges pause.
    peer.memory(1024);
    let mut disconnected = DisconnectedPeer::new(hub, &binary);
    let disabled = call(hub, "queue-enable", &json!({"enabled":false}));
    assert_eq!(disabled["enabled"], false);
    assert_eq!(disabled["pause_effective"], false);
    assert_eq!(disabled["pause_pending"], json!(["worker"]));
    observe("offline pause stays honestly pending", Duration::from_secs(2), || {
        let view = call(hub, "queue-list", &json!({}));
        assert_eq!(view["pause_pending"], json!(["worker"]));
        assert_eq!(view["pause_effective"], false);
        assert!(started(root, "paused-peer").is_none());
        public(&view);
    });
    disconnected.restore(&binary);
    wait("returning worker acknowledges the exact disabled policy", || paused(&call(hub, "queue-list", &json!({}))));
    assert!(paused(&hub.state()["work_queue"]));
    peer.memory(14336);
    quota(peer, 0);
    wait("worker resource and provider holds clear", || observed_quota(peer, 0));
    observe("eligible body cannot cross an acknowledged paused gate", Duration::from_secs(2), || {
        assert!(paused(&call(hub, "queue-list", &json!({}))));
        assert!(started(root, "paused-peer").is_none());
        assert_eq!(peer.info(job)["status"], "starting");
        assert_eq!(fs::read_dir(root.join("duplicates")).unwrap().count(), 0);
    });
    let enabled = call(hub, "queue-enable", &json!({"enabled":true}));
    assert_eq!(enabled["enabled"], true);
    assert_eq!(enabled["pause_effective"], false);
    wait("re-enabled accepted intent starts exactly once", || {
        started(root, "paused-peer").is_some() && peer.info(job)["status"] == "working"
    });
    assert_eq!(job_id(&item(hub, id)), job);
    assert_eq!(add(hub, &request), id);
    assert_eq!(peer.jobs().len(), 1);
    assert_eq!(fs::read_dir(root.join("started")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(root.join("duplicates")).unwrap().count(), 0);
    release(root, "paused-peer");
    settled(hub, id, "done");
    assert_eq!(hub.info(job)["exit_code"], 0);
    assert_eq!(peer.info(job)["exit_code"], 0);
    let intent = read_json(&hub.root.join("state/jobs").join(format!("{job:010x}")).join("intent.json")).unwrap();
    let binding = intent["gate_binding"].as_str().expect("private queue execution binding");
    for view in [hub.state(), call(hub, "queue-list", &json!({}))] {
        public(&view);
        assert!(!view.to_string().contains(binding), "execution gate credentials remain private");
    }
    private_modes(&hub.root.join("state/jobs"));
    private_modes(&peer.root.join("state/jobs"));
    eprintln!("queue-pause-proof: accepted_remote_intents=1 offline_pending=true pause_ack_before_resource_release=true bodies_while_effectively_paused=0 reenabled_bodies=1 logical_tls_hosts=2 physical_hosts=1");
}
