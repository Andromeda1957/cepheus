use super::*;

use std::net::{Ipv4Addr, Ipv6Addr};

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

#[test]
fn a_range_admits_its_addresses_and_nothing_else() {
    let ranges = parse_cidrs(&["192.0.2.0/24".into(), "198.51.100.102".into()]);
    assert_eq!(ranges.len(), 2);
    assert!(cidr_allows(&ranges, v4(192, 0, 2, 171)));
    assert!(cidr_allows(&ranges, v4(192, 0, 2, 0)));
    assert!(cidr_allows(&ranges, v4(198, 51, 100, 102)));
    assert!(!cidr_allows(&ranges, v4(198, 51, 100, 101)), "a bare address admits only itself");
    assert!(!cidr_allows(&ranges, v4(192, 168, 2, 171)));
    // An IPv4 hub reaching a dual-stack listener arrives IPv4-mapped.
    assert!(cidr_allows(&ranges, IpAddr::V6(Ipv4Addr::new(192, 0, 2, 5).to_ipv6_mapped())));
}

#[test]
fn malformed_ranges_are_left_out() {
    let ranges = parse_cidrs(&["192.0.2.0/33".into(), "nonsense".into(), "".into(), "::1/129".into()]);
    assert!(ranges.is_empty(), "{ranges:?}");
    assert!(!cidr_allows(&ranges, v4(127, 0, 0, 1)));
}

#[test]
fn ipv6_ranges_and_the_whole_internet_range_work() {
    let ranges = parse_cidrs(&["fd00::/8".into()]);
    assert!(cidr_allows(&ranges, IpAddr::V6("fd12::1".parse::<Ipv6Addr>().unwrap())));
    assert!(!cidr_allows(&ranges, IpAddr::V6(Ipv6Addr::LOCALHOST)));
    assert!(!cidr_allows(&ranges, v4(10, 0, 0, 1)), "an IPv6 range admits no IPv4 address");
    let any = parse_cidrs(&["0.0.0.0/0".into()]);
    assert!(cidr_allows(&any, v4(8, 8, 8, 8)));
}

#[test]
fn only_a_different_and_older_host_build_is_replaced() {
    let ours = ("a".repeat(64), 2_000);
    assert!(host_needs_update(&ours, PROTOCOL_VERSION, &"b".repeat(64), 1_000));
    assert!(!host_needs_update(&ours, PROTOCOL_VERSION, &"a".repeat(64), 1_000), "the same build stays");
    assert!(
        !host_needs_update(&ours, PROTOCOL_VERSION, &"b".repeat(64), 3_000),
        "a newer host build is never downgraded"
    );
    assert!(!host_needs_update(&ours, PROTOCOL_VERSION, "", 1_000), "a host that does not say its build is left alone");
    assert!(
        !host_needs_update(&(String::new(), 2_000), PROTOCOL_VERSION, &"b".repeat(64), 1_000),
        "nor when this build is unknown"
    );
}

#[test]
fn the_hub_key_is_made_once_and_kept_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::testsupport::TempDir::new("hub-key");
    assert!(existing_hub_key(dir.path()).is_none());
    let key = hub_key(dir.path()).unwrap();
    assert_eq!(key.len(), 64);
    assert_eq!(hub_key(dir.path()).unwrap(), key, "the same key every time");
    assert_eq!(existing_hub_key(dir.path()).as_deref(), Some(key.as_str()));
    let mode = fs::metadata(hub_key_path(dir.path())).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}
