use super::*;
use crate::testsupport::TempDir;

struct Fixture {
    dir: TempDir,
    controller: Controller,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new("startup");
        let script = r###"#!/usr/bin/python3
import json
import sys
from pathlib import Path
root = Path(__file__).parent
args = sys.argv[1:]
if not args:
    raise SystemExit(0)
with (root / 'events').open('a') as output:
    output.write(json.dumps([Path(__file__).name, args]) + '\n')
action = next(arg for arg in args if not arg.startswith('--'))
if Path(__file__).name == 'loginctl':
    if action == 'show-user':
        print('yes' if (root / 'linger').exists() else 'no')
    elif action == 'enable-linger':
        (root / 'linger').touch()
    else:
        raise SystemExit(99)
elif action == 'show':
    name = args[args.index(action) + 1]
    unit = root / 'units' / name
    print('LoadState=' + ('loaded' if unit.exists() else 'not-found'))
    print('FragmentPath=' + (str(unit) if unit.exists() else ''))
    print('ActiveState=' + ('active' if (root / 'running').exists() else 'inactive'))
elif action == 'is-enabled':
    print('enabled')
elif action in ('daemon-reload', 'reenable', 'disable'):
    pass
elif action == 'start' and '--no-block' in args:
    (root / 'start-requested').touch()
else:
    # A preservation regression is contained here, never sent to real systemd.
    raise SystemExit(99)
"###;
        let systemctl = dir.executable("systemctl", script);
        let loginctl = dir.executable("loginctl", script);
        let program = dir.executable(".local/bin/cepheus", "#!/bin/sh\nexit 0\n");
        let controller = Controller {
            home: dir.path().into(),
            config: dir.path().join("config.json"),
            state: dir.path().join("state"),
            units: dir.path().join("units"),
            program,
            start_dir: dir.path().join("repository with spaces"),
            path_env: "/custom/bin:/usr/bin:/bin".into(),
            provider_env: vec![],
            systemctl,
            loginctl,
            // SAFETY: geteuid has no preconditions.
            uid: unsafe { libc::geteuid() }.to_string(),
        };
        Self { dir, controller }
    }

    fn execute(&self, args: &[&str]) -> Result<String> {
        self.controller.execute(&args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>(), Settings::default())
    }

    fn events(&self) -> Vec<(String, Vec<String>)> {
        fs::read_to_string(self.dir.path().join("events"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[test]
fn boot_login_and_disable_preserve_running_sessions_and_other_configuration() {
    let fixture = Fixture::new();
    fixture.dir.write("running", "healthy detached session");
    fixture.dir.write("config.json", r#"{"notifications":false,"custom":{"nested":[1,"kept"]}}"#);
    fixture.execute(&["enable", "--boot"]).unwrap();
    let config: serde_json::Value = serde_json::from_slice(&fs::read(&fixture.controller.config).unwrap()).unwrap();
    assert_eq!(config["custom"]["nested"], serde_json::json!([1, "kept"]));
    assert_eq!(config["notifications"], false);
    assert_eq!(config["startup"]["mode"], "boot");
    assert!(fixture.dir.path().join("linger").exists());
    let unit = fixture.controller.units.join(setup::UNIT);
    let boot = fs::read_to_string(&unit).unwrap();
    assert!(boot.contains("WantedBy=default.target"));
    fixture.execute(&["enable", "--login"]).unwrap();
    assert!(fs::read_to_string(&unit).unwrap().contains("WantedBy=graphical-session.target"));
    fixture.execute(&["disable"]).unwrap();
    assert!(fixture.dir.path().join("linger").exists(), "disabling startup must retain existing lingering");
    assert_eq!(fs::read_to_string(fixture.dir.path().join("running")).unwrap(), "healthy detached session");
    let events = fixture.events();
    assert!(events.iter().all(|(_, args)| !args
        .iter()
        .any(|arg| { matches!(arg.as_str(), "--now" | "start" | "stop" | "restart" | "disable-linger") })));
    assert!(events.iter().any(|(_, args)| args.iter().any(|arg| arg == "disable")));
}

#[test]
fn legacy_unit_is_reused_and_competing_units_are_held() {
    let mut fixture = Fixture::new();
    let legacy = fixture.dir.write(
        "units/agent-deck.service", // legacy-name
        format!("[Service]\nExecStart={} service\n", fixture.dir.path().join(".local/bin/agent-deck").display()), // legacy-name
    );
    fixture.execute(&["enable"]).unwrap();
    assert!(!fixture.controller.units.join(setup::UNIT).exists());
    let alias = fixture.dir.path().join("unit-path-alias");
    std::os::unix::fs::symlink(&fixture.controller.units, &alias).unwrap();
    fixture.controller.units = alias;
    fixture.execute(&["enable"]).unwrap();
    let kept = fs::read_to_string(&legacy).unwrap();
    fixture.dir.write("units/cepheus.service", &kept);
    let config_before = fs::read(&fixture.controller.config).unwrap();
    let error = fixture.execute(&["enable"]).unwrap_err().to_string();
    assert!(error.contains("competing services"));
    assert_eq!(fs::read(&fixture.controller.config).unwrap(), config_before);
    assert_eq!(fs::read_to_string(&legacy).unwrap(), kept);
}

#[test]
fn incompatible_units_and_invalid_configuration_are_not_overwritten() {
    let fixture = Fixture::new();
    let foreign = fixture.dir.write("units/cepheus.service", "[Service]\nExecStart=/unrelated/program service\n");
    assert!(fixture.execute(&["enable"]).is_err());
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "[Service]\nExecStart=/unrelated/program service\n");
    assert!(!fixture.dir.path().join("linger").exists());

    let fixture = Fixture::new();
    fixture.dir.write("config.json", "{broken");
    assert!(fixture.execute(&["enable"]).is_err());
    assert!(!fixture.controller.units.exists());
    assert_eq!(fs::read_to_string(&fixture.controller.config).unwrap(), "{broken");
    assert!(fixture.events().is_empty());
}

#[test]
fn generated_unit_preserves_literal_paths_and_configured_timings() {
    let mut fixture = Fixture::new();
    fixture.controller.program = fixture.dir.path().join("literal % $ with spaces/cepheus");
    fixture.controller.home = fixture.dir.path().join("literal % $ \" and back\\slash with spaces");
    fixture.controller.state = fixture.controller.home.join("state");
    let settings = Settings { mode: Mode::Boot, delay_secs: 12, restart_secs: 9, stop_timeout_secs: 44 };
    let unit = fixture.controller.unit_text(&settings).unwrap();
    assert!(unit.contains("literal %% $$ with spaces/cepheus\" service"));
    let scalar_home = fixture.controller.home.to_string_lossy().replace('%', "%%");
    assert!(unit.contains(&format!("WorkingDirectory={scalar_home}\n")));
    assert!(unit.contains(&format!("StandardOutput=append:{scalar_home}/state/service.log\n")));
    assert!(unit.contains(&format!("StandardError=append:{scalar_home}/state/service.log\n")));
    assert!(unit.contains("ExecStartPre=/usr/bin/sleep 12\n"));
    assert!(unit.contains("RestartSec=9\nTimeoutStopSec=44\n"));
    fixture.controller.path_env = "/usr/bin\nExecStart=/unrelated".into();
    assert!(fixture.controller.unit_text(&settings).is_err());
}

#[test]
fn every_service_unit_keeps_systemd_from_killing_the_deck_and_its_sessions() {
    let fixture = Fixture::new();
    let peer = setup::unit_text(Path::new("/home/owner/.local/bin/cepheus"), "/usr/bin", Path::new("/state/log"));
    for mode in [Mode::Boot, Mode::Login] {
        let settings = Settings { mode, ..Settings::default() };
        for unit in [fixture.controller.unit_text(&settings).unwrap(), peer.clone()] {
            let service = unit.split("[Service]").nth(1).unwrap().split("[Install]").next().unwrap();
            for line in
                ["Slice=session.slice", "ManagedOOMPreference=omit", "OOMPolicy=continue", "OOMScoreAdjust=-1000"]
            {
                assert_eq!(service.lines().filter(|l| *l == line).count(), 1, "{line} missing in:\n{unit}");
            }
            assert!(!unit.contains("MemoryMax") && !unit.contains("MemoryHigh"), "no memory limit is wanted");
        }
    }
}

#[test]
fn opening_a_client_uses_the_configured_service_without_bypassing_its_delay() {
    let fixture = Fixture::new();
    fixture.execute(&["enable", "--boot"]).unwrap();
    let settings = Settings { mode: Mode::Boot, delay_secs: 12, ..Settings::default() };
    let wait = fixture.controller.request_start(&settings).unwrap().unwrap();
    assert_eq!(wait, Duration::from_secs(20));
    assert!(fixture.dir.path().join("start-requested").exists());
    assert!(fixture.events().iter().any(|(_, args)| args.iter().any(|arg| arg == "start")
        && args.iter().any(|arg| arg == "--no-block")
        && args.iter().any(|arg| arg == setup::UNIT)));
}

#[test]
fn generated_unit_keeps_relative_configuration_paths_at_the_enabling_directory() {
    let mut fixture = Fixture::new();
    fixture.controller.config = "relative-config.json".into();
    fixture.controller.state = "relative-state".into();
    fixture.controller.start_dir = "relative-workspace".into();
    let cwd = std::env::current_dir().unwrap();
    let unit = fixture.controller.unit_text(&Settings::default()).unwrap();
    for (key, path) in [("CEPHEUS_CONFIG", "relative-config.json"), ("CEPHEUS_STATE_DIR", "relative-state")] {
        assert!(unit.contains(&format!("Environment=\"{key}={}\"", cwd.join(path).display())));
    }
    assert!(unit.contains(&format!(
        "Environment=\"CEPHEUS_START_DIR={}\"",
        fixture.controller.home.join("relative-workspace").display()
    )));
    assert!(unit.contains(&format!("StandardOutput=append:{}\n", cwd.join("relative-state/service.log").display())));
}
