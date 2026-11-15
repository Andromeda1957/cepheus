//! Actual reopened process generations fence an unadmitted retry, while an
//! already admitted key continues to reconcile the original durable receipt.
use super::*;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;

fn end_owned_actor(root: &Path, script: &Path, service: &Service, id: u64, pid: u32) {
    let identity = fs::read_dir(root.join("actors"))
        .unwrap()
        .filter_map(|entry| entry.ok().and_then(|entry| read_json(&entry.path())))
        .find(|value| value["pid"] == pid && value["session"] == id && value["host"] == "hub")
        .expect("exact private hosted actor receipt");
    assert!(pid > 1 && pid != service.pid());
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(raw >= 0, "pin own hosted actor: {}", std::io::Error::last_os_error());
    let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    assert_eq!(identity["uid"].as_u64(), Some(unsafe { libc::geteuid() } as u64));
    assert_eq!(identity["uid"].as_u64(), Some(fs::metadata(&proc_dir).unwrap().uid() as u64));
    assert_eq!(identity["boot"].as_str(), Some(fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim()));
    let stat = fs::read_to_string(proc_dir.join("stat")).unwrap();
    let birth = stat.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap().parse::<u64>().unwrap();
    assert_eq!(identity["start"].as_u64(), Some(birth));
    let command = fs::read(proc_dir.join("cmdline")).unwrap();
    let args = command.split(|byte| *byte == 0).collect::<Vec<_>>();
    assert_eq!(args.get(1).copied(), Some(script.as_os_str().as_encoded_bytes()));
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGTERM,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        },
        0,
        "end only this private actor, leaving the deck session resumable"
    );
}

#[test]
fn immutable_sender_target_rejects_unadmitted_retry_after_reopen_but_reconciles_admitted_key() {
    let scratch = Scratch::new("attention-target");
    let root = &scratch.0;
    let binary = backend();
    provenance(&binary);
    let fake = install_fake(root);
    let mut fleet = AttentionFleet::new(root, &binary);
    configure(&fleet.hub, root, &fake, "hub");
    configure(&fleet.peer, root, &fake, "peer");
    fleet.hub.start();
    ready(&fleet.hub);
    let caps: Value = serde_json::from_slice(&fleet.hub.run(&["ctl", "fleet", "capabilities"], None).unwrap()).unwrap();
    assert!(
        caps["protocol"].as_u64().is_some_and(|version| version >= 17),
        "captured target admission requires protocol17"
    );
    let id = start(&fleet.hub, "claude", "@here", &fleet.hub.root.join("work"), "private-frozen-target");
    wait("initial actor has a known conversation", || {
        let row = session(&fleet.hub, id);
        row["status"] == "finished" && row["conversation"].is_string() && row["pid"].as_u64().is_some()
    });
    control(root, "hub", id, json!({"mode":"working","hook":false}));
    wait("original target working", || session(&fleet.hub, id)["status"] == "working");
    // The first outbound packet can be lost before the origin admits it. Keep
    // that exact locally captured payload; never refresh its target on retry.
    let never_admitted =
        FrozenAttentionSend::new(&fleet.hub, "lost-before-admission", id, "ATTENTION_PRIVATE_PAYLOAD unadmitted");
    let admitted = FrozenAttentionSend::new(&fleet.hub, "already-admitted", id, "ATTENTION_PRIVATE_PAYLOAD admitted");
    let frozen_unadmitted = never_admitted.args.clone();
    let frozen_admitted = admitted.args.clone();
    let mid = admitted.id(&fleet.hub);
    wait("admitted original receipt held", || message(&fleet.hub, &mid)["state"] == "held");
    assert_eq!(attention(&fleet.hub)["messages"].as_array().unwrap().len(), 1);
    quiet(root, "hub", id, Duration::from_secs(1));
    let original = session(&fleet.hub, id);
    let pid = original["pid"].as_u64().unwrap().try_into().unwrap();
    // ctl close intentionally removes a session; ending our own process instead
    // preserves the real session identity used by the production Reopen path.
    end_owned_actor(root, &fake, &fleet.hub, id, pid);
    wait("original actor ended with resumable session retained", || session(&fleet.hub, id)["alive"] == false);
    fleet.hub.run(&["ctl", "reopen", &id.to_string()], None).unwrap();
    wait("same opaque session id has a different live process generation", || {
        let row = session(&fleet.hub, id);
        row["alive"] == true
            && row["spawned_ms"].as_u64().is_some_and(|value| value != original["spawned_ms"].as_u64().unwrap())
            && row["pid"].as_u64().is_some_and(|value| value > 1)
            && row["conversation"] == original["conversation"]
    });
    assert!(never_admitted.result(&fleet.hub).is_err(), "an absent-key retry cannot retarget a reopened process");
    assert_eq!(admitted.id(&fleet.hub), mid, "an admitted key reconciles its original receipt after reopen");
    assert_eq!(never_admitted.args, frozen_unadmitted);
    assert_eq!(admitted.args, frozen_admitted);
    let snapshot = attention(&fleet.hub);
    let messages = snapshot["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1, "the rejected retry must not create a new receipt");
    assert_eq!(messages[0]["id"], mid);
    quiet(root, "hub", id, Duration::from_secs(1));
    assert!(events(root, "hub", id).iter().all(|event| event["category"] != "owner_input"));
    public(&snapshot);
    eprintln!(
        "attention-target: generation_changed=true frozen_unadmitted_rejected=1 admitted_receipts=1 extra_deliveries=0"
    );
}
