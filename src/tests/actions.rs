use super::*;

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| w.to_string()).collect()
}

fn ctx() -> Ctx {
    Ctx {
        hosts: strings(&["fedora", "ubuntu"]),
        user: vec![("tasks".into(), strings(&["ready", "active", "all"])), ("fleet".into(), Vec::new())],
        options: strings(&["custom", "sidebar"]),
        sessions: 3,
    }
}

#[test]
fn words_split_on_spaces_and_keep_quoted_text_together() {
    assert_eq!(words("  new  codex fedora ").unwrap(), strings(&["new", "codex", "fedora"]));
    assert_eq!(
        words(r#"broadcast "run the tests" 'and then' stop"#).unwrap(),
        strings(&["broadcast", "run the tests", "and then", "stop"])
    );
    // A backslash keeps the next character, inside double quotes too; single
    // quotes keep everything.
    assert_eq!(
        words(r#"send 2 a\ b "say \"hi\"" 'c\d'"#).unwrap(),
        strings(&["send", "2", "a b", "say \"hi\"", "c\\d"])
    );
    // An empty quoted word is still a word.
    assert_eq!(words(r#"rename """#).unwrap(), strings(&["rename", ""]));
    assert_eq!(words("").unwrap(), Vec::<String>::new());
}

#[test]
fn words_refuse_an_unclosed_quote_or_a_trailing_backslash() {
    assert!(words("send 1 \"never closed").is_err());
    assert!(words("send 1 'never closed").is_err());
    assert!(words("send 1 trailing\\").is_err());
}

#[test]
fn take_value_reads_both_spellings_and_stops_at_the_double_dash() {
    let mut w = strings(&["--cwd", "/a b", "fedora", "--name=x"]);
    assert_eq!(take_value(&mut w, "--cwd").unwrap(), Some("/a b".into()));
    assert_eq!(take_value(&mut w, "--name").unwrap(), Some("x".into()));
    assert_eq!(w, strings(&["fedora"]));
    // After `--` the words are the command's own.
    let mut w = strings(&["fedora", "--", "ls", "--cwd", "/x"]);
    assert_eq!(take_value(&mut w, "--cwd").unwrap(), None);
    assert_eq!(w, strings(&["fedora", "--", "ls", "--cwd", "/x"]));
    // A flag without its value is an error, not the next flag taken as one.
    assert!(take_value(&mut strings(&["--cwd"]), "--cwd").is_err());
    assert!(take_value(&mut strings(&["--cwd", "--", "ls"]), "--cwd").is_err());
}

#[test]
fn take_switch_only_before_the_double_dash() {
    let mut w = strings(&["x", "--yes", "--", "--yes"]);
    assert!(take_switch(&mut w, "--yes"));
    assert_eq!(w, strings(&["x", "--", "--yes"]));
    assert!(!take_switch(&mut w, "--yes"));
}

#[test]
fn every_name_and_alias_finds_its_own_command() {
    let mut seen = std::collections::HashSet::new();
    for spec in COMMANDS {
        for name in std::iter::once(&spec.name).chain(spec.aliases) {
            assert!(seen.insert(*name), "{name} names two commands");
            assert_eq!(find(name).map(|s| s.name), Some(spec.name));
        }
    }
    assert!(is_builtin("q!") && is_builtin("wq") && is_builtin("br") && is_builtin("add"));
    assert!(!is_builtin("tasks") && !is_builtin("fleet"));
}

#[test]
fn completing_the_first_word_offers_built_in_and_script_commands() {
    let (start, items) = complete("", &ctx());
    assert_eq!(start, 0);
    assert!(items.contains(&"new".to_string()) && items.contains(&"tasks".to_string()));
    let mut sorted = items.clone();
    sorted.sort();
    assert_eq!(items, sorted);
    assert_eq!(complete("br", &ctx()), (0, strings(&["branches", "broadcast"])));
    assert_eq!(complete("ta", &ctx()), (0, strings(&["tasks"])));
}

#[test]
fn completing_new_offers_programs_then_hosts_past_any_flags() {
    let ctx = ctx();
    assert_eq!(complete("new ", &ctx), (4, strings(&["agy", "claude", "codex", "grok", "opencode"])));
    assert_eq!(complete("new c", &ctx).1, strings(&["claude", "codex"]));
    assert_eq!(complete("new codex ", &ctx), (10, strings(&["fedora", "here", "ubuntu"])));
    assert_eq!(complete("new codex --cwd /srv/x ", &ctx).1, strings(&["fedora", "here", "ubuntu"]));
    assert_eq!(complete("new --name x codex f", &ctx).1, strings(&["fedora"]));
    // A flag's value is the owner's to type.
    assert_eq!(complete("new codex --cwd ", &ctx).1, Vec::<String>::new());
    assert_eq!(complete("new codex fedora ", &ctx).1, Vec::<String>::new());
    assert_eq!(complete("new codex --", &ctx).1, strings(&["--cwd", "--model", "--name"]));
    // OPS-S11301: a typed separator of several bytes (no-break, em and narrow
    // no-break spaces) panicked the view; the word starts after all of it.
    for space in ['\u{a0}', '\u{2003}', '\u{202f}'] {
        let line = format!("new{space}c");
        assert_eq!(complete(&line, &ctx), (3 + space.len_utf8(), strings(&["claude", "codex"])), "{space:?}");
    }
}

#[test]
fn direct_program_commands_complete_hosts_and_start_options() {
    let ctx = ctx();
    for name in ["claude", "codex", "agy", "grok", "opencode"] {
        assert!(complete(name, &ctx).1.contains(&name.to_string()), "{name}");
        assert_eq!(complete(&format!("{name} "), &ctx).1, strings(&["fedora", "here", "ubuntu"]));
        assert_eq!(complete(&format!("{name} --"), &ctx).1, strings(&["--cwd", "--model", "--name"]));
        assert_eq!(complete(&format!("{name} --cwd /srv/x f"), &ctx).1, strings(&["fedora"]));
        assert!(complete(&format!("{name} --model "), &ctx).1.is_empty());
    }
}

#[test]
fn completing_arguments_by_kind() {
    let ctx = ctx();
    assert_eq!(complete("term ", &ctx).1, strings(&["fedora", "here", "ubuntu"]));
    assert_eq!(complete("term -- ", &ctx).1, Vec::<String>::new());
    assert_eq!(complete("dev o", &ctx).1, strings(&["off", "on"]));
    assert_eq!(complete("go ", &ctx).1, strings(&["1", "2", "3"]));
    assert_eq!(complete("set ", &ctx).1, strings(&["custom", "sidebar"]));
    assert_eq!(complete("install owner/repo --host ", &ctx).1, strings(&["all", "fedora", "here", "ubuntu"]));
    assert_eq!(
        complete("install owner/repo --agents c", &ctx).1,
        strings(&["claude", "claude,codex", "claude,codex,agy", "codex"])
    );
    assert_eq!(complete("install x --", &ctx).1, strings(&["--agents", "--dry-run", "--host", "--yes"]));
    assert_eq!(complete("tasks r", &ctx).1, strings(&["ready"]));
    assert_eq!(complete("tasks ready ", &ctx).1, Vec::<String>::new());
    assert_eq!(complete("unknown ", &ctx).1, Vec::<String>::new());
}

#[test]
fn common_prefix_of_candidates() {
    assert_eq!(common_prefix(&strings(&["claude", "claude,codex"])), "claude");
    assert_eq!(common_prefix(&strings(&["branches", "broadcast"])), "br");
    assert_eq!(common_prefix(&strings(&["été", "étage"])), "ét");
    assert_eq!(common_prefix(&[]), "");
}
