use super::*;
use crate::testsupport::TempDir;

/// A data folder and a home where Claude and Codex are used, Antigravity
/// not.
fn places(name: &str) -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new(name);
    let data = dir.path().join("data");
    let home = dir.path().join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    (dir, data, home)
}

fn link_target(path: &Path) -> Option<PathBuf> {
    fs::symlink_metadata(path).ok().filter(|m| m.file_type().is_symlink())?;
    fs::read_link(path).ok()
}

#[test]
fn sync_writes_the_shipped_skills_and_links_them_for_each_agent_used_here() {
    let (_dir, data, home) = places("skills-sync");
    let report = sync(&data, &home).unwrap();
    assert_eq!(report.written, vec!["cepheus".to_string(), "cepheus-plugin".into()]);
    assert_eq!(report.problems, Vec::<String>::new());
    let store = store(&data);
    for skill in BUNDLED {
        for (file, text) in skill.files {
            assert_eq!(fs::read_to_string(store.join(skill.name).join(file)).unwrap(), *text);
        }
        assert_eq!(link_target(&home.join(".claude/skills").join(skill.name)), Some(store.join(skill.name)));
        assert_eq!(link_target(&home.join(".codex/skills").join(skill.name)), Some(store.join(skill.name)));
    }
    assert!(!home.join(".agents").exists(), "Antigravity is not used here");
    // Nothing to do the second time.
    let again = sync(&data, &home).unwrap();
    assert_eq!(again, SyncReport::default());
    assert_eq!(again.summary(), "The deck's skills are linked for every agent");
}

#[test]
fn a_skill_of_the_owners_own_with_the_same_name_is_left_alone() {
    let (_dir, data, home) = places("skills-own");
    let own = home.join(".claude/skills/cepheus");
    fs::create_dir_all(&own).unwrap();
    fs::write(own.join("SKILL.md"), "mine").unwrap();
    let elsewhere = home.join(".codex/skills/cepheus-plugin");
    fs::create_dir_all(home.join(".codex/skills")).unwrap();
    symlink(home.join("somewhere"), &elsewhere).unwrap();
    let report = sync(&data, &home).unwrap();
    assert_eq!(fs::read_to_string(own.join("SKILL.md")).unwrap(), "mine");
    assert_eq!(link_target(&elsewhere), Some(home.join("somewhere")));
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(report.preserved.len(), 2, "{:?}", report.preserved);
}

#[test]
fn links_to_skills_the_deck_no_longer_has_are_removed_and_no_others() {
    let (dir, _data, home) = places("skills-stale");
    let data = dir.path().join("share/cepheus");
    fs::create_dir_all(home.join(".gemini")).unwrap();
    sync(&data, &home).unwrap();
    let previous = dir.path().join("share/agent-deck/skills"); // legacy-name
    for (old, new) in [("agent-deck", "cepheus"), ("agent-deck-plugin", "cepheus-plugin")] {
        // legacy-name
        fs::create_dir_all(previous.join(old)).unwrap();
        fs::write(previous.join(old).join("SKILL.md"), "earlier bundled skill").unwrap();
        for &(_, rel, _) in AGENTS {
            symlink(previous.join(old), home.join(rel).join(old)).unwrap();
            assert_eq!(link_target(&home.join(rel).join(new)), Some(store(&data).join(new)));
        }
    }
    // A personal link or directory with an earlier name must survive too.
    let own_link = home.join(".codex/skills/agent-deck-plugin"); // legacy-name
    fs::remove_file(&own_link).unwrap();
    symlink(home.join("personal-skill"), &own_link).unwrap();
    let own_dir = home.join(".claude/skills/agent-deck-plugin"); // legacy-name
    fs::remove_file(&own_dir).unwrap();
    fs::create_dir_all(&own_dir).unwrap();
    fs::write(own_dir.join("SKILL.md"), "mine").unwrap();
    let skills = home.join(".claude/skills");
    symlink(store(&data).join("retired"), skills.join("retired")).unwrap();
    symlink(home.join("mine"), skills.join("mine")).unwrap();
    // A dangling replacement must not retire a working earlier skill.
    fs::remove_dir_all(store(&data).join("cepheus-plugin")).unwrap();
    let mut report = SyncReport::default();
    link_all(&store(&data), &home, &mut report);
    assert!(report.problems.is_empty(), "{report:?}");
    assert!(report.removed.contains(&"retired for claude".to_string()));
    assert!(link_target(&skills.join("retired")).is_none());
    assert_eq!(link_target(&skills.join("mine")), Some(home.join("mine")));
    for &(_, rel, _) in AGENTS {
        assert!(link_target(&home.join(rel).join("agent-deck")).is_none()); // legacy-name
    }
    let earlier_plugin = home.join(".agents/skills/agent-deck-plugin"); // legacy-name
    assert_eq!(link_target(&earlier_plugin), Some(previous.join("agent-deck-plugin"))); // legacy-name
                                                                                        // Once sync restores the replacement, its earlier alias can retire.
    sync(&data, &home).unwrap();
    assert!(link_target(&earlier_plugin).is_none());
    assert_eq!(link_target(&own_link), Some(home.join("personal-skill")));
    assert_eq!(fs::read_to_string(own_dir.join("SKILL.md")).unwrap(), "mine");
    assert_eq!(fs::read_to_string(previous.join("agent-deck/SKILL.md")).unwrap(), "earlier bundled skill");
    // legacy-name
}

