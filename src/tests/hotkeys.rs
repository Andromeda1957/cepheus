use super::*;

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

fn keys(json: &str) -> (Keymap, Vec<String>) {
    Keymap::from_config(&serde_json::from_str(json).unwrap())
}

#[test]
fn a_key_is_found_however_the_terminal_spells_shift() {
    let (map, _) = keys(r#"{"stop":"alt+Q","quit":"alt+q","help":"alt+?"}"#);
    // kitty sends the unshifted key with Shift; other terminals the character.
    assert_eq!(map.find(&key(KeyCode::Char('q'), KeyModifiers::ALT | KeyModifiers::SHIFT)), Some(Hotkey::Stop));
    assert_eq!(map.find(&key(KeyCode::Char('Q'), KeyModifiers::ALT | KeyModifiers::SHIFT)), Some(Hotkey::Stop));
    assert_eq!(map.find(&key(KeyCode::Char('Q'), KeyModifiers::ALT)), Some(Hotkey::Stop));
    assert_eq!(map.find(&key(KeyCode::Char('q'), KeyModifiers::ALT)), Some(Hotkey::Quit));
    assert_eq!(map.find(&key(KeyCode::Char('/'), KeyModifiers::ALT | KeyModifiers::SHIFT)), Some(Hotkey::Help));
    assert_eq!(map.find(&key(KeyCode::Up, KeyModifiers::SUPER)), Some(Hotkey::MoveUp));
    assert_eq!(map.find(&key(KeyCode::Up, KeyModifiers::SUPER | KeyModifiers::SHIFT)), None);
    assert_eq!(map.find(&key(KeyCode::F(2), KeyModifiers::NONE)), Some(Hotkey::Errors));
    assert_eq!(map.find(&key(KeyCode::Char('w'), KeyModifiers::CONTROL)), Some(Hotkey::Window));
    assert_eq!(map.find(&key(KeyCode::Char('x'), KeyModifiers::ALT | KeyModifiers::CONTROL)), None);
    assert_eq!(Chord::parse("shift+tab"), Ok(Chord::of(&key(KeyCode::BackTab, KeyModifiers::SHIFT))));
    assert_eq!(Chord::parse("Alt+Shift+Q"), Chord::parse("alt+Q"));
    assert_eq!(Chord::parse("ctrl++").map(|c| c.label()), Ok("Ctrl-+".to_string()));
}

#[test]
fn every_default_key_reads_back_and_belongs_to_one_hotkey() {
    let (map, errors) = keys("{}");
    assert!(errors.is_empty(), "{errors:?}");
    let mut seen = Vec::new();
    for (hotkey, name, defaults, _) in HOTKEYS {
        assert_eq!(Hotkey::named(name), Some(*hotkey));
        let chords = map.chords(*hotkey);
        assert_eq!(chords.len(), defaults.len(), "{name} keeps every default");
        for chord in chords {
            assert_eq!(Chord::parse(&chord.spec()), Ok(*chord), "{name}: {}", chord.spec());
            assert!(!seen.contains(chord), "{} is the default of two hotkeys", chord.spec());
            seen.push(*chord);
        }
    }
    let labels: Vec<String> =
        [Hotkey::MoveUp, Hotkey::Errors, Hotkey::Window].iter().map(|h| map.label(*h).unwrap()).collect();
    assert_eq!(labels, ["Super+↑", "F2", "Ctrl-w"]);
}

#[test]
fn the_config_file_rebinds_and_says_what_it_could_not_use() {
    let (map, errors) = keys(
        r#"{"errors": "f3", "move_up": ["super+k", "alt+up"], "new": "alt+x", "stop": [],
            "close": "alt+bogus", "sidebar": "alt+a", "broadcast": "alt+a", "teleport": "alt+t"}"#,
    );
    assert_eq!(map.find(&key(KeyCode::F(3), KeyModifiers::NONE)), Some(Hotkey::Errors));
    assert_eq!(map.find(&key(KeyCode::F(2), KeyModifiers::NONE)), None);
    assert_eq!(map.find(&key(KeyCode::Char('k'), KeyModifiers::SUPER)), Some(Hotkey::MoveUp));
    assert_eq!(map.find(&key(KeyCode::Up, KeyModifiers::ALT)), Some(Hotkey::MoveUp));
    assert_eq!(map.find(&key(KeyCode::Up, KeyModifiers::SUPER)), None);
    // A key given to another hotkey leaves the one it was the default of.
    assert_eq!(map.find(&key(KeyCode::Char('x'), KeyModifiers::ALT)), Some(Hotkey::New));
    assert_eq!(map.label(Hotkey::Close), None, "an invalid close binding leaves it unbound");
    assert_eq!(map.label(Hotkey::Stop), None, "an empty list unbinds");
    assert_eq!(map.find(&key(KeyCode::Char('a'), KeyModifiers::ALT)), Some(Hotkey::Broadcast));
    assert_eq!(map.label(Hotkey::Sidebar), None);
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("keys.close") && e.contains("\"bogus\" is not a key")), "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("alt+a is set for both broadcast and sidebar")), "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("\"teleport\"")), "{errors:?}");
}

#[test]
fn hints_leave_out_hotkeys_without_a_key() {
    let (map, _) = keys(r#"{"close": [], "new": "ctrl+n"}"#);
    assert_eq!(map.fill("{Enter} open · {<close>} close · {<new>} new <b>"), "{Enter} open · {Ctrl-n} new <b>");
    let listing = listing(&map, "/home/me/.config/cepheus/config.json");
    assert!(listing.contains("  close         (none)"), "{listing}");
    assert!(listing.contains("  new           ctrl+n"), "{listing}");
    assert!(listing.contains("  stop          (none)"), "{listing}");
    assert!(listing.contains("\"keys\" in /home/me/.config/cepheus/config.json"), "{listing}");
}
