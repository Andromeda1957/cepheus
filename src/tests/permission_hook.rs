use super::*;
#[test]
fn untrusted_requests_reject_duplicate_unknown_and_oversized_native_inputs() {
    assert!(decode(br#"{"a":{"x":1,"x":2}}"#).is_none());
    assert!(decode(b"[]").is_none());
    let good = json!({"hook_event_name":"PermissionRequest","tool_name":"Read","session_id":"conversation","prompt_id":"turn","cwd":"/work","tool_input":{"file_path":"/work/file"}});
    assert!(request(&good));
    for (field, value) in [
        ("cwd", json!("relative")),
        ("session_id", json!("served:untrusted")),
        ("prompt_id", json!("")),
        ("tool_input", json!({"file_path":"/work/file","unknown":true})),
        ("tool_input", json!({"file_path":"/work/file","offset":true})),
        ("tool_input", json!({"file_path":"/work/file","limit":10001})),
    ] {
        let mut bad = good.clone();
        bad[field] = value;
        assert!(!request(&bad));
    }
    let mut nested = json!({});
    for _ in 0..18 {
        nested = json!({"nested":nested});
    }
    assert!(decode(&serde_json::to_vec(&nested).unwrap()).is_none());
}
