use super::*;
use crate::testsupport::TempDir;

#[test]
fn background_skill_sync_preserves_owner_files_without_announcing_success_or_losing_failures() {
    let dir = TempDir::new("skill-sync-severity");
    let home = dir.path().join("home");
    let data = dir.path().join("data");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let first = crate::skills::sync(&data, &home).unwrap();
    assert!(!first.written.is_empty(), "the first startup really updates shipped skills");
    assert!(skills_notice(None, Ok(first)).is_none(), "a bundled update stays in the service log");
    let own = home.join(".claude/skills/cepheus/SKILL.md");
    fs::remove_file(own.parent().unwrap()).unwrap();
    fs::create_dir_all(own.parent().unwrap()).unwrap();
    fs::write(&own, "owner skill").unwrap();
    let report = crate::skills::sync(&data, &home).unwrap();
    assert_eq!(fs::read_to_string(&own).unwrap(), "owner skill");
    assert!(skills_notice(None, Ok(report.clone())).is_none(), "background success stays in the service log");

    let (tx, _) = std::sync::mpsc::channel();
    let mut service = Service::new(crate::service::tests::options(&dir, "true"), tx);
    service.log_error("previous genuine failure");
    let requested = skills_notice(Some(7), Ok(report)).expect("an explicit sync receives its result");
    assert!(matches!(&requested, Ev::Notice { client: Some(7), error: false, .. }));
    service.handle(requested);
    assert_eq!(service.errors.borrow().len(), 1, "requested success leaves prior history intact");

    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(home.join(".codex/skills"), "owner file blocking the skills directory").unwrap();
    let failed = crate::skills::sync(&data, &home).unwrap();
    assert!(!failed.problems.is_empty());
    let error = skills_notice(None, Ok(failed)).expect("a real filesystem failure is reported");
    assert!(matches!(&error, Ev::Notice { error: true, .. }));
    service.handle(error);
    let errors = service.errors.borrow();
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0].text, "previous genuine failure");
    assert!(!errors[1].text.contains("kept your own"), "routine preservation is excluded from failure details");
}
