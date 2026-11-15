use super::*;

#[test]
fn a_name_stays_in_its_folder_and_types_without_quoting() {
    assert_eq!(safe_name("../My shot (1).png"), "My_shot_1_.png");
    assert_eq!(safe_name("a/b\\c.txt"), "c.txt");
    assert_eq!(safe_name(".bashrc"), "bashrc", "never hidden, so never a leftover piece's name");
    assert_eq!(safe_name(".."), "file");
    assert_eq!(safe_name(""), "file");
    assert_eq!(safe_name("Résumé.pdf"), "R_sum_.pdf");
    assert_eq!(safe_name("IMG_2041.JPG"), "IMG_2041.JPG");
    let long = format!("{}.jpeg", "x".repeat(300));
    let short = safe_name(&long);
    assert!(short.len() <= NAME_MAX && short.ends_with("x.jpeg"), "{short}");
}

#[test]
fn a_taken_name_gets_a_number() {
    let dir = crate::testsupport::TempDir::new("uploads-names");
    assert_eq!(free_path(dir.path(), "a.png"), dir.path().join("a.png"));
    std::fs::write(dir.path().join("a.png"), "x").unwrap();
    std::fs::write(dir.path().join("a-2.png"), "x").unwrap();
    assert_eq!(free_path(dir.path(), "a.png"), dir.path().join("a-3.png"));
    std::fs::write(dir.path().join("notes"), "x").unwrap();
    assert_eq!(free_path(dir.path(), "notes"), dir.path().join("notes-2"));
}
