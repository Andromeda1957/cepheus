use super::*;
use crate::testsupport::TempDir;

fn entry(id: u64) -> Entry {
    Entry::started(id, Tool::Claude, Some(format!("conv-{id}")), format!("session {id}"), 100 + id)
}

#[test]
fn round_trips_and_next_id_stays_ahead() {
    let dir = TempDir::new("registry-roundtrip");
    let path = dir.path().join("sessions.json");
    let reg = Registry { next_id: 2, sessions: vec![entry(1), entry(7)] };
    reg.save(&path).unwrap();
    let loaded = Registry::load(&path);
    assert_eq!(loaded.sessions, reg.sessions);
    assert_eq!(loaded.next_id, 8, "next id never reuses a recorded id");
}

#[test]
fn unreadable_file_is_kept_aside() {
    let dir = TempDir::new("registry-corrupt");
    let path = dir.write("sessions.json", "{truncated");
    let reg = Registry::load(&path);
    assert_eq!(reg.next_id, 1);
    assert!(!path.exists());
    let kept: Vec<String> =
        fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(kept.len(), 1);
    assert!(kept[0].starts_with("sessions.corrupt-"), "{kept:?}");
    assert_eq!(fs::read(dir.path().join(&kept[0])).unwrap(), b"{truncated");
}

#[test]
fn a_session_name_is_kept_and_an_older_list_reads_without_one() {
    let dir = TempDir::new("registry-name");
    let path = dir.path().join("sessions.json");
    let mut named = entry(1);
    named.name = Some("Ship the parser".into());
    let reg = Registry { next_id: 3, sessions: vec![named.clone(), entry(2)] };
    reg.save(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.matches("\"name\"").count(), 1, "no name, no field: {text}");
    assert_eq!(Registry::load(&path).sessions, vec![named, entry(2)]);
}
