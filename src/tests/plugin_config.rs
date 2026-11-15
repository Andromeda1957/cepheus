use super::*;
use crate::testsupport::TempDir;

fn registered(svc: &Service) -> bool {
    svc.service_plugin_commands().iter().any(|p| p.id == "hello" && p.commands.iter().any(|c| c.name == "ping"))
}

/// Look twice a second apart, as the service does: a changed file is taken
/// once it has stayed the same between two looks.
fn looks(svc: &mut Service, at: &mut Instant) {
    for _ in 0..2 {
        *at += Duration::from_secs(1);
        svc.upkeep_plugin_config(*at);
    }
}

#[test]
fn a_plugin_turned_off_or_on_in_the_config_file_follows_without_a_restart() {
    let dir = TempDir::new("plugin-config");
    let config = dir.path().join("config/config.json");
    dir.write(
        "config/service/hello.lua",
        r#"require("deck").service.command("hello", "ping", function() return {} end)"#,
    );
    fs::write(&config, r#"{"plugins":{}}"#).unwrap();
    let mut opts = super::tests::options(&dir, "true");
    opts.config_file = Some(config.clone());
    let (tx, _rx) = mpsc::channel();
    let mut svc = Service::new(opts, tx);
    svc.load_service_plugins();
    assert!(registered(&svc));
    let mut at = Instant::now();
    looks(&mut svc, &mut at);
    assert!(registered(&svc), "an unchanged file reloads nothing");

    fs::write(&config, r#"{"plugins":{"hello":false}}"#).unwrap();
    at += Duration::from_secs(1);
    svc.upkeep_plugin_config(at);
    assert!(registered(&svc), "a file is taken only once it stays the same");
    at += Duration::from_secs(1);
    svc.upkeep_plugin_config(at);
    assert!(!registered(&svc), "turned off in the file, unloaded with no restart");
    assert_eq!(svc.opts.config.plugins.get("hello"), Some(&false));

    fs::write(&config, r#"{"plugins":{"hello":false}"#).unwrap();
    looks(&mut svc, &mut at);
    assert!(!registered(&svc), "a file that does not parse keeps the running settings");
    assert!(svc.errors.borrow().iter().any(|e| e.text.starts_with("Plugin settings unchanged:")));

    fs::write(&config, r#"{"plugins":{"hello":true}}"#).unwrap();
    looks(&mut svc, &mut at);
    assert!(registered(&svc), "turned on again, loaded again");
}