#[test]
fn sync_retires_legacy_bundled_links_without_removing_stored_skills() {
    for layout in ["default", "xdg", "legacy"] {
        let (dir, _, home) = places("skills-rename");
        fs::create_dir_all(home.join(".gemini")).unwrap();
        let base = if layout == "xdg" { dir.path().join("xdg") } else { home.join(".local/share") };
        let legacy = base.join("agent-deck/skills"); // legacy-name
        let data = base.join(if layout == "legacy" { "agent-deck" } else { "cepheus" }); // legacy-name
        for (old, _) in [("agent-deck", "cepheus"), ("agent-deck-plugin", "cepheus-plugin")] {
            // legacy-name
            fs::create_dir_all(legacy.join(old)).unwrap();
            fs::write(legacy.join(old).join("SKILL.md"), "recoverable original").unwrap();
            for &(_, rel, _) in AGENTS {
                fs::create_dir_all(home.join(rel)).unwrap();
                symlink(legacy.join(old), home.join(rel).join(old)).unwrap();
            }
        }
        let report = sync(&data, &home).unwrap();
        assert!(report.problems.is_empty(), "{layout}: {report:?}");
        for (old, current) in [("agent-deck", "cepheus"), ("agent-deck-plugin", "cepheus-plugin")] {
            // legacy-name
            for &(_, rel, _) in AGENTS {
                assert!(fs::symlink_metadata(home.join(rel).join(old)).is_err(), "{layout}: {rel}/{old}");
                assert_eq!(link_target(&home.join(rel).join(current)), Some(store(&data).join(current)));
            }
            assert_eq!(fs::read_to_string(legacy.join(old).join("SKILL.md")).unwrap(), "recoverable original");
        }
        assert_eq!(sync(&data, &home).unwrap(), SyncReport::default());
    }
}

