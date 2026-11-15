use super::*;

use serde_json::json;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn host(name: &str) -> HostConfig {
    HostConfig {
        role: Default::default(),
        name: name.into(),
        label: Some(default_label(name)),
        tag: 0,
        addresses: vec!["192.0.2.166:8766".into()],
        ssh: Some("build-worker".into()),
        fingerprint: "f".repeat(64),
    }
}

#[test]
fn install_takes_a_host_name_and_its_options() {
    let parsed = parse_install(&args(&[
        "fedora",
        "--ssh",
        "build-worker",
        "--address",
        "192.0.2.166:8766",
        "--label",
        "Fedora",
        "--allow",
        "192.0.2.0/24",
    ]))
    .unwrap();
    assert_eq!(parsed.name, "fedora");
    assert_eq!(parsed.ssh.as_deref(), Some("build-worker"));
    assert_eq!(parsed.addresses, ["192.0.2.166:8766"]);
    assert_eq!(parsed.label.as_deref(), Some("Fedora"));
    assert_eq!(parsed.allow, ["192.0.2.0/24"]);
    assert!(parse_install(&args(&[])).is_err(), "a name is required");
    assert!(parse_install(&args(&["Fedora"])).is_err(), "names are lowercase");
    assert!(parse_install(&args(&["fedora", "--allow", "anyone"])).is_err());
    assert!(parse_install(&args(&["fedora", "controller"])).is_err(), "one host at a time");
    assert!(parse_install(&args(&["fedora", "--bogus"])).is_err());
}

#[test]
fn a_new_host_takes_the_lowest_free_number_and_a_reinstall_keeps_its_own() {
    let config = json!({"theme": "kept", "hosts": [{"name": "controller", "tag": 1, "addresses": ["x:1"], "fingerprint": "e".repeat(64), "role": "build"}]});
    let (config, fedora) = with_host(config, host("fedora")).unwrap();
    assert_eq!(fedora.tag, 2);
    assert_eq!(config["theme"], "kept");
    assert_eq!(config["hosts"].as_array().unwrap().len(), 2);
    let (config, controller) = with_host(config, host("controller")).unwrap();
    assert_eq!((controller.tag, controller.role), (1, crate::config::HostRole::Build), "a reinstall keeps its role");
    assert_eq!(config["hosts"][0]["role"], "build");

    let mut moved = host("fedora");
    moved.addresses = vec!["198.51.100.9:8766".into()];
    let (config, again) = with_host(config, moved).unwrap();
    assert_eq!(again.tag, 2, "its sessions keep their ids");
    let hosts = config["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 2);
    assert_eq!(hosts[1]["addresses"], json!(["198.51.100.9:8766"]));

    let parsed: Config = serde_json::from_value(config).unwrap();
    assert_eq!(parsed.hosts.len(), 2, "the recorded hosts load");
    assert_eq!(parsed.hosts.iter().find(|h| h.name == "fedora").unwrap().label(), "Fedora");

    let (fresh, first) = with_host(Value::Null, host("fedora")).unwrap();
    assert_eq!(first.tag, 1);
    assert_eq!(fresh["hosts"].as_array().unwrap().len(), 1);
    assert!(with_host(json!({"hosts": "nope"}), host("fedora")).is_err());
}

#[test]
fn removing_a_host_leaves_the_others() {
    let config = json!({"hosts": [{"name": "fedora", "tag": 2}, {"name": "controller", "tag": 1}]});
    let (config, removed) = without_host(config, "fedora");
    assert!(removed);
    assert_eq!(config["hosts"], json!([{"name": "controller", "tag": 1}]));
    let (_, removed) = without_host(config, "fedora");
    assert!(!removed);
}

#[test]
fn the_allowed_range_is_the_network_the_hub_reaches_the_host_from() {
    use std::net::Ipv4Addr;
    assert_eq!(network(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 171)), 24), "192.0.2.0/24");
    assert_eq!(network(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 102)), 32), "198.51.100.102/32");
    assert_eq!(network(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 102)), 0), "0.0.0.0/0");
    assert_eq!(network("fd12:3456::9".parse().unwrap(), 64), "fd12:3456::/64");
    assert!(hosts::parse_cidr(&network(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 171)), 24)).is_some());
}

/// The VPN runs client-to-client, so every peer on it reaches a host there:
/// on that point-to-point link a host admits only the hub's own address, as
/// the phone gateway admits only the phone. On the LAN it admits the range,
/// since DHCP moves the hub's address within it.
#[test]
fn a_host_admits_the_hubs_lan_range_but_only_its_own_vpn_address() {
    assert_eq!(hub_range("192.0.2.171".parse().unwrap(), 24, false), "192.0.2.0/24");
    assert_eq!(hub_range("198.51.100.102".parse().unwrap(), 24, true), "198.51.100.102/32");
    assert_eq!(hub_range("fd12:3456::9".parse().unwrap(), 64, true), "fd12:3456::9/128");
}

#[test]
fn the_ssh_alias_resolves_to_its_host_name() {
    let out = "user builder\nhostname 192.0.2.166\nport 22\n";
    assert_eq!(ssh_hostname(out).as_deref(), Some("192.0.2.166"));
    assert_eq!(ssh_hostname("user builder\n"), None);
    assert_eq!(default_label("fedora"), "Fedora");
}
