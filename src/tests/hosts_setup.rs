use super::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn setup_needs_a_listen_address_and_valid_ranges() {
    let parsed = parse_args(&args(&[
        "--label",
        "Fedora",
        "--listen",
        "0.0.0.0:8766",
        "--allow",
        "192.0.2.0/24",
        "--allow",
        "198.51.100.102/32",
    ]))
    .unwrap();
    assert_eq!(
        parsed,
        SetupArgs {
            label: "Fedora".into(),
            listen: "0.0.0.0:8766".into(),
            allow: vec!["192.0.2.0/24".into(), "198.51.100.102/32".into()]
        }
    );
    assert!(parse_args(&args(&["--allow", "192.0.2.0/24"])).is_err(), "no listen address");
    assert!(parse_args(&args(&["--listen", "0.0.0.0:8766"])).is_err(), "nobody allowed");
    assert!(parse_args(&args(&["--listen", "0.0.0.0:8766", "--allow", "everyone"])).is_err());
    assert!(parse_args(&args(&["--listen"])).is_err());
}

#[test]
fn setup_turns_the_listener_on_and_keeps_the_hosts_own_settings() {
    let a = SetupArgs { label: "Fedora".into(), listen: "0.0.0.0:8766".into(), allow: vec!["192.0.2.0/24".into()] };
    let merged =
        merged_config(json!({"scrollback": 5000, "peer_allow": ["1.2.3.4"], "notifications": true}), &a).unwrap();
    assert_eq!(merged["scrollback"], 5000);
    assert_eq!(merged["host_name"], "Fedora");
    assert_eq!(merged["peer_listen"], "0.0.0.0:8766");
    assert_eq!(merged["peer_allow"], json!(["192.0.2.0/24"]), "install owns the allowed ranges");
    assert_eq!(merged["notifications"], true, "the host owner's choice stays");

    let fresh = merged_config(Value::Null, &a).unwrap();
    assert_eq!(fresh["notifications"], false, "the hub raises alerts where the owner is");
    let config: Config = serde_json::from_value(fresh).unwrap();
    assert_eq!(config.peer_listen, "0.0.0.0:8766");
    assert_eq!(config.local_host_label(), "Fedora");

    assert!(merged_config(json!([1, 2]), &a).is_err(), "a config that is not an object is not replaced");
}