#[test]
fn legacy_retirement_preserves_owner_skills_foreign_links_and_explicit_installs() {
    for legacy_data in [false, true] {
        let (_dir, default_data, home) = places("skills-rename-own");
        let legacy = home.join(".local/share/agent-deck/skills"); // legacy-name
        let data = if legacy_data { legacy.parent().unwrap().to_path_buf() } else { default_data };
        let claude = home.join(".claude/skills");
        let codex = home.join(".codex/skills");
        fs::create_dir_all(claude.join("cepheus")).unwrap();
        fs::write(claude.join("cepheus/SKILL.md"), "owner replacement").unwrap();
        symlink(legacy.join("agent-deck"), claude.join("agent-deck")).unwrap(); // legacy-name
        fs::create_dir_all(codex.join("agent-deck-plugin")).unwrap(); // legacy-name
        fs::write(codex.join("agent-deck-plugin/SKILL.md"), "owner original").unwrap(); // legacy-name
        symlink(legacy.join("agent-deck-extra"), codex.join("agent-deck")).unwrap(); // legacy-name
        sync(&data, &home).unwrap();
        assert_eq!(link_target(&claude.join("agent-deck")), Some(legacy.join("agent-deck"))); // legacy-name
        assert_eq!(link_target(&codex.join("agent-deck")), Some(legacy.join("agent-deck-extra"))); // legacy-name
        assert_eq!(fs::read_to_string(codex.join("agent-deck-plugin/SKILL.md")).unwrap(), "owner original"); // legacy-name

        fs::remove_file(codex.join("agent-deck")).unwrap(); // legacy-name
        symlink(legacy.join("agent-deck"), codex.join("agent-deck")).unwrap(); // legacy-name
        let mut manifest = read_manifest(&store(&data));
        manifest.installed.insert("agent-deck".into(), Installed { source: "owner/custom".into(), agents: vec![] }); // legacy-name
        write_manifest(&store(&data), &manifest).unwrap();
        sync(&data, &home).unwrap();
        assert_eq!(link_target(&codex.join("agent-deck")), Some(legacy.join("agent-deck")));
        // legacy-name
    }
}

#[test]
fn legacy_retirement_checks_the_original_stores_installation_records() {
    for contents in
        [Some(r#"{"installed":{"agent-deck":{"source":"owner/custom","agents":[]}}}"#), Some("invalid manifest"), None]
    {
        // legacy-name
        let (dir, _, home) = places("skills-rename-records");
        let data = home.join(".local/share/cepheus");
        let legacy = home.join(".local/share/agent-deck/skills"); // legacy-name
        fs::create_dir_all(&legacy).unwrap();
        if let Some(contents) = contents {
            fs::write(legacy.join(MANIFEST), contents).unwrap();
        } else {
            // A manifest that cannot be read as a file proves no ownership.
            fs::create_dir(legacy.join(MANIFEST)).unwrap();
        }
        fs::create_dir_all(legacy.join("agent-deck")).unwrap(); // legacy-name
        fs::write(legacy.join("agent-deck/SKILL.md"), "owner original").unwrap(); // legacy-name
        fs::create_dir_all(home.join(".codex/skills")).unwrap();
        let alias = home.join(".codex/skills/agent-deck"); // legacy-name
        symlink(legacy.join("agent-deck"), &alias).unwrap(); // legacy-name
        let report = sync(&data, &home).unwrap();
        assert_eq!(link_target(&alias), Some(legacy.join("agent-deck"))); // legacy-name
        assert!(!report.preserved.is_empty());

        // Installing and removing another skill uses the same linking path.
        let src = dir.path().join("review");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("SKILL.md"), "review").unwrap();
        install_dir(&data, &home, &src, "review", &[], "owner/review").unwrap();
        assert_eq!(link_target(&alias), Some(legacy.join("agent-deck"))); // legacy-name
        remove(&data, &home, "review").unwrap();
        assert_eq!(link_target(&alias), Some(legacy.join("agent-deck"))); // legacy-name
        assert_eq!(fs::read_to_string(alias.join("SKILL.md")).unwrap(), "owner original");
    }
}

#[test]
fn an_installed_skill_is_copied_without_links_or_git_and_linked_for_its_agents() {
    let (dir, data, home) = places("skills-install");
    let src = dir.path().join("fetched/skill");
    fs::create_dir_all(src.join("docs")).unwrap();
    fs::create_dir_all(src.join(".git")).unwrap();
    fs::write(src.join("SKILL.md"), "---\nname: review\ndescription: \"Review a diff\"\n---\nbody").unwrap();
    fs::write(src.join("docs/how.md"), "how").unwrap();
    fs::write(src.join(".git/config"), "[core]").unwrap();
    let secret = dir.path().join("outside-secret");
    fs::write(&secret, "not for the skill").unwrap();
    symlink(&secret, src.join("leak.txt")).unwrap();
    symlink(dir.path(), src.join("docs/up")).unwrap();

    let report = install_dir(&data, &home, &src, "review", &["claude".to_string()], "owner/review").unwrap();
    let copied = store(&data).join("review");
    assert_eq!(fs::read_to_string(copied.join("docs/how.md")).unwrap(), "how");
    for left_out in [".git", "leak.txt", "docs/up"] {
        assert!(fs::symlink_metadata(copied.join(left_out)).is_err(), "{left_out} was copied");
    }
    assert!(report.linked.contains(&"review for claude".to_string()), "{report:?}");
    assert_eq!(link_target(&home.join(".claude/skills/review")), Some(copied.clone()));
    assert!(link_target(&home.join(".codex/skills/review")).is_none(), "only for Claude");
    // Before the first sync the shipped skills are not in the store yet:
    // a link to one would lead nowhere.
    assert!(link_target(&home.join(".codex/skills/cepheus")).is_none());
    let manifest = read_manifest(&store(&data));
    assert_eq!(
        manifest.installed["review"],
        Installed { source: "owner/review".into(), agents: vec!["claude".into()] }
    );

    let listed = list(&data, &home);
    let review = listed.iter().find(|s| s.name == "review").unwrap();
    assert_eq!((review.description.as_str(), review.source.as_str()), ("Review a diff", "owner/review"));
    assert_eq!(review.linked, vec!["claude".to_string()]);

    // Removing it takes its links, folder and record.
    let removed = remove(&data, &home, "review").unwrap();
    assert_eq!(removed.removed, vec!["review for claude".to_string()]);
    assert!(!copied.exists() && read_manifest(&store(&data)).installed.is_empty());
    assert!(remove(&data, &home, "review").is_err());
    assert!(remove(&data, &home, "cepheus").is_err(), "the deck's own skills stay");

    // A user can explicitly install and remove a skill whose name happens to
    // match one of the retired bundled aliases.
    for &(name, _) in LEGACY_BUNDLED {
        install_dir(&data, &home, &src, name, &["claude".into()], "owner/custom").unwrap();
        assert_eq!(link_target(&home.join(".claude/skills").join(name)), Some(store(&data).join(name)));
        let report = remove(&data, &home, name).unwrap();
        assert!(report.problems.is_empty(), "{report:?}");
        assert!(fs::symlink_metadata(home.join(".claude/skills").join(name)).is_err());
    }
}

#[test]
fn an_install_refuses_strange_names_the_decks_own_and_folders_without_a_skill() {
    let (dir, data, home) = places("skills-refuse");
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("SKILL.md"), "x").unwrap();
    for name in ["../escape", "a/b", ".hidden", "-rf", "", "cepheus", &"n".repeat(65)] {
        assert!(install_dir(&data, &home, &src, name, &[], "x").is_err(), "{name:?} was taken");
    }
    assert!(!dir.path().join("escape").exists() && !data.join("escape").exists());
    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    assert!(install_dir(&data, &home, &empty, "empty", &[], "x").is_err());
    assert!(valid_name("my_skill-2.0") && !valid_name("my skill"));
}

