use super::*;
use crate::testsupport::TempDir;
use serde_json::json;
fn service(d: &TempDir) -> Service {
    let (tx, _) = mpsc::channel();
    Service::new(super::super::tests::options(d, "true"), tx)
}
#[test]
fn supervisor_off_retry_never_reapplies_an_old_generation_after_owner_reenable() {
    let d = TempDir::new("attention-off-generation");
    let mut svc = service(&d);
    let args = json!({"enabled":false,"origin":"origin-a","generation":5});
    svc.attention_command("attention-supervisor-set", &args).unwrap();
    svc.attention_command("attention-supervisor-set", &json!({"enabled":true})).unwrap();
    let revision = svc.attention_snapshot().supervisor.revision;
    let reply = svc.attention_command("attention-supervisor-set", &args).unwrap();
    assert_eq!(reply["supervisor"]["enabled"], true);
    assert_eq!(svc.attention_snapshot().supervisor.revision, revision);
    assert!(svc
        .attention_command("attention-supervisor-set", &json!({"enabled":false,"origin":"origin-a","generation":4}))
        .is_err());
    svc.attention_command("attention-supervisor-set", &json!({"enabled":false,"origin":"origin-a","generation":6}))
        .unwrap();
    assert!(!svc.attention_snapshot().supervisor.enabled);
}
