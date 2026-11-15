use super::*;
use crate::remote::auth;
use crate::testsupport::TempDir;

fn remote_on() -> Config {
    Config { remote: true, remote_listen: "198.51.100.102:8765".into(), ..Config::default() }
}

#[test]
fn half_blocks_put_two_module_rows_in_each_text_row() {
    let modules = vec![vec![true, false, true, false], vec![true, true, false, false], vec![false, true, true, false]];
    assert_eq!(half_blocks(&modules), ["█▄▀ ", " ▀▀ "], "an odd last row has a light lower half");
}

#[test]
fn the_code_has_its_finder_patterns_inside_a_light_border() {
    let data = payload("198.51.100.102", 8765, &"ab".repeat(32), "ABCD2345");
    assert_eq!(data, format!("agentdeck://pair?host=198.51.100.102&port=8765&fp={}&code=ABCD2345", "ab".repeat(32))); // legacy-name
    let modules = qr_modules(&data).unwrap();
    let size = modules.len();
    assert!(modules.iter().all(|row| row.len() == size), "square");
    let width = size - 2 * QUIET;
    assert_eq!((width - 17) % 4, 0, "a QR symbol is 4n+17 modules wide");
    assert!(width <= 41, "small enough to fit a 30-row window: {width}");
    for row in &modules {
        for edge in [&row[..QUIET], &row[size - QUIET..]] {
            assert!(edge.iter().all(|m| !m), "the quiet zone is light");
        }
    }
    // The three 7×7 finder patterns: a dark ring, a light ring, a dark 3×3.
    for (ox, oy) in [(0, 0), (width - 7, 0), (0, width - 7)] {
        for y in 0..7 {
            for x in 0..7 {
                let ring = x.min(y).min(6 - x).min(6 - y);
                assert_eq!(
                    modules[QUIET + oy + y][QUIET + ox + x],
                    ring != 1,
                    "finder at ({ox}, {oy}) module ({x}, {y})"
                );
            }
        }
    }
    let rows = half_blocks(&modules);
    assert_eq!(rows.len(), size.div_ceil(2));
    assert_eq!(
        payload("fd00::2", 8765, "f", "abcd-2345"),
        "agentdeck://pair?host=[fd00::2]&port=8765&fp=f&code=ABCD2345" // legacy-name
    );
}

#[test]
fn the_shown_code_pairs_once_and_closing_the_screen_ends_it() {
    let dir = TempDir::new("pairing-screen");
    let remote = dir.path().join("remote");
    let off = Pairing::open(&remote, &Config::default(), 1_000);
    assert!(off.offer.as_ref().unwrap_err().contains("\"remote\": true"), "the gateway is off");
    let early = Pairing::open(&remote, &remote_on(), 1_000);
    assert!(early.offer.as_ref().unwrap_err().contains("not started yet"), "no certificate yet");

    let identity = store::load_or_create_identity(&remote, "198.51.100.102").unwrap();
    let screen = Pairing::open(&remote, &remote_on(), 1_000);
    let offer = screen.offer.as_ref().unwrap();
    assert_eq!(offer.address, "198.51.100.102:8765");
    assert_eq!(offer.fingerprint, identity.fingerprint);
    assert_eq!(offer.expires, 1_000 + store::PAIRING_SECS);
    assert_eq!(offer.code.len(), 9, "{}", offer.code);
    assert!(!screen.expired(1_000 + store::PAIRING_SECS - 1) && screen.expired(1_000 + store::PAIRING_SECS));
    // The code as shown, typed on the phone, is the one the gateway takes.
    auth::redeem_code(&remote, &offer.code, 1_010).unwrap();
    assert!(auth::redeem_code(&remote, &offer.code, 1_011).is_err(), "once");

    let mut screen = Pairing::open(&remote, &remote_on(), 2_000);
    let first = screen.offer.as_ref().unwrap().code.clone();
    screen.renew(&remote_on(), 2_100);
    let second = screen.offer.as_ref().unwrap().code.clone();
    assert!(auth::redeem_code(&remote, &second, 2_101).is_ok(), "the new code works");
    if first != second {
        screen.renew(&remote_on(), 2_200);
        assert!(auth::redeem_code(&remote, &first, 2_201).is_err(), "the code it replaced does not");
    }
    screen.renew(&remote_on(), 2_300);
    screen.close();
    let code = screen.offer.as_ref().unwrap().code.clone();
    assert!(auth::redeem_code(&remote, &code, 2_301).is_err(), "closing the screen ends the code");

    let wildcard = Config { remote_listen: "0.0.0.0:8765".into(), ..remote_on() };
    assert!(Pairing::open(&remote, &wildcard, 3_000).offer.unwrap_err().contains("the address the phone reaches"));
}

#[test]
fn a_removed_phone_leaves_the_list_on_disk() {
    let dir = TempDir::new("pairing-remove");
    let (a, _) = auth::new_device("Pixel", 1).unwrap();
    let (b, _) = auth::new_device("Tablet", 2).unwrap();
    store::save_devices(dir.path(), &[a.clone(), b.clone()]).unwrap();
    let mut screen = Pairing::open(dir.path(), &Config::default(), 10);
    assert_eq!(screen.devices.len(), 2);
    screen.select(5);
    assert_eq!(screen.sel, 1);
    // The gateway records a sign-in meanwhile; removing keeps it.
    let seen = Device { last_seen: Some(99), ..a.clone() };
    store::save_devices(dir.path(), &[seen.clone(), b.clone()]).unwrap();
    assert_eq!(screen.remove_selected().unwrap().map(|d| d.id), Some(b.id));
    assert_eq!(store::load_devices(dir.path()), vec![seen.clone()]);
    assert_eq!(screen.devices, vec![seen]);
    assert_eq!(screen.sel, 0);
    screen.remove_selected().unwrap();
    assert!(store::load_devices(dir.path()).is_empty());
    assert_eq!(screen.remove_selected().unwrap(), None, "nothing left to remove");
}
