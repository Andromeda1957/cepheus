//! Explicit trusted production adapter provenance; builtin screens never attest.
use super::*;
use std::os::fd::AsRawFd;

/// Disconnect only the owned peer's listener. The hub's exact authenticated
/// endpoint binding stays unchanged while the durable fleet-off is pending.
struct Disconnected<'a> {
    hub: &'a Service,
    peer: &'a Service,
    hosts: Value,
    listen: Value,
    restored: bool,
}
impl<'a> Disconnected<'a> {
    fn new(hub: &'a Service, peer: &'a Service) -> Self {
        let hosts = read_json(&hub.root.join("config.json")).unwrap()["hosts"].clone();
        let listen = read_json(&peer.root.join("config.json")).unwrap()["peer_listen"].clone();
        assert!(listen.as_str().is_some_and(|value| !value.is_empty()));
        let value = Self { hub, peer, hosts, listen, restored: false };
        value.peer.configure(|config| config["peer_listen"] = json!(""));
        wait("owned peer listener exits and authenticated link goes offline", || {
            let path = value.peer.root.join("state/peer/peer.lock");
            let quiet = match fs::OpenOptions::new().read(true).write(true).open(path) {
                Ok(file) => unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 },
                Err(error) => error.kind() == std::io::ErrorKind::NotFound,
            };
            quiet && value.hub.state()["host_schedulers"]["worker"].is_null()
        });
        assert_eq!(read_json(&value.hub.root.join("config.json")).unwrap()["hosts"], value.hosts);
        value
    }
    fn restore_config(&self) -> std::io::Result<()> {
        let path = self.peer.root.join("config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path)?)?;
        config["peer_listen"] = self.listen.clone();
        let next = path.with_extension("listener-reconnect");
        fs::write(&next, config.to_string())?;
        fs::rename(next, path)
    }
    fn restore(&mut self) {
        self.restore_config().unwrap();
        wait("same authenticated endpoint reconnects through its own supervised listener", || {
            self.hub.state()["host_schedulers"]["worker"].is_object()
        });
        assert_eq!(read_json(&self.hub.root.join("config.json")).unwrap()["hosts"], self.hosts);
        self.restored = true;
    }
}
impl Drop for Disconnected<'_> {
    fn drop(&mut self) {
        if !self.restored && self.restore_config().is_err() {
            let _ = fs::write(
                self.hub.root.parent().unwrap().join("cleanup-incomplete"),
                "Own peer listener config restore failed; preserve private receipts.\n",
            );
        }
    }
}
fn enters(root: &Path, id: u64) -> usize {
    events(root, "hub", id).iter().filter(|v| v["category"] == "permission_enter").count()
}
fn packet(root: &Path, id: u64, request: &str, overrides: Value) {
    control(
        root,
        "hub",
        id,
        json!({"mode":"trusted_permission","redraw":false,"permission_request":request,"packet_overrides":overrides,"hook":false}),
    );
}
fn no_new_enter(root: &Path, id: u64, n: usize) {
    observe(Duration::from_secs(1), || {
        assert_eq!(enters(root, id), n, "permission input changed without complete current authority")
    });
}

