//! The bundled `lua/unmerged_branches.lua`: the number beside the
//! repository title.
use super::*;
use crate::protocol::{BranchInfo, HostInfo, HostStats, ServerMsg};
use serde_json::{json, Value};

fn branch(name: &str, path: &str, ahead: u32) -> Value {
    json!({"branch": name, "path": path, "ahead": ahead})
}

fn main_checkout() -> Value {
    json!({"branch": "master", "path": "/w/main", "main": true})
}

/// The number the title row shows, if any.
fn count(sections: &[Section]) -> Option<String> {
    let section = sections.iter().find(|s| s.id == "unmerged-branches")?;
    Some(section.lines.iter().flatten().map(|segment| segment.text.as_str()).collect())
}

// Owner, 2026-11-08: "that 60 number is supposed to be branches unmerged and
// not detatched" and "unmerged branches should be a lua plugin".
#[test]
fn the_title_counts_each_unmerged_branch_checked_out_on_any_host_once() {
    let dir = TempDir::new("lua-unmerged");
    let (mut e, _rx) = engine(&dir);
    e.set_state(json!({
        "branches": [
            main_checkout(),
            branch("agent/a", "/w/a", 3),
            branch("agent/merged", "/w/merged", 0),
            branch("agent/no-worktree", "", 9),
        ],
        "hosts": [
            {"name":"fedora","stats":{"branches":[branch("agent/a", "/f/a", 3), branch("agent/b", "/f/b", 1)]}},
            {"name":"offline"},
        ],
    }));
    e.load();
    assert_eq!(e.take_errors(), Vec::<String>::new());
    let sections = e.sections();
    let section = sections.iter().find(|s| s.id == "unmerged-branches").expect("the count");
    assert_eq!(section.header, Some(HeaderPlace::Inline), "right after the branch on the repository title row");
    assert_eq!(section.lines, vec![vec![seg("2", "strong")]], "agent/a once, agent/b");
    // A change is counted at once: everything merged, the number goes away.
    e.set_state(json!({"branches":[main_checkout(), branch("agent/a", "/w/a", 0)],"hosts":[]}));
    e.emit_json("state", &Value::Null);
    assert_eq!(count(&e.sections()), None);
    // Each host's statistics tick is a change: those right after it are
    // counted together once a few seconds have passed.
    e.set_state(json!({"branches":[main_checkout(), branch("agent/c", "/w/c", 1)],"hosts":[]}));
    e.emit_json("state", &Value::Null);
    assert_eq!(count(&e.sections()), None, "not yet counted again");
    assert!(e.tick(Instant::now() + Duration::from_secs(6)));
    assert_eq!(count(&e.sections()).as_deref(), Some("1"));
}

// The count flashed 15 then 51 (owner report relayed 2026-11-09): it was counted
// before every host had reported its branches.
#[test]
fn the_count_waits_for_every_hosts_branches_and_keeps_its_number_while_one_reconnects() {
    let dir = TempDir::new("lua-unmerged-partial");
    let (mut e, _rx) = engine(&dir);
    let state = |fedora: Value| json!({"branches":[main_checkout(), branch("agent/a", "/w/a", 3)],"hosts":[fedora]});
    let connecting = json!({"name":"fedora","online":false,"reason":"connecting"});
    e.set_state(state(connecting.clone()));
    e.load();
    assert_eq!(count(&e.sections()), None, "connecting: its branches are not known yet");
    e.set_state(state(json!({"name":"fedora","online":true})));
    e.emit_json("state", &Value::Null);
    assert_eq!(count(&e.sections()), None, "connected but not reported yet");
    e.set_state(state(json!({"name":"fedora","online":true,"stats":{"branches":[branch("agent/b", "/f/b", 1)]}})));
    e.emit_json("state", &Value::Null);
    assert!(e.tick(Instant::now() + Duration::from_secs(6)));
    assert_eq!(count(&e.sections()).as_deref(), Some("2"));
    // The service updated and the host reconnects: the number stays.
    e.set_state(state(connecting));
    e.emit_json("state", &Value::Null);
    assert!(e.tick(Instant::now() + Duration::from_secs(12)));
    assert_eq!(count(&e.sections()).as_deref(), Some("2"), "never a count without fedora's branches");
    // A host known to be down counts with what it last reported, if anything.
    e.set_state(state(json!({"name":"fedora","online":false,"reason":"connection refused"})));
    e.emit_json("state", &Value::Null);
    assert!(e.tick(Instant::now() + Duration::from_secs(18)));
    assert_eq!(count(&e.sections()).as_deref(), Some("1"));
}

// The deck told its scripts of each new state but not of new hosts, so the
// count lacked the hosts until the next state and the throttle after it.
#[test]
fn the_count_follows_the_hosts_when_they_change() {
    let dir = TempDir::new("lua-unmerged-hosts");
    std::fs::create_dir_all(dir.path().join("start")).unwrap();
    let (mut app, _rx) = crate::tui::tests::app_with(Vec::new(), Vec::new());
    let local = |name: &str, ahead: u32, main: bool| BranchInfo {
        branch: name.into(),
        path: format!("/w/{name}"),
        ahead: Some(ahead),
        main,
        ..BranchInfo::default()
    };
    let mut state = app.state.clone();
    state.start_dir = dir.path().join("start").to_string_lossy().into_owned();
    state.branches = vec![local("master", 0, true), local("agent/a", 3, false)];
    crate::tui::tests::server(&mut app, ServerMsg::State(state));
    app.load_lua(
        Places { config_dir: dir.path().join("config"), start_dir: dir.path().join("start") },
        Arc::new(|_| {}),
    );
    assert_eq!(count(&app.lua_sections).as_deref(), Some("1"));
    let fedora = HostInfo {
        name: "fedora".into(),
        label: "Fedora".into(),
        online: true,
        stats: Some(HostStats { branches: vec![local("agent/b", 1, false)], ..HostStats::default() }),
        ..HostInfo::default()
    };
    crate::tui::tests::server(&mut app, ServerMsg::Hosts { hosts: vec![fedora] });
    assert_eq!(count(&app.lua_sections).as_deref(), Some("2"));
}