#[test]
fn an_install_larger_than_the_budget_leaves_nothing_behind() {
    let (dir, data, home) = places("skills-budget");
    let src = dir.path().join("big");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("SKILL.md"), "x").unwrap();
    for i in 0..MAX_FILES {
        fs::write(src.join(format!("f{i}")), "").unwrap();
    }
    let err = install_dir(&data, &home, &src, "big", &[], "x").unwrap_err();
    assert!(err.to_string().contains("larger than the deck copies"), "{err}");
    let store = store(&data);
    assert!(!store.join("big").exists() && !store.join(".big.new").exists());
    assert!(read_manifest(&store).installed.is_empty());
}

#[test]
fn a_skills_description_comes_from_its_front_matter() {
    assert_eq!(description("---\nname: x\ndescription: Does things\n---\n# X"), Some("Does things".into()));
    assert_eq!(description("---\ndescription: \"Quoted\"\n---\n"), Some("Quoted".into()));
    assert_eq!(description("# No front matter\ndescription: no"), None);
    assert_eq!(description("---\nname: x\n---\n"), None);
    for skill in BUNDLED {
        let md = skill.files.iter().find(|(f, _)| *f == "SKILL.md").unwrap().1;
        assert!(description(md).is_some_and(|d| d.len() > 40), "{} needs a description", skill.name);
    }
}
