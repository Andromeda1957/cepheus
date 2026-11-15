use super::*;

// A changed pet or picture must load new art, or the sidebar keeps the old
// pet after the owner changes it in ChatGPT.
#[test]
fn art_key_changes_with_the_pet_or_picture() {
    let pet = Profile { pet: Some("codex".into()), image: Some("a".into()), ..Default::default() };
    let other = Profile { pet: Some("hoots".into()), ..pet.clone() };
    assert_ne!(pet.art_key(), other.art_key());
    // A pet with no public sheet falls back to the stored picture.
    let custom = Profile {
        pet: Some("mine".into()),
        file: Some("avatar.png".into()),
        image: Some("b".into()),
        ..Default::default()
    };
    let repainted = Profile { image: Some("c".into()), ..custom.clone() };
    assert!(custom.art_key().is_some());
    assert_ne!(custom.art_key(), repainted.art_key());
}

// The picture name comes from a file another process writes; it must never
// lead outside the Dot state folder.
#[test]
fn profile_ignores_unsafe_picture_names_and_control_characters() {
    let dir = std::env::temp_dir().join(format!("deck-pet-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("profile.json"), r#"{"name":" AndroDot ","pet":"codex","file":"../x.png"}"#).unwrap();
    let profile = Profile::read(&dir).unwrap();
    assert_eq!(profile.name.as_deref(), Some("AndroDot"));
    assert_eq!(profile.file, None);
    std::fs::write(dir.join("profile.json"), "{\"name\":\"a\\u001b[2Jb\"}").unwrap();
    assert_eq!(Profile::read(&dir).unwrap().name, None);
    std::fs::remove_dir_all(&dir).unwrap();
}