#[test]
fn trusted_adapter_executes_one_harmless_allow_once_and_current_policy_revocation_fails_closed() {
    let scratch = Scratch::new("attention-supervisor");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let fake = install_fake(root);
    let mut fleet = AttentionFleet::new(root, &binary);
    configure(&fleet.hub, root, &fake, "hub");
    configure(&fleet.peer, root, &fake, "peer");
    fleet
        .hub
        .configure(|config| config["attention"]["permission_adapters"] = json!({"codex":fake.canonicalize().unwrap()}));
    let cwd = fleet.hub.root.join("work");
    fs::write(cwd.join("safe.txt"), "OWNED_TRUSTED_TOOL_OUTPUT\n").unwrap();
    connect(&fleet.hub, &fleet.peer);
    fleet.peer.start();
    fleet.hub.start();
    ready(&fleet.hub);
    ready(&fleet.peer);
    wait("authenticated peer supervisor capability", || fleet.hub.state()["host_schedulers"]["worker"].is_object());
    let id = start(&fleet.hub, "codex", "@here", &cwd, "private-trusted-adapter");
    let unsupported = start(&fleet.hub, "claude", "@here", &cwd, "private-unproven-builtin");
    wait("both hosted provider identities are bound", || {
        [id, unsupported].iter().all(|id| {
            session(&fleet.hub, *id)["status"] == "finished" && session(&fleet.hub, *id)["conversation"].is_string()
        })
    });
    let rules = json!([{"id":"private-cat-once","tool":"Bash","pattern":{"kind":"exact","command":"cat safe.txt"}}]);
    call(&fleet.hub, "attention-supervisor-rules", &json!({"rules":rules}));
    control(root, "hub", id, json!({"mode":"trusted_permission","hook":false}));
    wait("actual selected allow-once prompt", || session(&fleet.hub, id)["status"] == "needs_you");
    packet(root, id, "off-request", json!({}));
    no_new_enter(root, id, 0);
    call(&fleet.hub, "attention-supervisor-set", &json!({"enabled":true}));
    packet(root, id, "one-request", json!({}));
    wait("real approved literal tool completes", || {
        events(root, "hub", id).iter().any(|v| v["category"] == "tool_completed" && v["exit_code"] == 0)
    });
    assert_eq!(enters(root, id), 1);
    let output = records(&root.join(format!("tool-output-{}.jsonl", key("hub", id))));
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["stdout"], "OWNED_TRUSTED_TOOL_OUTPUT\n");
    assert_eq!(
        events(root, "hub", id)
            .iter()
            .filter(|v| v["category"] == "raw_input")
            .map(|v| v["bytes"].as_u64().unwrap())
            .sum::<u64>(),
        1,
        "exactly one Enter; no option navigation or typed answer"
    );
    packet(root, id, "one-request", json!({}));
    no_new_enter(root, id, 1);
    call(&fleet.hub, "attention-supervisor-set", &json!({"enabled":false}));
    packet(root, id, "disabled-after-answer", json!({}));
    no_new_enter(root, id, 1);
    call(&fleet.hub, "attention-supervisor-rules", &json!({"rules":[]}));
    call(&fleet.hub, "attention-supervisor-set", &json!({"enabled":true}));
    packet(root, id, "revoked-rules", json!({}));
    no_new_enter(root, id, 1);
    call(&fleet.hub, "attention-supervisor-rules", &json!({"rules":rules}));
    packet(root, id, "incomplete-request", json!({"complete":false}));
    no_new_enter(root, id, 1);
    control(root, "hub", id, json!({"mode":"trusted_permission","selection":"deny","hook":false}));
    packet(root, id, "wrong-selected-option", json!({}));
    no_new_enter(root, id, 1);
    control(root, "hub", unsupported, json!({"mode":"trusted_permission","hook":false}));
    wait("builtin without configured adapter has same actionable-looking screen", || {
        session(&fleet.hub, unsupported)["status"] == "needs_you"
    });
    no_new_enter(root, unsupported, 0);
    // Actual offline peer proves 'fleet off' distinguishes pending from an ACK.
    call(&fleet.peer, "attention-supervisor-set", &json!({"enabled":true}));
    let mut disconnected = Disconnected::new(&fleet.hub, &fleet.peer);
    let off = call(&fleet.hub, "attention-supervisor-off", &json!({"fleet":true}));
    assert_eq!(off["supervisor"]["enabled"], false);
    assert_eq!(off["supervisor"]["pending"], json!(["worker"]));
    assert_eq!(
        attention(&fleet.peer)["supervisor"]["enabled"],
        true,
        "offline peer has not acknowledged local disable"
    );
    disconnected.restore();
    wait("fleet off acknowledged by original authenticated peer", || {
        attention(&fleet.hub)["supervisor"]["pending"].as_array().is_some_and(Vec::is_empty)
            && attention(&fleet.peer)["supervisor"]["enabled"] == false
    });
    packet(root, id, "after-fleet-off", json!({}));
    no_new_enter(root, id, 1);
    let state = attention(&fleet.hub);
    public(&state);
    assert!(state["supervisor"]["audit"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["session"] == id && v["category"] == "prepared"));
    for entry in fs::read_dir(fleet.hub.root.join("state/attention/permission-requests")).unwrap() {
        let receipt = read_json(&entry.unwrap().path()).unwrap();
        let metadata = receipt.as_object().unwrap();
        for field in
            ["command", "argv", "env", "token", "fingerprint", "question", "selected", "cwd", "executable", "text"]
        {
            assert!(!metadata.contains_key(field), "private {field} leaked into durable action audit");
        }
        let encoded = receipt.to_string();
        for text in ["cat safe.txt", "OWNED_TRUSTED_TOOL_OUTPUT", root.to_str().unwrap()] {
            assert!(!encoded.contains(text), "action audit contains raw private intent instead of categories/ids");
        }
    }
    assert_eq!(records(&root.join(format!("tool-output-{}.jsonl", key("hub", id)))).len(), 1);
    eprintln!("attention-supervisor: production_trusted_adapter=true real_fixed_cat_exec=1 guarded_enters=1 duplicate_request=0 builtin_unproven=0 fleet_off_pending_then_ack=true");
}
